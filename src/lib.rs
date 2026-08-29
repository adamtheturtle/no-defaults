use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::comparable::{ComparableLiteral, ComparableNumber};
use ruff_python_ast::helpers::Truthiness;
use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::visitor::{walk_except_handler, walk_expr, walk_pattern, walk_stmt, Visitor};
use ruff_python_ast::{self as ast, Expr, Pattern, Stmt};
use ruff_python_parser::{parse_expression, parse_module};
use ruff_text_size::{Ranged, TextRange, TextSize};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use unicode_width::UnicodeWidthChar as _;

#[derive(Debug, Parser)]
#[command(version, about = "Forbid defaults in Python functions and dataclasses")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap stores independent command-line switches as booleans"
)]
struct Cli {
    /// Python files or directories to check.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Only enforce private (underscore-prefixed) functions, classes, and fields.
    #[arg(long)]
    private_only: bool,

    /// In private-only mode, treat names an `__init__.py` re-exports as public.
    #[arg(long)]
    respect_reexports: bool,

    /// Remove detected defaults automatically.
    #[arg(long)]
    fix: bool,

    /// Preview fixes as a unified diff without writing files.
    #[arg(long, conflicts_with = "fix")]
    diff: bool,

    /// Diagnostic output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Full)]
    output_format: OutputFormat,

    /// Show the effective settings for each supplied file and exit.
    #[arg(long, conflicts_with_all = ["fix", "diff"])]
    show_settings: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Full,
    Concise,
    Json,
    Github,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Enforcement {
    All,
    Private,
    None,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default, alias = "private-only")]
    private_only: bool,

    #[serde(default, alias = "respect-reexports")]
    respect_reexports: bool,

    #[serde(default, alias = "per-file-enforcement")]
    per_file_enforcement: BTreeMap<String, Enforcement>,

    /// Classes whose subclasses carry fields, the way `@dataclass` marks a
    /// class that does. Listing one here is what makes `class Job(BaseModel)`
    /// have its annotated assignments checked.
    #[serde(default = "default_field_base_classes", alias = "field-base-classes")]
    field_base_classes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            private_only: false,
            respect_reexports: false,
            per_file_enforcement: BTreeMap::new(),
            field_base_classes: default_field_base_classes(),
        }
    }
}

/// Pydantic models are the common way to carry fields without `@dataclass`, and
/// a run that passed over them silently would say a codebase full of defaults
/// has none.
fn default_field_base_classes() -> Vec<String> {
    vec!["pydantic.BaseModel".to_owned()]
}

#[derive(Clone)]
struct LoadedConfig {
    root: PathBuf,
    config: Config,
    /// Derived from `config.field_base_classes` once per configuration file
    /// rather than once per checked file.
    field_bases: Arc<FieldBases>,
    /// Compiled from `config.per_file_enforcement` once per configuration file
    /// for the same reason: a long table would otherwise have every one of its
    /// globs recompiled for every file in the project.
    overrides: Arc<Vec<PerFileEnforcement>>,
}

/// The base classes that make a class carry fields.
///
/// A base is matched by the last segment of its name, as a decorator is, so
/// `pydantic.BaseModel` in configuration recognises `class Job(BaseModel)` and
/// `class Job(pydantic.BaseModel)` alike. Resolving the module a base really
/// came from would need the import graph, and the name alone is what the
/// decorator check has always gone by.
#[derive(Clone, Debug, Default)]
struct FieldBases {
    /// As configured, so `--show-settings` can report it back as written.
    configured: Vec<String>,
    /// The last segment of each, which is what a class header is matched
    /// against.
    names: BTreeSet<String>,
}

impl FieldBases {
    fn new(configured: &[String]) -> Self {
        Self {
            configured: configured.to_vec(),
            names: configured
                .iter()
                .map(|name| name.rsplit('.').next().unwrap_or(name).to_owned())
                .collect(),
        }
    }

    fn matches(&self, base: &Expr, aliases: &Aliases) -> bool {
        match base {
            Expr::Name(name) => self.names.contains(aliases.resolve(name.id.as_str())),
            Expr::Attribute(attribute) => self.names.contains(attribute.attr.as_str()),
            // `class Job(BaseModel, Generic[T])` names its base through a
            // subscript, as a generic model does.
            Expr::Subscript(subscript) => self.matches(&subscript.value, aliases),
            _ => false,
        }
    }

    fn matches_unshadowed(
        &self,
        base: &Expr,
        aliases: &Aliases,
        local_classes: &BTreeSet<String>,
    ) -> bool {
        if let Expr::Name(name) = base {
            if local_classes.contains(name.id.as_str())
                && !self
                    .configured
                    .iter()
                    .any(|configured| configured == name.id.as_str())
            {
                return false;
            }
        }
        self.matches(base, aliases)
    }
}

/// The names that a package's `__init__.py` files make part of its public API.
///
/// Under `private_only` with `respect_reexports`, a name here is left alone
/// wherever it is defined: a helper in `_upload.py` that the package root
/// re-exports is public API, and its defaults are public API with it.
#[derive(Clone, Debug, Default)]
struct Reexports {
    /// Whether a `from ... import *` stood in the way. The names behind it
    /// cannot be listed without resolving the module, so every name counts as
    /// re-exported.
    wildcard: bool,
    /// Whether this target module itself is reachable through a package import.
    module: bool,
    names: BTreeSet<String>,
}

impl Reexports {
    fn covers(&self, name: &str) -> bool {
        self.wildcard || self.names.contains(name)
    }
}

struct PerFileEnforcement {
    matcher: GlobMatcher,
    negated: bool,
    specificity: usize,
    pattern: String,
    enforcement: Enforcement,
}

#[derive(Clone, Debug)]
struct Diagnostic {
    path: PathBuf,
    line: usize,
    column: usize,
    code: &'static str,
    message: String,
    /// The text to delete to fix this, or `None` when there is nothing to
    /// delete — a syntax error is reported but cannot be fixed away.
    fix: Option<TextRange>,
}

/// A default that `--fix` removed, and the argument that replaces it.
#[derive(Clone, Debug)]
struct Removed {
    parameter: String,
    /// The default deletion to cancel if a checked call cannot be rewritten
    /// safely.
    fix: TextRange,
    /// Source text to pass at call sites, when the default can be reproduced
    /// without depending on names that the caller may not have imported.
    value: Option<String>,
}

type FixKey = (PathBuf, TextSize, TextSize);

fn fix_key(path: &Path, range: TextRange) -> FixKey {
    (path.to_path_buf(), range.start(), range.end())
}

/// Enough of a callable's shape to decide what a call to it is missing.
#[derive(Clone, Debug)]
struct Signature {
    name: String,
    /// The file that defines it. A call only resolves here when the caller
    /// actually imported this file, so an unrelated `connect` is left alone.
    path: PathBuf,
    /// Parameters that a call can fill positionally, in order. For a method
    /// this includes `self`; for a dataclass these are the constructor fields.
    positional: Vec<String>,
    /// How many leading entries of `positional` are positional-only.
    positional_only: usize,
    kind: Callable,
    /// Whether `positional` is the whole parameter list. A dataclass that
    /// inherits fields has a constructor this file cannot see.
    complete: bool,
    removed: Vec<Removed>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Callable {
    Function,
    /// A method. Only calls through `self`, `cls`, or the class's own name are
    /// rewritten, because the type of any other receiver is unknown.
    Method {
        class: String,
        receiver: Receiver,
    },
    /// A normal class call reaches its `__init__` after Python creates and
    /// supplies the instance.
    Constructor,
    Dataclass,
}

/// What a method is implicitly given ahead of its written arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Receiver {
    /// An ordinary method, given the instance.
    Instance,
    /// A `classmethod`, given the class however it is reached.
    Class,
    /// A `staticmethod`, given nothing.
    None,
}

/// The receiver a function body sees without naming it, and what it stands for.
///
/// The class travels with the receiver rather than being read back off the
/// class stack, because a class nested inside a method pushes its own name
/// while the method's receiver stays in view. Reading the top of that stack
/// from in there would name the nested class for a receiver belonging to the
/// enclosing one.
#[derive(Clone, Debug)]
struct ImplicitReceiver {
    /// The parameter name, conventionally `self` or `cls`.
    name: String,
    /// The qualified name of the class the receiver stands for.
    class: String,
    /// Whether it is a `classmethod`'s class rather than an instance.
    is_class: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MethodAliasKind {
    Direct,
    Static,
    Class,
    Property,
}

#[derive(Clone, Debug)]
struct MethodAlias {
    name: String,
    kind: MethodAliasKind,
    /// A statically named class whose method was copied into this namespace.
    /// `None` means the original is a method defined by this class itself.
    original_class: Option<String>,
}

impl Callable {
    /// Whether the name refers to a function, whose appearance outside a call
    /// is worth a warning. A class name appears in annotations and `isinstance`
    /// checks all the time, so those stay quiet.
    fn is_function(&self) -> bool {
        matches!(self, Self::Function | Self::Method { .. })
    }

    fn implicit_bound(&self) -> usize {
        usize::from(matches!(self, Self::Constructor))
    }
}

/// What a name in a file refers to, as far as the import statements say.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Binding {
    /// A module, so `name.attribute(...)` resolves into that file.
    Module(PathBuf),
    /// A symbol imported from a module, so `name(...)` resolves to it.
    Symbol(PathBuf, String),
    /// A name definitely bound, by an import whose checked definition is
    /// ambiguous or by a parameter of the scope being read, to something this
    /// file cannot follow. It shadows earlier candidates but cannot be
    /// resolved, and it is not free for a later definition to answer to.
    Unknown,
}

/// A replacement for a range of a file. Deletions carry an empty replacement;
/// call-site rewrites carry an empty range and the text to insert.
#[derive(Clone, Debug)]
struct Edit {
    range: TextRange,
    replacement: String,
    /// The call this edit belongs to. One call can need both a positional and
    /// a keyword insertion, at different offsets, and that is one updated call
    /// site rather than two.
    site: TextSize,
}

impl Edit {
    /// A deletion, which stands alone and is never counted as a call site.
    fn deletion(range: TextRange) -> Self {
        Self {
            range,
            replacement: String::new(),
            site: range.start(),
        }
    }
}

/// A call that could not be updated, and why.
#[derive(Debug)]
struct Skipped {
    path: PathBuf,
    line: usize,
    column: usize,
    callable: String,
    reason: String,
}

/// The callables `--fix` changed, indexed for resolution from a call site.
///
/// `None` against a name marks one that several definitions in the same file
/// share, which makes calls to it unresolvable.
#[derive(Default)]
struct Definitions {
    /// Functions and dataclasses, by defining file and then name.
    symbols: BTreeMap<PathBuf, BTreeMap<String, Option<Signature>>>,
    /// Methods, by defining file and class name, and then method name.
    methods: BTreeMap<(PathBuf, String), BTreeMap<String, Option<Signature>>>,
    /// Imported names in checked files, used to follow package re-exports to
    /// the file that owns a callable's signature.
    bindings: BTreeMap<(PathBuf, String), Binding>,
    /// Direct statically resolved base classes for method lookup.
    bases: BTreeMap<(PathBuf, String), Vec<(PathBuf, String)>>,
    /// Classes two suites of one statement gave different bases, so which
    /// ancestry the file ends up with is not known.
    uncertain_bases: BTreeSet<(PathBuf, String)>,
    /// Every name a fixed callable goes by, so a call that cannot be resolved
    /// to one can still be reported rather than silently left behind.
    names: BTreeSet<String>,
    /// Default deletions grouped by every callable spelling, used to retain
    /// them when an unresolved checked call may refer to that callable.
    fixes_by_name: BTreeMap<String, BTreeSet<FixKey>>,
}

impl Definitions {
    /// Record the bases a class definition names.
    ///
    /// Two suites of the same statement are alternatives: only one of them
    /// runs, and a class each writes under the same name is one class at
    /// runtime rather than two. Where they disagree on its bases, nothing here
    /// says which set the file ends up with, so the ancestry is held unknown
    /// and the inherited calls it would have answered are left alone. A
    /// definition that certainly runs settles the question again, since it is
    /// the one standing when the module is done.
    fn record_bases(
        &mut self,
        class: (PathBuf, String),
        bases: Vec<(PathBuf, String)>,
        alternative: bool,
    ) {
        if !alternative {
            self.uncertain_bases.remove(&class);
        } else if self.bases.get(&class).is_some_and(|held| *held != bases) {
            self.uncertain_bases.insert(class);
            return;
        }
        self.bases.insert(class, bases);
    }

    fn symbol(&self, file: &Path, name: &str) -> Option<&Signature> {
        let mut file = file.to_path_buf();
        let mut name = name.to_owned();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert((file.clone(), name.clone())) {
                return None;
            }
            if let Some(signature) = self.symbols.get(&file).and_then(|table| table.get(&name)) {
                return signature.as_ref();
            }
            let Binding::Symbol(next_file, next_name) =
                self.bindings.get(&(file.clone(), name.clone()))?
            else {
                return None;
            };
            file.clone_from(next_file);
            name.clone_from(next_name);
        }
    }

    fn method(&self, file: &Path, class: &str, name: &str) -> Option<&Signature> {
        self.method_in_mro(file, class, name, false)
    }

    fn class_identity(&self, file: &Path, class: &str) -> Option<(PathBuf, String)> {
        let mut file = file.to_path_buf();
        let mut class = class.to_owned();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert((file.clone(), class.clone())) {
                return None;
            }
            let key = (file.clone(), class.clone());
            if self.methods.contains_key(&key) || self.bases.contains_key(&key) {
                return Some(key);
            }
            let Binding::Symbol(next_file, next_class) =
                self.bindings.get(&(file.clone(), class.clone()))?
            else {
                return None;
            };
            file.clone_from(next_file);
            class.clone_from(next_class);
        }
    }

    /// The method a zero-argument `super()` call reaches.
    ///
    /// Python starts that lookup after the class the call appears in, so a
    /// method the class itself defines is never what `super().name(...)`
    /// calls, even when it is the one `--fix` changed.
    fn super_method(&self, file: &Path, class: &str, name: &str) -> Option<&Signature> {
        self.method_in_mro(file, class, name, true)
    }

    /// Whether this class, or a class it inherits from, has an ancestry the
    /// file does not settle. Nothing a lookup through it answers is reliable,
    /// so a default behind such a call has to stay where it is.
    fn ancestry_is_uncertain(&self, class: &(PathBuf, String)) -> bool {
        self.linearized_mro(class, &mut BTreeSet::new()).is_none()
    }

    fn method_in_mro(
        &self,
        file: &Path,
        class: &str,
        name: &str,
        skip_class: bool,
    ) -> Option<&Signature> {
        let identity = (file.to_path_buf(), class.to_owned());
        // Where no whole order can be walked the lookup still walks the start
        // of it, so a method written on the class itself, or on a base between
        // it and whatever left the rest unknown, resolves as it always did.
        // Only a lookup that would have to read past that point gives up, and
        // the call it belongs to is then reported and its default held back.
        let mro = self
            .linearized_mro(&identity, &mut BTreeSet::new())
            .unwrap_or_else(|| self.settled_ancestry(&identity));
        for identity in mro.iter().skip(usize::from(skip_class)) {
            if let Some(method) = self
                .methods
                .get(identity)
                .and_then(|methods| methods.get(name))
            {
                return method.as_ref();
            }
        }
        None
    }

    /// The classes a lookup certainly reaches before the order stops being
    /// known, in the order it reaches them.
    ///
    /// A class holds its own methods however its ancestry turns out, and so
    /// does each class on a chain of single bases leading down from it: with
    /// one base at each step the order is the chain itself, whatever the
    /// classes further down are. The walk stops at the first class whose bases
    /// the file does not settle, which is where a whole order would have had
    /// to pick one set of them, and at a class with several bases, whose order
    /// depends on ancestries that may be among the ones in doubt.
    fn settled_ancestry(&self, class: &(PathBuf, String)) -> Vec<(PathBuf, String)> {
        let mut settled: Vec<(PathBuf, String)> = Vec::new();
        let mut current = class.clone();
        while !self.uncertain_bases.contains(&current) && !settled.contains(&current) {
            settled.push(current.clone());
            let Some([base]) = self.bases.get(&current).map(Vec::as_slice) else {
                break;
            };
            current = base.clone();
        }
        settled
    }

    fn linearized_mro(
        &self,
        class: &(PathBuf, String),
        visiting: &mut BTreeSet<(PathBuf, String)>,
    ) -> Option<Vec<(PathBuf, String)>> {
        // A class whose bases are not known has no resolution order to walk:
        // answering with one set of them would rewrite calls against ancestors
        // the class may never have had.
        if self.uncertain_bases.contains(class) {
            return None;
        }
        if !visiting.insert(class.clone()) {
            return None;
        }
        let bases = self.bases.get(class).cloned().unwrap_or_default();
        let mut sequences = bases
            .iter()
            .map(|base| self.linearized_mro(base, visiting))
            .collect::<Option<Vec<_>>>()?;
        visiting.remove(class);
        sequences.push(bases);
        let mut result = vec![class.clone()];
        while sequences.iter().any(|sequence| !sequence.is_empty()) {
            let candidate = sequences.iter().find_map(|sequence| {
                let head = sequence.first()?;
                sequences
                    .iter()
                    .all(|other| !other.iter().skip(1).any(|item| item == head))
                    .then(|| head.clone())
            })?;
            result.push(candidate.clone());
            for sequence in &mut sequences {
                if sequence.first() == Some(&candidate) {
                    sequence.remove(0);
                }
            }
        }
        Some(result)
    }

    /// Give a subclass with no constructor of its own the signature of the
    /// checked `__init__` it inherits.
    fn index_inherited_constructors(&mut self) {
        let classes: Vec<(PathBuf, String)> = self.bases.keys().cloned().collect();
        for (file, class) in classes {
            if self
                .symbols
                .get(&file)
                .is_some_and(|symbols| symbols.contains_key(&class))
            {
                continue;
            }
            let Some(mut signature) = self.method(&file, &class, "__init__").cloned() else {
                continue;
            };
            signature.name.clone_from(&class);
            signature.kind = Callable::Constructor;
            self.symbols
                .entry(file)
                .or_default()
                .insert(class, Some(signature));
        }
    }
}

/// What checking one file produced.
#[derive(Default)]
struct Checked {
    diagnostics: Vec<Diagnostic>,
    signatures: Vec<Signature>,
    /// Calls the checker already knows `--fix` will not reach, such as those
    /// to a lambda whose default it removed.
    skipped: Vec<Skipped>,
}

/// What scanning files for calls to fixed callables produced.
#[derive(Default)]
struct CallSites {
    edits: BTreeMap<PathBuf, Vec<Edit>>,
    skipped: Vec<Skipped>,
    retained: BTreeSet<FixKey>,
}

/// The calls one file makes to fixed callables.
#[derive(Default)]
struct FileCallSites {
    edits: Vec<Edit>,
    skipped: Vec<Skipped>,
    retained: BTreeSet<FixKey>,
}

#[must_use]
pub fn main() -> ExitCode {
    match run() {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => ExitCode::from(1),
        Err(error) => {
            eprintln!("no-defaults: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool, String> {
    let cli = Cli::parse();
    if cli.diff && matches!(cli.output_format, OutputFormat::Json | OutputFormat::Github) {
        return Err("--diff cannot be combined with a machine-readable --output-format".to_owned());
    }
    let files = collect_files(&cli.paths)?;
    let settings = settings_for_files(&files, cli.private_only, cli.respect_reexports)?;
    if cli.show_settings {
        for (path, setting) in files.iter().zip(&settings) {
            println!("{}", path.display());
            println!("  project-root = {}", setting.project_root.display());
            println!(
                "  enforcement = {}",
                match setting.private_only {
                    Some(true) => "private",
                    Some(false) => "all",
                    None => "none",
                }
            );
            println!("  respect-reexports = {}", setting.respect_reexports);
            println!(
                "  field-base-classes = [{}]",
                setting
                    .field_bases
                    .configured
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok(false);
    }
    let fixing = cli.fix || cli.diff;
    let results: Vec<Checked> = files
        .par_iter()
        .zip(settings.par_iter())
        .map(|(path, setting)| {
            // An exempt file contributes no violations and no signatures, but
            // its calls are still rewritten when a callable it uses is fixed
            // elsewhere. Exemption is about which definitions are checked, not
            // about leaving the file broken at runtime.
            setting
                .private_only
                .map_or_else(Checked::default, |private_only| {
                    check_file(
                        path,
                        private_only,
                        &setting.project_root,
                        &setting.reexports,
                        &setting.field_bases,
                        fixing,
                    )
                })
        })
        .collect();
    let mut diagnostics = Vec::new();
    let mut signatures = Vec::new();
    let mut skipped = Vec::new();
    for checked in results {
        diagnostics.extend(checked.diagnostics);
        signatures.extend(checked.signatures);
        skipped.extend(checked.skipped);
    }
    diagnostics.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    if fixing && !diagnostics.is_empty() {
        return apply_fixes(&cli, &files, &diagnostics, signatures, skipped);
    }
    report_diagnostics(&diagnostics, cli.output_format, true)?;
    Ok(!diagnostics.is_empty())
}

/// Remove the defaults the diagnostics name, update the call sites that relied
/// on them, and report what happened. Returns whether anything is left over.
fn apply_fixes(
    cli: &Cli,
    files: &[PathBuf],
    diagnostics: &[Diagnostic],
    signatures: Vec<Signature>,
    skipped: Vec<Skipped>,
) -> Result<bool, String> {
    let mut call_sites = call_site_edits(files, signatures)?;
    // Calls the checker already knew were out of reach, such as those to a
    // lambda whose default was removed, are warned about alongside the rest.
    call_sites.skipped.extend(skipped);
    call_sites.skipped.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    for diagnostic in diagnostics {
        let Some(range) = diagnostic.fix else {
            continue;
        };
        if call_sites
            .retained
            .contains(&fix_key(&diagnostic.path, range))
        {
            continue;
        }
        call_sites
            .edits
            .entry(diagnostic.path.clone())
            .or_default()
            .push(Edit::deletion(range));
    }
    let mut updated = 0;
    let mut unfixed = BTreeSet::new();
    let changes = fixed_sources(call_sites.edits, &mut updated, &mut unfixed)?;
    if cli.diff {
        print_diffs(&changes);
        let remaining = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.fix.is_none_or(|range| {
                    call_sites
                        .retained
                        .contains(&fix_key(&diagnostic.path, range))
                }) || unfixed.contains(&diagnostic.path)
            })
            .cloned()
            .collect::<Vec<_>>();
        report_diagnostics(&remaining, cli.output_format, true)?;
        warn_about_skipped_calls(&call_sites.skipped);
        return Ok(true);
    }
    write_fixes_atomically(changes)?;
    // A syntax error carries no fix, and a file whose result would not have
    // parsed was left as it was, so both are still there afterwards and the
    // run has to say so rather than claim everything was fixed.
    let remaining_diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.fix.is_none_or(|range| {
                call_sites
                    .retained
                    .contains(&fix_key(&diagnostic.path, range))
            }) || unfixed.contains(&diagnostic.path)
        })
        .cloned()
        .collect::<Vec<_>>();
    let remaining = remaining_diagnostics.len();
    let summary = format!(
        "Found {} error{} ({} fixed, {remaining} remaining).",
        diagnostics.len(),
        if diagnostics.len() == 1 { "" } else { "s" },
        diagnostics.len() - remaining,
    );
    match cli.output_format {
        // The machine formats carry the diagnostics themselves. A summary line
        // on stdout would make the JSON unparseable, so it joins the warnings
        // on stderr rather than being dropped.
        OutputFormat::Json | OutputFormat::Github => {
            report_diagnostics(diagnostics, cli.output_format, true)?;
            eprintln!("{summary}");
        }
        OutputFormat::Full | OutputFormat::Concise => {
            report_diagnostics(&remaining_diagnostics, cli.output_format, false)?;
            println!("{summary}");
        }
    }
    report_call_sites(
        diagnostics,
        &call_sites.skipped,
        updated,
        cli.output_format,
        &unfixed,
        &call_sites.retained,
    );
    Ok(remaining > 0)
}

fn warn_about_skipped_calls(skipped: &[Skipped]) {
    for skip in skipped {
        eprintln!(
            "warning: {}:{}:{}: left the call to `{}` alone: {}",
            skip.path.display(),
            skip.line,
            skip.column,
            skip.callable,
            skip.reason
        );
    }
}

/// Report what `--fix` did to call sites, and what it could not reach.
/// How many defaults `--fix` actually removed.
///
/// A default counts only if it carried a fix and its file was written. One
/// left on disk because its result would not have parsed still has every
/// default it started with.
fn removed_defaults(
    diagnostics: &[Diagnostic],
    unfixed: &BTreeSet<PathBuf>,
    retained: &BTreeSet<FixKey>,
) -> usize {
    diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.code == "NOD001"
                && diagnostic.fix.is_some()
                && !diagnostic
                    .fix
                    .is_some_and(|range| retained.contains(&fix_key(&diagnostic.path, range)))
                && !unfixed.contains(&diagnostic.path)
        })
        .count()
}

fn report_call_sites(
    diagnostics: &[Diagnostic],
    skipped: &[Skipped],
    updated: usize,
    format: OutputFormat,
    unfixed: &BTreeSet<PathBuf>,
    retained: &BTreeSet<FixKey>,
) {
    let removed = removed_defaults(diagnostics, unfixed, retained);
    // A run that removed only unused `noqa` directives touched no call site
    // and has nothing to say here. Rewritten or skipped calls are reported
    // whether or not a default survived in a file that could not be written.
    if removed == 0 && updated == 0 && skipped.is_empty() {
        return;
    }
    // Under a machine format stdout carries the diagnostics and nothing else,
    // so this count joins the warnings on stderr rather than making the JSON
    // unparseable.
    let count = format!(
        "Updated {updated} call site{}.",
        if updated == 1 { "" } else { "s" }
    );
    match format {
        OutputFormat::Full | OutputFormat::Concise => println!("{count}"),
        OutputFormat::Json | OutputFormat::Github => eprintln!("{count}"),
    }
    warn_about_skipped_calls(skipped);
    if removed > 0 {
        eprintln!(
            "warning: {removed} default{} removed. Call sites in the checked files were \
             updated, but callers outside them, and calls made dynamically, were not. \
             Run your tests.",
            if removed == 1 { "" } else { "s" },
        );
    }
}

/// Insert the removed defaults as explicit arguments at every call the checked
/// files make to a callable that `--fix` changed.
///
/// A call resolves only when the calling file's own imports say it refers to
/// the file that was fixed. A bare name match is not enough: a project with its
/// own `connect` must not have `socket.connect` rewritten.
/// The physical path of each checked file, by the spelling it was collected
/// under.
///
/// A file named through a symlink and the file it points at are one module, so
/// resolution and the definition index are keyed by the physical path. What is
/// reported stays the spelling the file was collected under.
fn physical_paths(files: &[PathBuf]) -> BTreeMap<&Path, PathBuf> {
    files
        .iter()
        .map(|path| {
            (
                path.as_path(),
                std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()),
            )
        })
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "building the cross-file definition index and scanning its callers is one pipeline"
)]
fn call_site_edits(files: &[PathBuf], signatures: Vec<Signature>) -> Result<CallSites, String> {
    let physical = physical_paths(files);
    let physical_path = |path: &Path| -> PathBuf {
        physical
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_path_buf())
    };
    let mut definitions = Definitions::default();
    for signature in signatures {
        let display_name = signature
            .name
            .rsplit('.')
            .next()
            .unwrap_or(&signature.name)
            .to_owned();
        definitions.names.insert(display_name.clone());
        definitions
            .fixes_by_name
            .entry(display_name)
            .or_default()
            .extend(
                signature
                    .removed
                    .iter()
                    .map(|removed| fix_key(&signature.path, removed.fix)),
            );
        let defining = physical_path(&signature.path);
        let table = match &signature.kind {
            Callable::Method { class, .. } => definitions
                .methods
                .entry((defining, class.clone()))
                .or_default(),
            Callable::Function | Callable::Constructor | Callable::Dataclass => {
                definitions.symbols.entry(defining).or_default()
            }
        };
        match table.entry(signature.name.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(Some(signature));
            }
            Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }
    let mut call_sites = CallSites::default();
    if definitions.names.is_empty() {
        return Ok(call_sites);
    }
    let known: BTreeSet<&Path> = physical.values().map(PathBuf::as_path).collect();
    for path in files {
        let Ok(source) = read_source(path) else {
            continue;
        };
        let Ok(parsed) = parse_module(&source) else {
            continue;
        };
        let importer = physical_path(path);
        let mut bindings = BTreeMap::new();
        collect_bindings(parsed.suite(), &importer, &known, &mut bindings);
        let mut outside_functions = BTreeSet::new();
        collect_indexed_class_names(parsed.suite(), None, &mut outside_functions);
        index_method_bases(
            parsed.suite(),
            &importer,
            &known,
            &bindings,
            ClassNamespace::module(),
            LexicalClasses::of_module(&outside_functions),
            &mut definitions,
        );
        definitions.bindings.extend(
            bindings
                .into_iter()
                .map(|(name, binding)| ((importer.clone(), name), binding)),
        );
    }
    definitions.index_inherited_constructors();
    let results: Vec<Result<FileCallSites, String>> = files
        .par_iter()
        .map(|path| {
            let Ok(source) = read_source(path) else {
                // The checker has already emitted a per-file diagnostic for
                // unreadable source. It cannot contain a safely rewritable
                // call site in this pass, but it must not abort other files.
                return Ok(FileCallSites::default());
            };
            // A file the parser rejects holds no calls this pass can see. It
            // is reported as a syntax error by the checker, so skipping it
            // here leaves the rest of the project fixable.
            match rewrite_calls(path, &physical_path(path), &source, &definitions, &known) {
                Ok(file) => Ok(file),
                Err(_) if parse_module(&source).is_err() => Ok(FileCallSites::default()),
                Err(error) => Err(error),
            }
        })
        .collect();
    for (path, result) in files.iter().zip(results) {
        let file = result?;
        if !file.edits.is_empty() {
            call_sites
                .edits
                .entry(path.clone())
                .or_default()
                .extend(file.edits);
        }
        call_sites.skipped.extend(file.skipped);
        call_sites.retained.extend(file.retained);
    }
    call_sites.skipped.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    Ok(call_sites)
}

/// The classes `index_method_bases` records without entering a function.
///
/// The walk descends into a class body, a function body and the suites of a
/// control-flow statement, and nothing else. A name it reaches must not be
/// held against a class in a function body: holding a name for a class that is
/// never recorded would leave both of them unindexed, and an inherited call in
/// the one that is written would be left alone after the default behind it was
/// removed. This mirrors the statements the walk itself matches on, and has to
/// keep mirroring them.
fn collect_indexed_class_names(
    statements: &[Stmt],
    parent_class: Option<&str>,
    found: &mut BTreeSet<String>,
) {
    for statement in statements {
        if let Stmt::ClassDef(class) = statement {
            let identity = qualified_name(parent_class, class.name.as_str());
            collect_indexed_class_names(&class.body, Some(&identity), found);
            found.insert(identity);
            continue;
        }
        for suite in control_flow_suites(statement) {
            collect_indexed_class_names(suite, parent_class, found);
        }
    }
}

/// The class names a file reaches without entering a function, and whether the
/// walk has entered one.
///
/// Nothing that names a class puts the function holding it in the name, here
/// or anywhere else a class is identified, so a class written in a function
/// body is spelled exactly as a namesake written outside one. The class
/// outside keeps the name, since it is the only one another file can reach.
#[derive(Clone, Copy)]
struct LexicalClasses<'a> {
    names: &'a BTreeSet<String>,
    in_function: bool,
}

impl<'a> LexicalClasses<'a> {
    fn of_module(names: &'a BTreeSet<String>) -> Self {
        Self {
            names,
            in_function: false,
        }
    }

    fn entering_function(self) -> Self {
        Self {
            in_function: true,
            ..self
        }
    }

    /// Whether a class of this identity holds the name where it is written.
    fn holds(self, identity: &str) -> bool {
        !self.in_function || !self.names.contains(identity)
    }
}

/// Where the walk stands, for naming the classes it finds and for judging
/// whether each one is the class the file ends up with.
///
/// A class body is a namespace a name reaches from outside it, so a class
/// written in one is named under the class holding it. A function body is not:
/// each call makes its classes afresh, and two functions defining a class of
/// the same name define two classes. Naming those under the scopes around them
/// keeps them apart, and matches the identity the checker gives the same class.
#[derive(Clone, Copy)]
struct ClassNamespace<'a> {
    /// The class whose body is being walked, when the statements are directly
    /// in one.
    parent: Option<&'a str>,
    /// The enclosing class and function names, outermost first.
    lexical_scope: &'a [String],
    /// Whether the statements sit in one of several suites of a control-flow
    /// statement of which only one runs, so a class written here stands beside
    /// whatever a sibling suite writes under the same name.
    alternative: bool,
}

impl ClassNamespace<'_> {
    fn module() -> Self {
        Self {
            parent: None,
            lexical_scope: &[],
            alternative: false,
        }
    }

    /// The identity a class of this spelling is indexed under.
    fn identity(self, spelling: &str) -> String {
        self.parent.map_or_else(
            || qualified_lexical_name(self.lexical_scope, spelling),
            |parent| qualified_name(Some(parent), spelling),
        )
    }
}

/// A class a scope defines, and the stretch of that scope over which the
/// spelling a base expression uses still names it.
///
/// The class takes the name over from an import of the same name where it is
/// written, and an import written below it takes the name straight back, so
/// the class only answers for the spelling between the two.
struct LocalClass {
    /// The identity the class is indexed under, which the scopes around it
    /// name.
    identity: String,
    /// Where the class statement that binds the spelling is written.
    defined_at: TextSize,
    /// Where an import of the same name binds the spelling again, when one in
    /// this scope does.
    superseded_at: Option<TextSize>,
}

impl LocalClass {
    /// Whether this class is what the spelling names at `read_at`.
    fn holds_at(&self, read_at: TextSize) -> bool {
        self.defined_at < read_at && !self.superseded_before(read_at)
    }

    /// Whether an import has already taken the spelling back by `read_at`.
    ///
    /// This is not the negation of `holds_at`: a class written below the base
    /// that reads it holds neither, and the two are told apart where a name
    /// nothing else binds still answers with the class the scope defines.
    fn superseded_before(&self, read_at: TextSize) -> bool {
        self.superseded_at.is_some_and(|offset| offset < read_at)
    }
}

/// The names an import statement binds, each under the spelling the rest of
/// the scope reads it as.
///
/// A dotted `import package.module` binds only the top package, so that is the
/// name it takes over; an `as` clause binds the name it gives instead. A star
/// import binds whatever the other module exports, which is not known here, so
/// it takes no name over.
fn imported_names(statement: &Stmt) -> Vec<&str> {
    match statement {
        Stmt::Import(import) => import
            .names
            .iter()
            .map(|alias| {
                alias.asname.as_ref().map_or_else(
                    || alias.name.as_str().split('.').next().unwrap_or_default(),
                    ast::Identifier::as_str,
                )
            })
            .collect(),
        Stmt::ImportFrom(import) => import
            .names
            .iter()
            .filter(|alias| alias.name.as_str() != "*")
            .map(|alias| {
                alias
                    .asname
                    .as_ref()
                    .map_or_else(|| alias.name.as_str(), ast::Identifier::as_str)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Where each spelling an import in this scope binds is taken back from a
/// class defined above it.
///
/// An import written in a suite that may not run leaves the class standing for
/// anything after the statement, and a class whose ancestry depends on which
/// suite ran is not settled here. A suite the statement is certain to enter is
/// read, though: it binds for the code after it exactly as a statement written
/// beside that code would, which is the same rule `carry_suite_aliases`
/// settles a name by. Every import of a name is kept, in the order they are
/// written: a scope that imports a name, defines a class of it and imports it
/// again is reclaimed by the second import, and only the offsets after the
/// class can say which one that is.
fn import_rebindings(statements: &[Stmt]) -> BTreeMap<String, Vec<TextSize>> {
    let mut rebindings = BTreeMap::new();
    collect_import_rebindings(statements, &mut rebindings);
    rebindings
}

fn collect_import_rebindings(statements: &[Stmt], found: &mut BTreeMap<String, Vec<TextSize>>) {
    for statement in statements {
        for bound in imported_names(statement) {
            found
                .entry(bound.to_owned())
                .or_default()
                .push(statement.start());
        }
        // A class body and a function body each bind their imports in a
        // namespace of their own, and neither is among the suites below.
        let suites = runnable_suites(statement);
        if suites.len() == 1 && a_suite_certainly_runs(statement) {
            for suite in suites {
                collect_import_rebindings(suite, found);
            }
        }
    }
}

/// Where the last class statement binding each name in this scope is written.
///
/// A scope that writes the name again below an import has taken it back from
/// that import, and the offsets a single spelling carries cannot say so. The
/// spelling is left to the import instead of being handed an answer the class
/// written last contradicts.
fn last_class_offsets(statements: &[Stmt], found: &mut BTreeMap<String, TextSize>) {
    for statement in statements {
        if let Stmt::ClassDef(class) = statement {
            found.insert(class.name.to_string(), statement.start());
            continue;
        }
        for suite in control_flow_suites(statement) {
            last_class_offsets(suite, found);
        }
    }
}

/// The classes a scope defines, under the spelling a base expression uses for
/// each one there.
///
/// A class this scope defines itself takes the name over from an import of the
/// same name, so a subclass written after it is built on the local class. One
/// written before it still reaches the import, which is what the offsets each
/// spelling carries decide.
fn local_class_identities(
    statements: &[Stmt],
    namespace: ClassNamespace<'_>,
) -> BTreeMap<String, LocalClass> {
    let mut spellings = BTreeMap::new();
    collect_local_class_offsets(statements, None, &mut spellings);
    let rebindings = import_rebindings(statements);
    let mut last_written = BTreeMap::new();
    last_class_offsets(statements, &mut last_written);
    spellings
        .into_iter()
        .map(|(spelling, defined_at)| {
            let identity = namespace.identity(&spelling);
            // A dotted spelling is reclaimed by an import of its root, which
            // is the name the import actually binds: once `package` is the
            // module again, `package.module.Base` is read there rather than in
            // the classes nested under the local `package`.
            let root = spelling.split('.').next().unwrap_or(spelling.as_str());
            // The import that takes the spelling back is the first one written
            // below every class statement binding the name and below this
            // class: a scope that writes the class again under an import has
            // taken the name back from it, and it is the next import after
            // that, if any, which reclaims it.
            let claimed_until = last_written
                .get(root)
                .copied()
                .unwrap_or(defined_at)
                .max(defined_at);
            let superseded_at = rebindings.get(root).and_then(|offsets| {
                offsets
                    .iter()
                    .copied()
                    .find(|offset| claimed_until < *offset)
            });
            (
                spelling,
                LocalClass {
                    identity,
                    defined_at,
                    superseded_at,
                },
            )
        })
        .collect()
}

/// Record the bases and attribute names of every class a file defines.
///
/// A class body and a function body each open a namespace of their own, so the
/// walk descends into both, naming what it finds under the class it is written
/// in, and leaving a name alone where `LexicalClasses` says another class
/// holds it.
fn index_method_bases(
    statements: &[Stmt],
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &BTreeMap<String, Binding>,
    namespace: ClassNamespace<'_>,
    lexical_classes: LexicalClasses<'_>,
    definitions: &mut Definitions,
) {
    let classes = local_class_identities(statements, namespace);
    let mut scope = BaseScope {
        namespace,
        lexical_classes,
        classes: &classes,
        aliases: BTreeMap::new(),
        contested: BTreeSet::new(),
        reclaimed: BTreeSet::new(),
    };
    index_scope_method_bases(
        statements,
        importer,
        known,
        bindings,
        definitions,
        &mut scope,
    );
}

/// What a scope has bound where a class written in it is read.
///
/// A control-flow suite opens no scope of its own: a class written in one is
/// spelled as a class beside it, and it reaches the classes the scope around it
/// defines and the aliases that scope had already bound. Carrying this into the
/// suite is what keeps a base written outside it in reach.
struct BaseScope<'a> {
    namespace: ClassNamespace<'a>,
    lexical_classes: LexicalClasses<'a>,
    /// Every class the scope defines, under the spelling a base expression
    /// uses for it, beside where it is written.
    classes: &'a BTreeMap<String, LocalClass>,
    /// The classes the scope's assignments have named so far.
    aliases: BTreeMap<String, (PathBuf, String)>,
    /// Names alternative suites bind to different classes, which therefore
    /// stand for no one class in the scope holding them.
    contested: BTreeSet<String>,
    /// Names an import in this scope has taken back off whatever an assignment
    /// had put behind them. The classes the scope defines carry the offsets
    /// that settle the same question; an alias carries none, so the reclaim is
    /// recorded here for the scope around a suite to read.
    reclaimed: BTreeSet<String>,
}

impl<'a> BaseScope<'a> {
    /// The same scope as one of a statement's suites sees it. An alternative
    /// suite is one of several of which only one runs, so a class it defines
    /// may not be the class the file ends up with.
    fn in_suite(&self, alternative: bool) -> BaseScope<'a> {
        BaseScope {
            namespace: ClassNamespace {
                alternative: self.namespace.alternative || alternative,
                ..self.namespace
            },
            lexical_classes: self.lexical_classes,
            classes: self.classes,
            aliases: self.aliases.clone(),
            contested: self.contested.clone(),
            // The suite records what its own imports take back; what the scope
            // around it already reclaimed is spent.
            reclaimed: BTreeSet::new(),
        }
    }
}

fn index_scope_method_bases(
    statements: &[Stmt],
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &BTreeMap<String, Binding>,
    definitions: &mut Definitions,
    scope: &mut BaseScope<'_>,
) {
    let namespace = scope.namespace;
    let lexical_classes = scope.lexical_classes;
    for statement in statements {
        let defined_at = statement.start();
        match statement {
            Stmt::ClassDef(class) => {
                let identity = namespace.identity(class.name.as_str());
                record_class_bases_and_attributes(
                    class,
                    &identity,
                    importer,
                    bindings,
                    definitions,
                    scope,
                );
                let mut inner = namespace.lexical_scope.to_vec();
                inner.push(class.name.to_string());
                index_method_bases(
                    &class.body,
                    importer,
                    known,
                    &scoped_bindings(&class.body, None, importer, known, bindings),
                    ClassNamespace {
                        parent: Some(&identity),
                        lexical_scope: &inner,
                        alternative: namespace.alternative,
                    },
                    lexical_classes,
                    definitions,
                );
            }
            Stmt::FunctionDef(function) => {
                let mut inner = namespace.lexical_scope.to_vec();
                inner.push(function.name.to_string());
                index_method_bases(
                    &function.body,
                    importer,
                    known,
                    &scoped_bindings(
                        &function.body,
                        Some(&function.parameters),
                        importer,
                        known,
                        bindings,
                    ),
                    ClassNamespace {
                        parent: None,
                        lexical_scope: &inner,
                        alternative: namespace.alternative,
                    },
                    lexical_classes.entering_function(),
                    definitions,
                );
            }
            Stmt::Import(_) | Stmt::ImportFrom(_) => {
                // An import binds its names afresh, so whatever class an
                // assignment above had put behind one of them is no longer
                // what a base written below reads there. The classes the
                // scope defines are taken back by the same rule, which
                // `local_class_identities` settles from the offsets.
                for bound in imported_names(statement) {
                    scope.aliases.remove(bound);
                    scope.reclaimed.insert(bound.to_owned());
                }
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) => {
                let Some((value, targets)) = assigned_value_and_targets(statement) else {
                    continue;
                };
                let Some(identity) = method_base_identity(
                    value,
                    importer,
                    bindings,
                    scope.classes,
                    defined_at,
                    &scope.aliases,
                    &definitions.methods,
                ) else {
                    continue;
                };
                // A name that stands for either of two classes hands that
                // doubt on: what is read here is whichever class the name it
                // was read from turned out to be, so the target is no more
                // settled than the source was.
                let contested_source =
                    base_root_name(value).is_some_and(|name| scope.contested.contains(name));
                for target in targets {
                    if let Expr::Name(alias) = target {
                        // An assignment no alternative suite guards takes the
                        // name over for good, which settles it again after
                        // competing suites left it standing for either class.
                        if contested_source {
                            scope.contested.insert(alias.id.to_string());
                        } else if !scope.namespace.alternative {
                            scope.contested.remove(alias.id.as_str());
                        }
                        // Binding the name again spends whatever an import
                        // above took it back from: what stands here now is
                        // this class, and the scope around a suite should be
                        // handed that rather than the reclaim.
                        scope.reclaimed.remove(alias.id.as_str());
                        scope.aliases.insert(alias.id.to_string(), identity.clone());
                    }
                }
            }
            _ => index_control_flow_method_bases(
                statement,
                importer,
                known,
                bindings,
                definitions,
                scope,
            ),
        }
    }
}

/// Record one class's bases and the attribute names its body leaves behind.
fn record_class_bases_and_attributes(
    class: &ast::StmtClassDef,
    identity: &str,
    importer: &Path,
    bindings: &BTreeMap<String, Binding>,
    definitions: &mut Definitions,
    scope: &BaseScope<'_>,
) {
    // A class written in a function body leaves the name to a namesake written
    // outside one: recording this one under it would replace that class's bases
    // with these and leave its calls resolved against the wrong ancestry. A
    // class nested in here can still hold a name of its own, so the walk goes on
    // either way.
    if !scope.lexical_classes.holds(identity) {
        return;
    }
    let bases = class
        .arguments
        .iter()
        .flat_map(|arguments| arguments.args.iter())
        .filter_map(|base| {
            method_base_identity(
                base,
                importer,
                bindings,
                scope.classes,
                class.start(),
                &scope.aliases,
                &definitions.methods,
            )
        })
        .collect();
    let methods = definitions
        .methods
        .entry((importer.to_path_buf(), identity.to_owned()))
        .or_default();
    // Every name the body still holds at the end of it is an attribute of the
    // class, whether a `def` wrote it or an assignment such as
    // `__init__ = setup` did. Recording the assigned ones too keeps them
    // shadowing what the bases hold: a subclass that binds `__init__` has a
    // constructor of its own, and rewriting its calls against an ancestor's
    // `__init__` would pass parameters the binding does not take. A name the
    // body never leaves behind is a shadow that does not exist, and recording
    // it would stop the lookup that should have walked on to a base.
    for name in BoundNames::of_class_attributes(&class.body) {
        methods.entry(name).or_insert(None);
    }
    definitions.record_bases(
        (importer.to_path_buf(), identity.to_owned()),
        bases,
        scope.namespace.alternative,
    );
    // A base spelled with a name competing suites bind to different classes
    // names a different ancestry depending on which of them ran, so the file
    // settles none of them, exactly as it settles none for a class those
    // suites give different bases. The name is read at the root of the base,
    // so a parameterized `Alias[int]` and a member of one are caught with it.
    if class_bases(class)
        .filter_map(base_root_name)
        .any(|name| scope.contested.contains(name))
    {
        definitions
            .uncertain_bases
            .insert((importer.to_path_buf(), identity.to_owned()));
    }
}

/// Record the bases of every class a control-flow suite holds.
///
/// A suite opens no namespace of its own, so a class written in one is named
/// exactly as a class beside it is, and a subclass written after it in the
/// same suite inherits from it. Leaving these bodies unwalked would record no
/// bases for either, and an inherited call would keep its arguments while the
/// default behind it was stripped. The suite is read with the scope around it
/// rather than afresh, so a base written above the statement is still in reach.
fn index_control_flow_method_bases(
    statement: &Stmt,
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &BTreeMap<String, Binding>,
    definitions: &mut Definitions,
    scope: &mut BaseScope<'_>,
) {
    let suites = runnable_suites(statement);
    let alternative = suites.len() > 1;
    // One suite the statement is certain to enter binds for the code after it
    // just as a statement written beside that code would. Anything else may
    // leave a name as it found it.
    let settles = !alternative && a_suite_certainly_runs(statement);
    for suite in suites {
        let mut inner = scope.in_suite(alternative);
        index_scope_method_bases(suite, importer, known, bindings, definitions, &mut inner);
        carry_suite_aliases(scope, &inner, settles);
    }
}

/// Whether a statement is certain to run one of the suites it holds.
///
/// A `with` body runs. An `if` chain runs one of its suites only when a test
/// the tool can read is true, or an `else` catches what the tests do not; an
/// `if` without either may bind nothing at all. A loop body may never be
/// entered, a `try` may leave part-way through, and a `match` without a
/// wildcard may match nothing, so none of those settles a name either.
fn a_suite_certainly_runs(statement: &Stmt) -> bool {
    let Stmt::If(branch) = statement else {
        return matches!(statement, Stmt::With(_));
    };
    std::iter::once(Some(branch.test.as_ref()))
        .chain(
            branch
                .elif_else_clauses
                .iter()
                .map(|clause| clause.test.as_ref()),
        )
        .any(|test| {
            test.is_none_or(|test| {
                matches!(
                    Truthiness::from_expr(test, |_| false),
                    Truthiness::True | Truthiness::Truthy
                )
            })
        })
}

/// Carry what a suite bound out into the scope around it.
///
/// A suite the statement is certain to enter leaves its names standing for the
/// code after it. Any other may not run, so a name it binds stands for either
/// what it bound or what was there before: a class built on such a name has an
/// ancestry for each candidate, and resolving its inherited calls against any
/// one of them strips the defaults behind the others. A contest the suite
/// itself found travels out whichever kind of suite it is, since a wrapper
/// that is certain to run settles nothing the branches inside it left open.
fn carry_suite_aliases(scope: &mut BaseScope<'_>, suite: &BaseScope<'_>, settles: bool) {
    for (spelling, identity) in &suite.aliases {
        if settles {
            // Binding what the scope already held still settles the name: an
            // earlier branch may have left another candidate standing, and
            // this assignment runs whatever that branch did.
            scope.contested.remove(spelling);
            scope.aliases.insert(spelling.clone(), identity.clone());
        } else if scope.aliases.get(spelling) != Some(identity) {
            scope.contested.insert(spelling.clone());
        }
    }
    // An import the suite makes takes a name back for the code after the
    // statement too, where the suite is one that certainly runs. Only the
    // names a suite binds travel out above; a name it unbound would otherwise
    // be left standing, and a base spelled with it read as the class the
    // assignment put there rather than the module the import names.
    //
    // A suite that may not run is left alone. Marking the name unsettled there
    // would decline the base while the defaults behind it were still stripped,
    // which turns a wrong argument into a call that cannot run at all.
    if settles {
        for spelling in &suite.reclaimed {
            scope.aliases.remove(spelling);
        }
    }
    scope.contested.extend(suite.contested.iter().cloned());
}

/// The suites of a statement whose classes the file can end up with.
///
/// A test the tool can already read leaves only one suite of an `if` chain
/// standing, and a suite behind a test that is never true defines nothing the
/// file keeps. Dropping those here is what lets a class guarded by `if True`
/// still be resolved against the base it names.
fn runnable_suites(statement: &Stmt) -> Vec<&[Stmt]> {
    let Stmt::If(branch) = statement else {
        return control_flow_suites(statement);
    };
    let clauses = std::iter::once((Some(branch.test.as_ref()), branch.body.as_slice())).chain(
        branch
            .elif_else_clauses
            .iter()
            .map(|clause| (clause.test.as_ref(), clause.body.as_slice())),
    );
    let mut suites = Vec::new();
    for (test, body) in clauses {
        match test.map_or(Truthiness::True, |test| {
            Truthiness::from_expr(test, |_| false)
        }) {
            Truthiness::False | Truthiness::Falsey | Truthiness::None => {}
            Truthiness::True | Truthiness::Truthy => {
                suites.push(body);
                return suites;
            }
            Truthiness::Unknown => suites.push(body),
        }
    }
    suites
}

/// The statement bodies a control-flow statement holds, in the order they are
/// written. Anything else holds none.
fn control_flow_suites(statement: &Stmt) -> Vec<&[Stmt]> {
    match statement {
        Stmt::If(branch) => std::iter::once(branch.body.as_slice())
            .chain(
                branch
                    .elif_else_clauses
                    .iter()
                    .map(|clause| clause.body.as_slice()),
            )
            .collect(),
        Stmt::For(loop_) => vec![&loop_.body, &loop_.orelse],
        Stmt::While(loop_) => vec![&loop_.body, &loop_.orelse],
        Stmt::With(block) => vec![&block.body],
        Stmt::Try(block) => std::iter::once(block.body.as_slice())
            .chain(block.handlers.iter().map(|handler| {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                handler.body.as_slice()
            }))
            .chain([block.orelse.as_slice(), block.finalbody.as_slice()])
            .collect(),
        Stmt::Match(block) => block
            .cases
            .iter()
            .map(|case| case.body.as_slice())
            .collect(),
        _ => Vec::new(),
    }
}

/// The value an assignment binds and the targets it binds it to.
///
/// An annotated binding names a class just as a plain one does, so a subclass
/// reaching the original through it is linked the same way.
fn assigned_value_and_targets(statement: &Stmt) -> Option<(&Expr, &[Expr])> {
    match statement {
        Stmt::Assign(assign) => Some((assign.value.as_ref(), assign.targets.as_slice())),
        Stmt::AnnAssign(assign) => assign
            .value
            .as_deref()
            .map(|value| (value, std::slice::from_ref(&*assign.target))),
        _ => None,
    }
}

/// The bindings a nested scope sees: the ones the scopes around it left
/// standing, less the names the scope's own parameters claim, plus the imports
/// the scope makes for itself. `collect_bindings` stops at every scope
/// boundary, so a body is read for its own imports as it is entered; without
/// that, a `from module import Base` written beside the class that inherits
/// from it would name nothing, though the same pair at module level resolves.
/// A parameter pushes the other way: it takes its name over for the whole
/// call, so a base spelled with it is whatever the caller handed in, and
/// linking the subclass to the import of that spelling would rewrite inherited
/// calls against a class the subclass never had.
fn scoped_bindings(
    body: &[Stmt],
    parameters: Option<&ast::Parameters>,
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &BTreeMap<String, Binding>,
) -> BTreeMap<String, Binding> {
    let mut scoped = bindings.clone();
    for name in parameters.into_iter().flat_map(BoundNames::of_parameters) {
        // Claimed, not vacated. Merely dropping the name would leave it free
        // for a class written later in the same body to answer to, and that
        // class does not exist yet where the subclass is written.
        scoped.insert(name, Binding::Unknown);
    }
    collect_bindings(body, importer, known, &mut scoped);
    scoped
}

/// Where each class this file defines is written, under the name a base
/// expression would spell it with — nested classes included, so `Outer.Inner`
/// is found as readily as a class at the top level.
fn collect_local_class_offsets(
    statements: &[Stmt],
    prefix: Option<&str>,
    found: &mut BTreeMap<String, TextSize>,
) {
    for statement in statements {
        match statement {
            Stmt::ClassDef(class) => {
                let qualified = qualified_name(prefix, class.name.as_str());
                found.entry(qualified.clone()).or_insert(statement.start());
                collect_local_class_offsets(&class.body, Some(&qualified), found);
            }
            _ => {
                for suite in control_flow_suites(statement) {
                    collect_local_class_offsets(suite, prefix, found);
                }
            }
        }
    }
}

/// Whether a class written in this file above `defined_at`, or a name this
/// scope has already bound to a class, holds the spelling a base expression's
/// prefix uses, so the prefix is that class rather than whatever a module of
/// the same dotted name would be. An assignment carries a class over to a name
/// just as a class statement does, and the aliases collected so far are only
/// those bound above, so no offset is needed to keep the order.
fn prefix_names_a_local_class(
    prefix: &Expr,
    local_classes: &BTreeMap<String, LocalClass>,
    defined_at: TextSize,
    aliases: &BTreeMap<String, (PathBuf, String)>,
) -> bool {
    let mut expression = prefix;
    loop {
        if dotted_name(expression).is_some_and(|spelling| {
            local_classes
                .get(&spelling)
                .is_some_and(|class| class.holds_at(defined_at))
                || aliases.contains_key(&spelling)
        }) {
            return true;
        }
        match expression {
            Expr::Attribute(attribute) => expression = attribute.value.as_ref(),
            _ => return false,
        }
    }
}

/// Resolve the class identity denoted by a base expression for method lookup.
fn method_base_identity(
    expression: &Expr,
    importer: &Path,
    bindings: &BTreeMap<String, Binding>,
    local_classes: &BTreeMap<String, LocalClass>,
    defined_at: TextSize,
    aliases: &BTreeMap<String, (PathBuf, String)>,
    methods: &BTreeMap<(PathBuf, String), BTreeMap<String, Option<Signature>>>,
) -> Option<(PathBuf, String)> {
    match expression {
        Expr::Subscript(subscript) => method_base_identity(
            &subscript.value,
            importer,
            bindings,
            local_classes,
            defined_at,
            aliases,
            methods,
        ),
        Expr::Name(name) => aliases.get(name.id.as_str()).cloned().or_else(|| {
            let local = local_classes.get(name.id.as_str());
            // A class already defined here holds the name, whatever an import
            // of the same name bound earlier, and only up to the import that
            // takes the name back.
            if let Some(class) = local.filter(|class| class.holds_at(defined_at)) {
                return Some((importer.to_path_buf(), class.identity.clone()));
            }
            match bindings.get(name.id.as_str()) {
                Some(Binding::Symbol(file, class)) => Some((file.clone(), class.clone())),
                // Something holds the name here that cannot be followed, so
                // the class it stands for is unknown. Reading on to a class
                // of the same spelling written further down the scope would
                // answer with one the subclass was never built on.
                Some(Binding::Unknown) => None,
                // A class the scope defines answers for a name nothing else
                // here binds, even where it is written below the base reading
                // it. An import that has already taken the name over is not
                // such a name: the base was built on whatever that import
                // bound, and where this pass cannot follow it — an unchecked
                // module, or a module bound rather than a symbol — the
                // spelling is left unresolved rather than answered with a
                // class the file has moved past.
                _ => local
                    .filter(|class| !class.superseded_before(defined_at))
                    .map(|class| (importer.to_path_buf(), class.identity.clone())),
            }
        }),
        Expr::Attribute(attribute) => {
            // A dotted submodule binding names the module its components spell
            // out, which is more specific than reading those same components
            // as classes nested in the package initializer — the reading the
            // prefix would otherwise be given, since the initializer's own
            // namesake answers before the module lookup below is reached.
            //
            // A class this scope has already put behind the spelling still
            // holds it first, whether it was written out as a class statement
            // or assigned to the name: the checks that follow settle that, so
            // the shortcut only fires where nothing above claimed the name.
            if !prefix_names_a_local_class(&attribute.value, local_classes, defined_at, aliases) {
                if let Some(module) = dotted_name(&attribute.value) {
                    if let Some(Binding::Module(file)) = bindings.get(&module) {
                        return Some((file.clone(), attribute.attr.to_string()));
                    }
                }
            }
            // The prefix may be a name this scope bound to a class rather than
            // a spelling written out in full, and an alias of an enclosing
            // class reaches the same nested class the dotted form does. The
            // member is only taken when the class it names is one this pass
            // has recorded methods for, so an ordinary attribute access is
            // left to the module lookup below.
            if let Some((file, parent)) = method_base_identity(
                &attribute.value,
                importer,
                bindings,
                local_classes,
                defined_at,
                aliases,
                methods,
            ) {
                let class = format!("{parent}.{}", attribute.attr);
                if methods.contains_key(&(file.clone(), class.clone())) {
                    return Some((file, class));
                }
            }
            let qualified = dotted_name(expression)?;
            // A nested class holds the dotted name only once it is written,
            // the same rule a simple name follows above. Before that the
            // prefix still names whatever an import bound.
            if let Some(local) = local_classes
                .get(&qualified)
                .filter(|local| local.holds_at(defined_at))
                .filter(|local| {
                    methods
                        .keys()
                        .any(|(file, class)| file == importer && *class == local.identity)
                })
            {
                return Some((importer.to_path_buf(), local.identity.clone()));
            }
            let module = dotted_name(&attribute.value)?;
            let Binding::Module(file) = bindings.get(&module)? else {
                return None;
            };
            Some((file.clone(), attribute.attr.to_string()))
        }
        _ => None,
    }
}

/// Find the checked file a dotted module name refers to.
///
/// An absolute import is resolved against the importer's own directory and the
/// ancestors above it, which is where `sys.path` must point for the import to
/// work at all, and failing that by path suffix, because the root of another
/// source tree is not known. A single-component import is only ever resolved
/// against those ancestors, since one filename matches at any depth; an
/// ambiguous suffix resolves to nothing rather than to a guess.
fn resolve_module(
    module: &str,
    level: u32,
    importer: &Path,
    known: &BTreeSet<&Path>,
) -> Option<PathBuf> {
    let parts: Vec<&str> = module.split('.').filter(|part| !part.is_empty()).collect();
    if level > 0 {
        let mut directory = importer.parent()?.to_path_buf();
        for _ in 1..level {
            if !directory.pop() {
                return None;
            }
        }
        for part in &parts {
            directory.push(part);
        }
        return module_candidate(&directory, known);
    }
    if parts.is_empty() {
        return None;
    }
    let explicit = resolve_from_pythonpath(&parts, known);
    // Whichever directory is on `sys.path`, a top-level import from this file
    // resolves at or above the file itself, so those are the roots to try —
    // and none of them can reach sideways into another subtree. A directory
    // that is a package of its own is not among them: Python has no implicit
    // relative imports, so `import utils` inside a package does not find the
    // package's own `utils`. Where more than one root answers, which is on
    // `sys.path` decides, and that is not knowable, so nothing does.
    let mut found: Option<PathBuf> = None;
    if let Some(mut directory) = importer.parent().map(Path::to_path_buf) {
        loop {
            if package_init(&directory).is_none() {
                let mut candidate = directory.clone();
                for part in &parts {
                    candidate.push(part);
                }
                if let Some(candidate) = module_candidate(&candidate, known) {
                    if found.as_ref().is_some_and(|first| *first != candidate) {
                        return None;
                    }
                    found = Some(candidate);
                }
            }
            if !directory.pop() {
                break;
            }
        }
    }
    // An import root given on `PYTHONPATH` is one more root that can answer,
    // so it is held to the same rule. Python searches the entry script's
    // directory ahead of `PYTHONPATH`, but which checked file is the entry
    // script is not knowable, so neither root can be declared the winner:
    // where the two name different files, the import resolves to nothing and
    // the call is left alone rather than rewritten against the wrong
    // definition.
    match (explicit, found) {
        (Some(explicit), Some(inferred)) if explicit != inferred => return None,
        (Some(path), _) | (None, Some(path)) => return Some(path),
        (None, None) => {}
    }
    // Elsewhere in the tree, a dotted import is still matched by path suffix,
    // because the import root of another source tree is not knowable. A
    // single-component import is not: its suffix is one filename, so it would
    // match `anything/at/any/depth/utils.py`, which is not what `import utils`
    // resolves to under any `sys.path` the tree implies. Two or more
    // components are evidence enough, and an ambiguous suffix still resolves
    // to nothing.
    if parts.len() < 2 {
        return None;
    }
    let matches = |suffix: &[String], path: &Path| {
        let components: Vec<String> = path
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        components.len() >= suffix.len() && components[components.len() - suffix.len()..] == *suffix
    };
    for last in [
        "__init__.py".to_owned(),
        format!("{}.py", parts[parts.len() - 1]),
    ] {
        let mut suffix: Vec<String> = parts.iter().map(|part| (*part).to_owned()).collect();
        if last == "__init__.py" {
            suffix.push(last);
        } else {
            let index = suffix.len() - 1;
            suffix[index] = last;
        }
        let mut found = known.iter().filter(|path| matches(&suffix, path));
        if let (Some(path), None) = (found.next(), found.next()) {
            return Some((*path).to_path_buf());
        }
    }
    None
}

/// Resolve a module against the explicit import roots on `PYTHONPATH`.
///
/// These roots reach a one-component module even where it lives below a
/// directory that otherwise looks like a package, which the roots inferred
/// from the checked tree do not. They do not outrank those inferred roots: an
/// import that both answer with different files is ambiguous, and the caller
/// resolves it to nothing.
fn resolve_from_pythonpath(parts: &[&str], known: &BTreeSet<&Path>) -> Option<PathBuf> {
    let python_path = std::env::var_os("PYTHONPATH")?;
    let mut found = None;
    for root in std::env::split_paths(&python_path) {
        let mut base = root;
        for part in parts {
            base.push(part);
        }
        for candidate in [base.join("__init__.py"), base.with_extension("py")] {
            let candidate = if known.contains(candidate.as_path()) {
                candidate
            } else {
                std::fs::canonicalize(&candidate).unwrap_or(candidate)
            };
            if known.contains(candidate.as_path()) {
                if found.as_ref().is_some_and(|first| *first != candidate) {
                    return None;
                }
                found = Some(candidate);
                break;
            }
        }
    }
    found
}

/// Resolve one importable path, matching Python's preference for a package
/// directory over a same-named source module in the same search location.
fn module_candidate(base: &Path, known: &BTreeSet<&Path>) -> Option<PathBuf> {
    [base.join("__init__.py"), base.with_extension("py")]
        .into_iter()
        .find(|candidate| known.contains(candidate.as_path()))
}

/// Derive the package bound by `import a.b` from the file already selected for
/// the complete dotted module. Resolving `a` again can choose a different
/// search root from the one that supplied `a.b`.
fn top_package_of_resolved(
    module: &str,
    resolved: &Path,
    known: &BTreeSet<&Path>,
) -> Option<PathBuf> {
    let parts = module.split('.').filter(|part| !part.is_empty()).count();
    if parts < 2 {
        return None;
    }
    let initializer = matches!(
        resolved.file_name().and_then(|name| name.to_str()),
        Some("__init__.py" | "__init__.pyi")
    );
    let mut directory = resolved.parent()?.to_path_buf();
    let ascents = parts - 2 + usize::from(initializer);
    for _ in 0..ascents {
        if !directory.pop() {
            return None;
        }
    }
    package_init(&directory).filter(|path| known.contains(path.as_path()))
}

#[derive(Serialize)]
struct JsonDiagnostic<'a> {
    path: String,
    line: usize,
    column: usize,
    code: &'static str,
    message: &'a str,
}

/// A file's text with the offset each of its lines starts at, so quoting a line
/// in `full` output costs neither a reread nor a walk from the top of the file.
struct SourceLines {
    text: String,
    starts: Vec<usize>,
}

fn source_line_starts(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                starts.push(index + 2);
                index += 2;
            }
            b'\r' | b'\n' => {
                starts.push(index + 1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    starts
}

fn previous_line_start(source: &str, offset: usize) -> usize {
    source[..offset]
        .rfind(['\n', '\r'])
        .map_or(0, |end| end + 1)
}

fn next_line_break(source: &str, offset: usize) -> usize {
    source[offset..]
        .find(['\n', '\r'])
        .map_or(source.len(), |end| offset + end)
}

fn line_break_end(source: &str, start: usize) -> usize {
    match source.as_bytes().get(start..) {
        Some([b'\r', b'\n', ..]) => start + 2,
        Some([b'\r' | b'\n', ..]) => start + 1,
        _ => start,
    }
}

impl SourceLines {
    fn read(path: &Path) -> Option<Self> {
        let text = read_source(path).ok()?;
        // An empty file has no lines at all, so it gets no first line either.
        let starts = if text.is_empty() {
            Vec::new()
        } else {
            source_line_starts(&text)
                .into_iter()
                .filter(|start| *start < text.len())
                .collect()
        };
        Some(Self { text, starts })
    }

    /// The one-based `number`th line, without its line break.
    fn line(&self, number: usize) -> Option<&str> {
        let start = *self.starts.get(number.checked_sub(1)?)?;
        let end = self.starts.get(number).copied().unwrap_or(self.text.len());
        Some(self.text[start..end].trim_end_matches(['\n', '\r']))
    }
}

fn caret_padding(line: &str, character_column: usize, gutter_width: usize) -> String {
    let mut terminal_column = gutter_width + 3;
    let mut padding = String::new();
    for character in line.chars().take(character_column.saturating_sub(1)) {
        let width = if character == '\t' {
            8 - terminal_column % 8
        } else {
            character.width().unwrap_or(0)
        };
        padding.extend(std::iter::repeat_n(' ', width));
        terminal_column += width;
    }
    padding
}

fn escaped_source_line(line: &str) -> String {
    line.chars()
        .flat_map(|character| {
            // A tab is laid out by `caret_padding`, which follows tab stops,
            // so it stays as it is. Every other control character would move
            // the cursor or colour what follows, and is written out instead.
            if character.is_control() && character != '\t' {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

fn report_diagnostics(
    diagnostics: &[Diagnostic],
    format: OutputFormat,
    include_summary: bool,
) -> Result<(), String> {
    match format {
        OutputFormat::Full => {
            // Diagnostics are sorted by path, so one file at a time is read
            // and indexed. Rereading and rewalking per diagnostic made this
            // quadratic in the violations a file holds, and `full` is the
            // default format.
            let mut quoted: Option<(&Path, Option<SourceLines>)> = None;
            for diagnostic in diagnostics {
                println!(
                    "{}:{}:{}: {} {}",
                    diagnostic.path.display(),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.code,
                    diagnostic.message
                );
                let path = diagnostic.path.as_path();
                if quoted.as_ref().is_none_or(|(cached, _)| *cached != path) {
                    quoted = Some((path, SourceLines::read(path)));
                }
                let Some((_, Some(source))) = &quoted else {
                    continue;
                };
                let Some(line) = source.line(diagnostic.line) else {
                    continue;
                };
                let width = diagnostic.line.to_string().len();
                let escaped = escaped_source_line(line);
                let prefix = line
                    .chars()
                    .take(diagnostic.column.saturating_sub(1))
                    .collect::<String>();
                let escaped_column = escaped_source_line(&prefix).chars().count() + 1;
                let padding = caret_padding(&escaped, escaped_column, width);
                println!("{space:width$} |", space = "", width = width);
                println!("{} | {escaped}", diagnostic.line);
                println!("{space:width$} | {padding}^", space = "", width = width);
                println!("{space:width$} |", space = "", width = width);
            }
        }
        OutputFormat::Concise => {
            for diagnostic in diagnostics {
                println!(
                    "{}:{}:{}: {} {}",
                    diagnostic.path.display(),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.code,
                    diagnostic.message
                );
            }
        }
        OutputFormat::Json => {
            let json = diagnostics
                .iter()
                .map(|diagnostic| JsonDiagnostic {
                    path: diagnostic.path.display().to_string(),
                    line: diagnostic.line,
                    column: diagnostic.column,
                    code: diagnostic.code,
                    message: &diagnostic.message,
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::to_string_pretty(&json).map_err(|error| error.to_string())?
            );
            return Ok(());
        }
        OutputFormat::Github => {
            for diagnostic in diagnostics {
                println!(
                    "::error file={},line={},col={},title={}::{}",
                    github_escape_property(&diagnostic.path.display().to_string()),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.code,
                    github_escape_message(&diagnostic.message)
                );
            }
            return Ok(());
        }
    }
    if include_summary && !diagnostics.is_empty() {
        println!(
            "Found {} error{}.",
            diagnostics.len(),
            if diagnostics.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn github_escape_message(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn github_escape_property(value: &str) -> String {
    github_escape_message(value)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

struct FileSettings {
    project_root: PathBuf,
    /// `None` when the file is exempt from the rule entirely.
    private_only: Option<bool>,
    respect_reexports: bool,
    /// Empty unless the file is checked in private-only mode with
    /// `respect_reexports`, which is the only combination that consults it.
    reexports: Arc<Reexports>,
    /// The base classes whose subclasses carry fields.
    field_bases: Arc<FieldBases>,
}

fn settings_for_files(
    files: &[PathBuf],
    cli_private_only: bool,
    cli_respect_reexports: bool,
) -> Result<Vec<FileSettings>, String> {
    let fallback_root = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut directory_cache: BTreeMap<PathBuf, Option<PathBuf>> = BTreeMap::new();
    let mut config_cache: BTreeMap<PathBuf, LoadedConfig> = BTreeMap::new();
    let mut reexport_cache: BTreeMap<(PathBuf, PathBuf), PackageReexports> = BTreeMap::new();
    let fallback_field_bases = Arc::new(FieldBases::new(&default_field_base_classes()));
    let fallback_overrides = Arc::new(Vec::new());
    let mut settings = Vec::with_capacity(files.len());
    for path in files {
        let config_path = discover_config_path(path, &mut directory_cache)?;
        let loaded = if let Some(config_path) = config_path {
            if !config_cache.contains_key(&config_path) {
                let loaded = load_config_path(&config_path)?;
                config_cache.insert(config_path.clone(), loaded);
            }
            config_cache
                .get(&config_path)
                .cloned()
                .ok_or_else(|| format!("configuration cache lost {}", config_path.display()))?
        } else {
            LoadedConfig {
                root: fallback_root.clone(),
                config: Config::default(),
                field_bases: Arc::clone(&fallback_field_bases),
                overrides: Arc::clone(&fallback_overrides),
            }
        };
        let private_only =
            private_only_for(path, &loaded).map(|private_only| cli_private_only || private_only);
        let respect_reexports = cli_respect_reexports || loaded.config.respect_reexports;
        // Nothing else consults the package's `__init__.py` files, so nothing
        // else pays for reading them.
        let reexports = if respect_reexports && private_only == Some(true) {
            let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let directory = resolved.parent().unwrap_or(Path::new("."));
            package_reexports(directory, &resolved, &loaded.root, &mut reexport_cache)?.names
        } else {
            Arc::new(Reexports::default())
        };
        settings.push(FileSettings {
            project_root: loaded.root.clone(),
            private_only,
            respect_reexports,
            reexports,
            field_bases: Arc::clone(&loaded.field_bases),
        });
    }
    Ok(settings)
}

/// What one package publishes, cached so each `__init__.py` is read once
/// however many directories below it are checked.
#[derive(Clone, Default)]
struct PackageReexports {
    names: Arc<Reexports>,
    /// Whether a private package stands between this one and the outside. Its
    /// own `__init__.py` then publishes nothing, and neither does any package
    /// below it, however public their names are.
    sealed: bool,
}

/// The names the `__init__.py` files above a file re-export.
///
/// Each directory's answer is its parent's plus its own `__init__.py`. A
/// directory without one holds no names of its own but is still a link in the
/// chain, because a namespace package under a regular one is imported through
/// it, so the walk climbs to the project root rather than stopping at the
/// first missing `__init__.py`. Outside the root — a file checked from an
/// unrelated working directory — it follows the package chain as far as that
/// reaches instead.
///
/// A private package seals what is below it, because a name re-exported by
/// `_internal/__init__.py` is no more reachable from outside than the module
/// it came from — unless the package above re-exports `_internal` itself,
/// which puts it back within reach.
fn package_reexports(
    directory: &Path,
    target: &Path,
    root: &Path,
    cache: &mut BTreeMap<(PathBuf, PathBuf), PackageReexports>,
) -> Result<PackageReexports, String> {
    let key = (directory.to_path_buf(), target.to_path_buf());
    if let Some(cached) = cache.get(&key) {
        return Ok(cached.clone());
    }
    let init = package_init(directory);
    let climbs = directory != root && (directory.starts_with(root) || init.is_some());
    let inherited = match directory.parent().filter(|_| climbs) {
        Some(parent) => package_reexports(parent, target, root, cache)?,
        None => PackageReexports::default(),
    };
    let name = directory.file_name().unwrap_or_default().to_string_lossy();
    // The directory the walk stops at is not reached by an import, so its own
    // name says nothing about what is inside it.
    let sealed = inherited.sealed
        || (climbs
            && is_private(&name)
            && !inherited.names.module
            && !inherited.names.covers(&name));
    let package = match init {
        Some(init) if !sealed => {
            let mut names = (*inherited.names).clone();
            collect_reexports_for_target(&init, target, root, &mut names)?;
            PackageReexports {
                names: Arc::new(names),
                sealed,
            }
        }
        _ => PackageReexports {
            names: inherited.names,
            sealed,
        },
    };
    cache.insert(key, package.clone());
    Ok(package)
}

/// The file that makes a directory a package, which is a stub in a stub-only
/// distribution.
fn package_init(directory: &Path) -> Option<PathBuf> {
    ["__init__.py", "__init__.pyi"]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
fn collect_reexports(path: &Path, reexports: &mut Reexports) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let parsed = parse_module(&source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let mut collector = ReexportCollector {
        reexports,
        bindings: BTreeSet::new(),
        all_names: BTreeSet::new(),
    };
    for statement in parsed.suite() {
        collector.visit_stmt(statement);
    }
    collector.finish();
    Ok(())
}

#[derive(Debug)]
enum TargetExport {
    Symbol(String),
    Module,
    Wildcard,
}

struct TargetReexportCollector<'a> {
    init: &'a Path,
    target: PathBuf,
    root: &'a Path,
    imports: Vec<(String, TargetExport)>,
    all_names: BTreeSet<String>,
}

impl TargetReexportCollector<'_> {
    fn collect_conditional(&mut self, branch: &ast::StmtIf) {
        let clauses = std::iter::once((Some(branch.test.as_ref()), branch.body.as_slice())).chain(
            branch
                .elif_else_clauses
                .iter()
                .map(|clause| (clause.test.as_ref(), clause.body.as_slice())),
        );
        for (test, body) in clauses {
            let truth = test.map_or(Truthiness::True, |test| {
                Truthiness::from_expr(test, |_| false)
            });
            match truth {
                Truthiness::False | Truthiness::Falsey | Truthiness::None => {}
                Truthiness::True | Truthiness::Truthy => {
                    for statement in body {
                        self.visit_stmt(statement);
                    }
                    return;
                }
                Truthiness::Unknown => {
                    for statement in body {
                        self.visit_stmt(statement);
                    }
                }
            }
        }
    }

    fn collect_assignment_export(&mut self, assign: &ast::StmtAssign) {
        let alias = match assign.value.as_ref() {
            Expr::Attribute(attribute) => match attribute.value.as_ref() {
                Expr::Name(module)
                    if self.imports.iter().any(|(name, export)| {
                        name == module.id.as_str() && matches!(export, TargetExport::Module)
                    }) =>
                {
                    Some(attribute.attr.to_string())
                }
                _ => None,
            },
            _ => None,
        };
        for target in &assign.targets {
            let Expr::Name(bound) = target else {
                continue;
            };
            self.imports.retain(|(name, _)| name != bound.id.as_str());
            if let Some(symbol) = &alias {
                self.imports
                    .push((bound.id.to_string(), TargetExport::Symbol(symbol.clone())));
            }
        }
    }

    fn module_path(&self, module: &str, level: u32) -> PathBuf {
        let mut path = if level > 0 {
            let mut path = self.init.parent().unwrap_or(Path::new("")).to_path_buf();
            for _ in 1..level {
                path.pop();
            }
            path
        } else {
            self.root.to_path_buf()
        };
        for part in module.split('.').filter(|part| !part.is_empty()) {
            path.push(part);
        }
        path
    }

    fn is_target_module(&self, module: &Path) -> bool {
        self.target == module
    }

    fn target_is_inside(&self, module: &Path) -> bool {
        self.target == module || (module.is_dir() && self.target.starts_with(module))
    }

    fn collect_all(&mut self, value: &Expr, replace: bool) {
        if replace {
            self.all_names.clear();
        }
        let elements = match value {
            Expr::List(list) => &list.elts,
            Expr::Tuple(tuple) => &tuple.elts,
            _ => return,
        };
        for element in elements {
            if let Expr::StringLiteral(string) = element {
                self.all_names.insert(string.value.to_string());
            }
        }
    }

    fn delete(&mut self, target: &Expr) {
        let mut deleted = BoundNames::default();
        deleted.bind(target);
        self.imports
            .retain(|(bound, _)| !deleted.names.contains(bound));
        if deleted.names.contains("__all__") {
            self.all_names.clear();
        }
    }

    fn finish(self, reexports: &mut Reexports) {
        for (bound, export) in self.imports {
            match export {
                TargetExport::Symbol(name) => {
                    if !is_private(&bound) || self.all_names.contains(&bound) {
                        reexports.names.insert(name);
                    }
                }
                // Importing a module makes every attribute on it reachable
                // through the package, even when that module segment itself
                // starts with an underscore.
                TargetExport::Module => reexports.module = true,
                TargetExport::Wildcard => reexports.wildcard = true,
            }
        }
    }
}

impl<'a> Visitor<'a> for TargetReexportCollector<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let module = self.module_path(alias.name.as_str(), 0);
                    if self.target_is_inside(&module) {
                        let bound = alias.asname.as_ref().map_or_else(
                            || alias.name.split('.').next().unwrap_or_default().to_owned(),
                            ToString::to_string,
                        );
                        self.imports.push((bound, TargetExport::Module));
                    }
                }
            }
            Stmt::ImportFrom(import) => {
                let module = import.module.as_ref().map_or("", ast::Identifier::as_str);
                let origin = self.module_path(module, import.level);
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        if self.is_target_module(&origin) {
                            self.imports.push(("*".to_owned(), TargetExport::Wildcard));
                        }
                        continue;
                    }
                    let bound = alias
                        .asname
                        .as_ref()
                        .map_or_else(|| alias.name.to_string(), ToString::to_string);
                    if self.is_target_module(&origin) {
                        self.imports
                            .push((bound, TargetExport::Symbol(alias.name.to_string())));
                        continue;
                    }
                    let submodule = origin.join(alias.name.as_str());
                    if self.target_is_inside(&submodule) {
                        self.imports.push((bound, TargetExport::Module));
                    }
                }
            }
            Stmt::Assign(assign) if assign.targets.iter().any(is_dunder_all) => {
                self.collect_all(&assign.value, true);
            }
            Stmt::Assign(assign) => self.collect_assignment_export(assign),
            Stmt::AnnAssign(assign) if is_dunder_all(&assign.target) => {
                if let Some(value) = assign.value.as_deref() {
                    self.collect_all(value, true);
                }
            }
            Stmt::AugAssign(assign) if is_dunder_all(&assign.target) => {
                self.collect_all(&assign.value, false);
            }
            Stmt::Delete(delete) => {
                for target in &delete.targets {
                    self.delete(target);
                }
            }
            Stmt::If(branch) => self.collect_conditional(branch),
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            _ => walk_stmt(self, statement),
        }
    }
}

fn source_module_path(path: &Path) -> PathBuf {
    if path.file_stem().is_some_and(|stem| stem == "__init__") {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.with_extension("")
    }
}

fn collect_reexports_for_target(
    init: &Path,
    target: &Path,
    root: &Path,
    reexports: &mut Reexports,
) -> Result<(), String> {
    let source = std::fs::read_to_string(init)
        .map_err(|error| format!("could not read {}: {error}", init.display()))?;
    let parsed = parse_module(&source)
        .map_err(|error| format!("could not parse {}: {error}", init.display()))?;
    let mut collector = TargetReexportCollector {
        init,
        target: source_module_path(target),
        root,
        imports: Vec::new(),
        all_names: BTreeSet::new(),
    };
    for statement in parsed.suite() {
        collector.visit_stmt(statement);
    }
    collector.finish(reexports);
    Ok(())
}

/// Gathers what an `__init__.py` binds: imported names, and the strings of a
/// literal `__all__`.
#[cfg(test)]
struct ReexportCollector<'a> {
    reexports: &'a mut Reexports,
    bindings: BTreeSet<String>,
    all_names: BTreeSet<String>,
}

#[cfg(test)]
impl ReexportCollector<'_> {
    /// Add the entries of an `__all__` that is written out as a list or tuple
    /// of string literals. One computed from a call or another module's
    /// `__all__` says nothing this run can read.
    fn collect_all(&mut self, value: &Expr, replace: bool) {
        if replace {
            self.all_names.clear();
        }
        let elements = match value {
            Expr::List(list) => &list.elts,
            Expr::Tuple(tuple) => &tuple.elts,
            _ => return,
        };
        for element in elements {
            if let Expr::StringLiteral(string) = element {
                self.all_names.insert(string.value.to_string());
            }
        }
    }

    fn finish(&mut self) {
        self.reexports
            .names
            .extend(self.all_names.intersection(&self.bindings).cloned());
    }
}

fn is_dunder_all(target: &Expr) -> bool {
    matches!(target, Expr::Name(name) if name.id.as_str() == "__all__")
}

#[cfg(test)]
impl<'a> Visitor<'a> for ReexportCollector<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        self.reexports.wildcard = true;
                    } else {
                        let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                        self.bindings.insert(bound.to_string());
                        self.reexports.names.insert(bound.to_string());
                        if alias.asname.is_some() && !is_private(bound.as_str()) {
                            // The public alias exposes the source symbol even
                            // when its defining name is private.
                            self.reexports.names.insert(alias.name.to_string());
                            if import.level > 0 && import.module.is_none() {
                                // `from . import _api as public` exposes the
                                // imported module and therefore its contents.
                                self.reexports.wildcard = true;
                            }
                        }
                    }
                }
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    // `import a.b` binds `a`; `import a.b as c` binds `c`.
                    let bound = alias.asname.as_ref().map_or_else(
                        || alias.name.split('.').next().unwrap_or_default().to_owned(),
                        ToString::to_string,
                    );
                    self.bindings.insert(bound.clone());
                    self.reexports.names.insert(bound);
                }
            }
            Stmt::Assign(assign) if assign.targets.iter().any(is_dunder_all) => {
                self.collect_all(&assign.value, true);
            }
            Stmt::AnnAssign(assign) if is_dunder_all(&assign.target) => {
                if let Some(value) = assign.value.as_deref() {
                    self.collect_all(value, true);
                }
            }
            Stmt::AugAssign(assign) if is_dunder_all(&assign.target) => {
                self.collect_all(&assign.value, false);
            }
            Stmt::FunctionDef(function) => {
                // The name is a module binding, but the body is another scope.
                self.bindings.insert(function.name.to_string());
            }
            Stmt::ClassDef(class) => {
                // Class-body imports become attributes, not package exports.
                self.bindings.insert(class.name.to_string());
            }
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    if let Expr::Name(name) = target {
                        self.bindings.insert(name.id.to_string());
                    }
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Expr::Name(name) = assign.target.as_ref() {
                    self.bindings.insert(name.id.to_string());
                }
            }
            // Imports guarded by `try` or `if TYPE_CHECKING` re-export just as
            // much as ones at the top level.
            _ => walk_stmt(self, statement),
        }
    }
}

fn discover_config_path(
    file: &Path,
    cache: &mut BTreeMap<PathBuf, Option<PathBuf>>,
) -> Result<Option<PathBuf>, String> {
    let absolute = std::fs::canonicalize(file)
        .map_err(|error| format!("could not resolve {}: {error}", file.display()))?;
    let mut directory = absolute.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut visited = Vec::new();
    let found = loop {
        if let Some(cached) = cache.get(&directory) {
            break cached.clone();
        }
        visited.push(directory.clone());
        let candidate = directory.join("pyproject.toml");
        if candidate.is_file() && pyproject_has_config(&candidate)? {
            break Some(candidate);
        }
        if !directory.pop() {
            break None;
        }
    };
    for directory in visited {
        cache.insert(directory, found.clone());
    }
    Ok(found)
}

fn pyproject_has_config(path: &Path) -> Result<bool, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    Ok(value
        .get("tool")
        .and_then(|tool| tool.get("no_defaults"))
        .is_some())
}

fn load_config_path(path: &Path) -> Result<LoadedConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let table = value
        .get("tool")
        .and_then(|tool| tool.get("no_defaults"))
        .cloned()
        .ok_or_else(|| format!("{} does not contain [tool.no_defaults]", path.display()))?;
    let config: Config = table
        .try_into()
        .map_err(|error| format!("invalid [tool.no_defaults] in {}: {error}", path.display()))?;
    Ok(LoadedConfig {
        root: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        field_bases: Arc::new(FieldBases::new(&config.field_base_classes)),
        overrides: Arc::new(compile_overrides(&config.per_file_enforcement)?),
        config,
    })
}

/// The rewritten source of every file an edit touches, and the number of call
/// sites actually rewritten.
fn fixed_sources(
    edits: BTreeMap<PathBuf, Vec<Edit>>,
    updated: &mut usize,
    unfixed: &mut BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, (String, String)>, String> {
    let mut changes = BTreeMap::new();
    for (path, edits) in edits {
        let source = read_source(&path)?;
        let (fixed, applied) = apply_edits(&source, edits);
        // A result that will not parse means this linter has a bug. The file
        // is left exactly as it was and said so about, loudly, rather than
        // holding back every other file's fix: one file it cannot handle used
        // to block fixing a whole project, with no remedy but to find it and
        // exclude it.
        if let Err(error) = parse_module(&fixed) {
            eprintln!(
                "warning: left {} unfixed: the result would not have parsed ({error}). \
                 This is a bug in no-defaults; please report it.",
                path.display()
            );
            unfixed.insert(path);
            continue;
        }
        *updated += applied;
        changes.insert(path, (source, fixed));
    }
    Ok(changes)
}

/// Apply edits from the end of the file backwards so earlier offsets stay
/// valid, returning the result and how many insertions survived.
///
/// A call site nested inside a default that is being deleted would otherwise be
/// rewritten into text that no longer exists, so those insertions are dropped
/// and are not counted as call sites updated.
fn apply_edits(source: &str, edits: Vec<Edit>) -> (String, usize) {
    let (deletions, mut insertions): (Vec<Edit>, Vec<Edit>) = edits
        .into_iter()
        .partition(|edit| edit.replacement.is_empty());
    let deletions = merge_deletions(deletions.into_iter().map(|edit| edit.range).collect());
    insertions.retain(|edit| {
        !deletions
            .iter()
            .any(|deletion| deletion.contains(edit.range.start()))
    });
    let applied = insertions
        .iter()
        .map(|edit| edit.site)
        .collect::<BTreeSet<_>>()
        .len();
    let mut edits: Vec<Edit> = insertions
        .into_iter()
        .chain(deletions.into_iter().map(Edit::deletion))
        .collect();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start()));
    let mut fixed = source.to_owned();
    for edit in edits {
        fixed.replace_range(
            edit.range.start().to_usize()..edit.range.end().to_usize(),
            &edit.replacement,
        );
    }
    (fixed, applied)
}

/// Combine deletions that overlap or touch into the disjoint ranges they cover.
///
/// Edits are applied from the end of the file backwards so that earlier offsets
/// stay valid, which only holds while no two of them overlap. Two overlapping
/// deletions applied in turn would have the later one address text the earlier
/// one had already removed — panicking when its end ran past what was left, and
/// quietly deleting the wrong span when it did not. This happens whenever an
/// unused `# noqa: NOD001` sits inside a multi-line default, because removing
/// the default already removes the directive.
///
/// Deleting the union of two overlapping deletions removes exactly what
/// deleting both was meant to.
fn merge_deletions(mut ranges: Vec<TextRange>) -> Vec<TextRange> {
    ranges.sort_by_key(|range| (range.start(), range.end()));
    let mut merged: Vec<TextRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start() <= last.end() => {
                *last = TextRange::new(last.start(), last.end().max(range.end()));
            }
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(unix)]
fn has_multiple_hard_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn has_multiple_hard_links(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn copy_extended_attributes(source: &Path, destination: &std::fs::File) -> Result<(), String> {
    use xattr::FileExt;

    let attributes = xattr::list(source).map_err(|error| {
        format!(
            "could not inspect extended attributes on {} before fixing: {error}",
            source.display()
        )
    })?;
    for name in attributes {
        let value = xattr::get(source, &name).map_err(|error| {
            format!(
                "could not read extended attribute {name:?} on {} before fixing: {error}",
                source.display()
            )
        })?;
        let Some(value) = value else { continue };
        if let Err(error) = destination.set_xattr(&name, &value) {
            if system_owned_attribute(&error) {
                continue;
            }
            return Err(format!(
                "could not preserve extended attribute {name:?} while fixing {}: {error}",
                source.display()
            ));
        }
    }
    Ok(())
}

/// Whether an extended attribute is the system's to write rather than ours.
///
/// macOS keeps names such as `com.apple.macl` and `com.apple.provenance` on
/// ordinary files and refuses to let a user process write them. Treating that
/// refusal as fatal would abandon the whole fix run over an attribute the fix
/// never touches, leaving the project unfixed.
#[cfg(target_os = "macos")]
fn system_owned_attribute(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
fn copy_extended_attributes(_: &Path, _: &std::fs::File) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_acl(source: &Path, destination: &Path) -> Result<(), String> {
    let entries = exacl::getfacl(source, None).map_err(|error| {
        format!(
            "could not inspect the access-control list on {} before fixing: {error}",
            source.display()
        )
    })?;
    exacl::setfacl(&[destination], &entries, None).map_err(|error| {
        format!(
            "could not preserve the access-control list while fixing {}: {error}",
            source.display()
        )
    })
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
fn copy_acl(_: &Path, _: &Path) -> Result<(), String> {
    Ok(())
}

fn write_fixes_atomically(changes: BTreeMap<PathBuf, (String, String)>) -> Result<(), String> {
    let mut prepared = Vec::with_capacity(changes.len());
    for (path, (_, fixed)) in changes {
        // `persist` replaces whatever sits at the path, so writing to a
        // symlink would leave a regular file where the link was and leave the
        // source it pointed at still holding its defaults. Resolving first
        // also puts the temporary file on the target's filesystem, which is
        // what makes the rename atomic.
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        // The source was read without its byte-order mark so that offsets
        // measured from the first real character; a file that had one keeps it.
        let fixed = if has_bom(&path) {
            format!("{BOM}{fixed}")
        } else {
            fixed
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let metadata = std::fs::metadata(&path).map_err(|error| {
            format!(
                "could not inspect {} before fixing: {error}",
                path.display()
            )
        })?;
        // Replacing one directory entry would silently detach it from every
        // other name for the same inode. Without knowing all those names, the
        // only safe atomic operation is to leave the linked file untouched.
        if has_multiple_hard_links(&metadata) {
            return Err(format!(
                "refusing to fix hard-linked file {} because atomic replacement would break its links",
                path.display()
            ));
        }
        let permissions = metadata.permissions();
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            format!(
                "could not create temporary file beside {}: {error}",
                path.display()
            )
        })?;
        temporary.write_all(fixed.as_bytes()).map_err(|error| {
            format!(
                "could not write temporary fix for {}: {error}",
                path.display()
            )
        })?;
        temporary.as_file().sync_all().map_err(|error| {
            format!(
                "could not sync temporary fix for {}: {error}",
                path.display()
            )
        })?;
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| {
                format!(
                    "could not preserve permissions while fixing {}: {error}",
                    path.display()
                )
            })?;
        copy_extended_attributes(&path, temporary.as_file())?;
        copy_acl(&path, temporary.path())?;
        prepared.push((path, temporary));
    }
    // Nothing reaches its destination until every file has been inspected and
    // its complete replacement has been created, synced, and configured. An
    // operational error during preparation therefore leaves the whole project
    // untouched instead of applying an arbitrary prefix of the changes.
    for (path, temporary) in prepared {
        temporary.persist(&path).map_err(|error| {
            format!(
                "could not atomically replace {}: {}",
                path.display(),
                error.error
            )
        })?;
    }
    Ok(())
}

fn print_diffs(changes: &BTreeMap<PathBuf, (String, String)>) {
    for (path, (source, fixed)) in changes {
        // Unified diff prefixes describe a synthetic tree, so an absolute
        // path must lose its root separator before it is placed below `a/`
        // or `b/`. Otherwise Unix paths become the malformed `a//...`.
        let path = path.to_string_lossy();
        let path = path.trim_start_matches(['/', '\\']);
        print!(
            "{}",
            TextDiff::from_lines(source, fixed)
                .unified_diff()
                .header(&format!("a/{path}"), &format!("b/{path}"))
        );
    }
}

fn compile_overrides(
    configured: &BTreeMap<String, Enforcement>,
) -> Result<Vec<PerFileEnforcement>, String> {
    configured
        .iter()
        .map(|(configured_pattern, enforcement)| {
            let (negated, pattern) = configured_pattern
                .strip_prefix('!')
                .map_or((false, configured_pattern.as_str()), |pattern| {
                    (true, pattern)
                });
            if pattern.is_empty() {
                return Err("per-file enforcement pattern must not be empty".to_owned());
            }
            let matcher = Glob::new(pattern)
                .map_err(|error| {
                    format!("invalid per-file enforcement pattern `{pattern}`: {error}")
                })?
                .compile_matcher();
            let specificity = pattern
                .chars()
                .filter(|character| !matches!(character, '*' | '?' | '[' | ']' | '{' | '}'))
                .count();
            Ok(PerFileEnforcement {
                matcher,
                negated,
                specificity,
                pattern: pattern.to_owned(),
                enforcement: *enforcement,
            })
        })
        .collect()
}

fn private_only_for(path: &Path, loaded: &LoadedConfig) -> Option<bool> {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = resolved.strip_prefix(&loaded.root).unwrap_or(&resolved);
    let filename = relative.file_name().unwrap_or_default();
    let selected = loaded
        .overrides
        .iter()
        .filter(|entry| {
            let matched = if entry.pattern.contains('/') {
                entry.matcher.is_match(relative)
            } else {
                entry.matcher.is_match(filename)
            };
            matched != entry.negated
        })
        .max_by(|left, right| {
            (left.specificity, &left.pattern).cmp(&(right.specificity, &right.pattern))
        })
        .map(|entry| entry.enforcement);
    match selected {
        Some(Enforcement::All) => Some(false),
        Some(Enforcement::Private) => Some(true),
        Some(Enforcement::None) => None,
        None => Some(loaded.config.private_only),
    }
}

fn collect_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            // Filtering a directory walk down to Python is what the walk is
            // for, but dropping a file the user named leaves a run that
            // checked nothing looking exactly like a clean one.
            if !is_python(path) {
                return Err(format!("not a Python file: {}", path.display()));
            }
            files.push(path.clone());
        } else if path.is_dir() {
            for entry in WalkBuilder::new(path).standard_filters(true).build() {
                let entry =
                    entry.map_err(|error| format!("could not walk {}: {error}", path.display()))?;
                // `Path::is_file` follows a file symlink even though the walk
                // deliberately does not follow directory symlinks.
                if entry.path().is_file() && is_python(entry.path()) {
                    files.push(entry.into_path());
                }
            }
        } else {
            return Err(format!("path does not exist: {}", path.display()));
        }
    }
    files.sort();
    // Two spellings of one path — `d.py` and `./d.py`, a relative and an
    // absolute form, or a symlink and its target — are one file, and checking
    // it twice inflates the diagnostic count and does the fixing work twice.
    // The first spelling in sorted order is kept, so what is reported stays
    // the path the user wrote and does not depend on argument order.
    let mut seen = BTreeSet::new();
    files.retain(|path| seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())));
    Ok(files)
}

fn is_python(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "py" || extension == "pyi")
}

/// Whether `path` is a type stub, whose contents describe an interface rather
/// than run.
fn is_stub(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "pyi")
}

/// The UTF-8 byte-order mark, which Windows editors write and which is not part
/// of the program.
const BOM: &str = "\u{feff}";

/// Read a checked file with any leading byte-order mark removed.
///
/// Measuring offsets from the mark would report every diagnostic on the first
/// line three columns too far right and misplace the caret. Every read of a
/// checked file goes through here, so an offset means the same thing to the
/// checker, the fixer, and the reporter; `write_fixes_atomically` puts the mark
/// back.
fn read_source(path: &Path) -> Result<String, String> {
    let mut source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if source.starts_with(BOM) {
        source.drain(..BOM.len());
    }
    Ok(source)
}

/// Whether a file starts with a byte-order mark, so fixing can preserve it.
fn has_bom(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes.starts_with(BOM.as_bytes()))
}

/// Lint Python source without filesystem or configuration-discovery overhead.
///
/// This is primarily exposed for performance benchmarks.
#[doc(hidden)]
pub fn lint_source(source: &str, private_only: bool) -> Result<usize, String> {
    Ok(check_source(
        Path::new("benchmark.py"),
        source,
        private_only,
        Path::new(""),
        &Reexports::default(),
        &FieldBases::new(&default_field_base_classes()),
        false,
    )
    .diagnostics
    .len())
}

fn check_file(
    path: &Path,
    private_only: bool,
    project_root: &Path,
    reexports: &Reexports,
    field_bases: &FieldBases,
    signatures: bool,
) -> Checked {
    let source = match read_source(path) {
        Ok(source) => source,
        Err(error) => return source_error(path, error),
    };
    check_source(
        path,
        &source,
        private_only,
        project_root,
        reexports,
        field_bases,
        signatures,
    )
}

/// The one diagnostic a source file that cannot be decoded or read produces.
fn source_error(path: &Path, error: String) -> Checked {
    Checked {
        diagnostics: vec![Diagnostic {
            path: path.to_path_buf(),
            line: 1,
            column: 1,
            code: "NOD000",
            message: error,
            fix: None,
        }],
        signatures: Vec::new(),
        skipped: Vec::new(),
    }
}

/// Note every module-level name defined more than once as a function.
///
/// A block that still binds module-level names — a conditional, a loop, a
/// `with`, a `try` — holds definitions that compete for the same name just as
/// two definitions written side by side do. Only one of them survives, and
/// which one is not knowable, so neither may have its defaults removed.
/// The local names that mean the `property` builtin: the name itself and
/// anything an import bound it to.
///
/// This runs before the alias table exists, so the import statements are read
/// directly. A module-qualified `builtins.property` needs no entry — the
/// attribute is matched by its own name.
fn property_alias_names(statements: &[Stmt], names: &mut BTreeSet<String>) {
    for statement in statements {
        match statement {
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.name.as_str() == "property" {
                        names.insert(
                            alias
                                .asname
                                .as_ref()
                                .map_or_else(|| alias.name.to_string(), ToString::to_string),
                        );
                    }
                }
            }
            Stmt::ClassDef(class) => property_alias_names(&class.body, names),
            Stmt::FunctionDef(function) => property_alias_names(&function.body, names),
            Stmt::If(branch) => {
                property_alias_names(&branch.body, names);
                for clause in &branch.elif_else_clauses {
                    property_alias_names(&clause.body, names);
                }
            }
            Stmt::For(loop_) => {
                property_alias_names(&loop_.body, names);
                property_alias_names(&loop_.orelse, names);
            }
            Stmt::While(loop_) => {
                property_alias_names(&loop_.body, names);
                property_alias_names(&loop_.orelse, names);
            }
            Stmt::With(block) => property_alias_names(&block.body, names),
            Stmt::Try(block) => {
                property_alias_names(&block.body, names);
                property_alias_names(&block.orelse, names);
                property_alias_names(&block.finalbody, names);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    property_alias_names(&handler.body, names);
                }
            }
            Stmt::Match(block) => {
                for case in &block.cases {
                    property_alias_names(&case.body, names);
                }
            }
            _ => {}
        }
    }
}

/// The methods a single class body hands to `property()` under another name,
/// including the ones written inside its control flow.
fn class_property_aliases(
    statements: &[Stmt],
    names: &BTreeSet<String>,
    found: &mut BTreeSet<(String, String)>,
) {
    for statement in statements {
        let value = match statement {
            Stmt::Assign(assign) => Some(assign.value.as_ref()),
            Stmt::AnnAssign(assign) => assign.value.as_deref(),
            Stmt::If(branch) => {
                class_property_aliases(&branch.body, names, found);
                for clause in &branch.elif_else_clauses {
                    class_property_aliases(&clause.body, names, found);
                }
                None
            }
            Stmt::For(loop_) => {
                class_property_aliases(&loop_.body, names, found);
                class_property_aliases(&loop_.orelse, names, found);
                None
            }
            Stmt::While(loop_) => {
                class_property_aliases(&loop_.body, names, found);
                class_property_aliases(&loop_.orelse, names, found);
                None
            }
            Stmt::With(block) => {
                class_property_aliases(&block.body, names, found);
                None
            }
            Stmt::Try(block) => {
                class_property_aliases(&block.body, names, found);
                class_property_aliases(&block.orelse, names, found);
                class_property_aliases(&block.finalbody, names, found);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    class_property_aliases(&handler.body, names, found);
                }
                None
            }
            Stmt::Match(block) => {
                for case in &block.cases {
                    class_property_aliases(&case.body, names, found);
                }
                None
            }
            _ => None,
        };
        let Some(Expr::Call(call)) = value else {
            continue;
        };
        let names_property = match call.func.as_ref() {
            Expr::Name(name) => names.contains(name.id.as_str()),
            Expr::Attribute(attribute) => attribute.attr.as_str() == "property",
            _ => false,
        };
        if !names_property || call.arguments.args.len() != 1 {
            continue;
        }
        if let Expr::Attribute(source) = &call.arguments.args[0] {
            if let Expr::Name(owner) = source.value.as_ref() {
                found.insert((owner.id.to_string(), source.attr.to_string()));
            }
        }
    }
}

/// Methods that some class body hands to `property()` under another name,
/// as `(class, method)`.
///
/// Attribute access runs a property's getter, so a default on the method
/// behind such an alias has no call site a fixer could update — and the class
/// holding the alias may be written after the one defining the method, so this
/// is gathered before checking starts.
fn collect_property_aliased_methods(
    statements: &[Stmt],
    names: &BTreeSet<String>,
    found: &mut BTreeSet<(String, String)>,
) {
    for statement in statements {
        match statement {
            Stmt::ClassDef(class) => {
                class_property_aliases(&class.body, names, found);
                collect_property_aliased_methods(&class.body, names, found);
            }
            Stmt::FunctionDef(function) => {
                collect_property_aliased_methods(&function.body, names, found);
            }
            Stmt::If(branch) => {
                collect_property_aliased_methods(&branch.body, names, found);
                for clause in &branch.elif_else_clauses {
                    collect_property_aliased_methods(&clause.body, names, found);
                }
            }
            Stmt::For(loop_) => {
                collect_property_aliased_methods(&loop_.body, names, found);
                collect_property_aliased_methods(&loop_.orelse, names, found);
            }
            Stmt::While(loop_) => {
                collect_property_aliased_methods(&loop_.body, names, found);
                collect_property_aliased_methods(&loop_.orelse, names, found);
            }
            Stmt::With(block) => collect_property_aliased_methods(&block.body, names, found),
            Stmt::Try(block) => {
                collect_property_aliased_methods(&block.body, names, found);
                collect_property_aliased_methods(&block.orelse, names, found);
                collect_property_aliased_methods(&block.finalbody, names, found);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_property_aliased_methods(&handler.body, names, found);
                }
            }
            Stmt::Match(block) => {
                for case in &block.cases {
                    collect_property_aliased_methods(&case.body, names, found);
                }
            }
            _ => {}
        }
    }
}

fn collect_repeated_functions(
    suite: &[Stmt],
    seen: &mut BTreeSet<String>,
    repeated: &mut BTreeSet<String>,
) {
    for statement in suite {
        match statement {
            Stmt::FunctionDef(function) => {
                let name = function.name.to_string();
                if !seen.insert(name.clone()) {
                    repeated.insert(name);
                }
            }
            Stmt::If(branch) => {
                collect_repeated_functions(&branch.body, seen, repeated);
                for clause in &branch.elif_else_clauses {
                    collect_repeated_functions(&clause.body, seen, repeated);
                }
            }
            Stmt::For(loop_statement) => {
                collect_repeated_functions(&loop_statement.body, seen, repeated);
                collect_repeated_functions(&loop_statement.orelse, seen, repeated);
            }
            Stmt::While(loop_statement) => {
                collect_repeated_functions(&loop_statement.body, seen, repeated);
                collect_repeated_functions(&loop_statement.orelse, seen, repeated);
            }
            Stmt::With(block) => collect_repeated_functions(&block.body, seen, repeated),
            Stmt::Try(block) => {
                collect_repeated_functions(&block.body, seen, repeated);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_repeated_functions(&handler.body, seen, repeated);
                }
                collect_repeated_functions(&block.orelse, seen, repeated);
                collect_repeated_functions(&block.finalbody, seen, repeated);
            }
            Stmt::Match(block) => {
                for case in &block.cases {
                    collect_repeated_functions(&case.body, seen, repeated);
                }
            }
            _ => {}
        }
    }
}

/// The one diagnostic a file the parser rejects produces.
///
/// It carries no fix: there is nothing to delete, and the file is left out of
/// the fixing pass entirely so a syntax error in one file cannot stop the rest
/// of the project from being fixed.
fn syntax_error(path: &Path, source: &str, error: &ruff_python_parser::ParseError) -> Checked {
    let (line, column) = line_column(source, error.location.start());
    Checked {
        diagnostics: vec![Diagnostic {
            path: path.to_path_buf(),
            line,
            column,
            code: "NOD000",
            message: format!("syntax error: {}", error.error),
            fix: None,
        }],
        signatures: Vec::new(),
        skipped: Vec::new(),
    }
}

/// Check one file. `signatures` records what each fixed callable looks like so
/// call sites can be updated; it costs allocation per parameter and per field,
/// so reporting runs leave it off.
fn check_source(
    path: &Path,
    source: &str,
    private_only: bool,
    project_root: &Path,
    reexports: &Reexports,
    field_bases: &FieldBases,
    signatures: bool,
) -> Checked {
    // A file the parser rejects is reported like any other finding and the run
    // carries on. A tree often holds something unparseable — a Python 2 file
    // kept for reference, a template saved as `.py`, a file being edited — and
    // aborting would hide every other file's diagnostics.
    let parsed = match parse_module(source) {
        Ok(parsed) => parsed,
        Err(error) => return syntax_error(path, source, &error),
    };
    let directives = collect_directives(source, parsed.tokens());
    // A blanket file-level directive silences every rule for the file,
    // including the unused-directive rule itself.
    if directives
        .iter()
        .any(|directive| directive.file_level && !directive.explicit)
    {
        return Checked::default();
    }
    let aliases = Aliases::default();
    let module_bindings = BoundNames::of_module(parsed.suite());
    let metaclass_intercepted_classes = metaclass_intercepted_classes(parsed.suite());
    let mut function_names = BTreeSet::new();
    let mut repeated_functions = BTreeSet::new();
    collect_repeated_functions(parsed.suite(), &mut function_names, &mut repeated_functions);
    let mut property_names = BTreeSet::from(["property".to_owned()]);
    property_alias_names(parsed.suite(), &mut property_names);
    let mut property_aliased_methods = BTreeSet::new();
    collect_property_aliased_methods(
        parsed.suite(),
        &property_names,
        &mut property_aliased_methods,
    );
    let mut checker = Checker {
        path,
        source,
        tokens: parsed.tokens(),
        private_only,
        reexports,
        field_bases,
        aliases,
        module_bindings,
        local_classes: BTreeSet::new(),
        metaclass_classes: BTreeSet::new(),
        entered_class_metaclass_classes: Vec::new(),
        metaclass_definitions: BTreeSet::new(),
        local_enum_classes: BTreeSet::new(),
        entered_class_enum_classes: Vec::new(),
        metaclass_intercepted_classes,
        repeated_functions,
        property_aliased_methods,
        base_field_classes: BTreeSet::new(),
        shapes: BTreeMap::new(),
        shape_namespaces: BTreeSet::new(),
        known_truthiness: BTreeMap::new(),
        rebound_globals: BTreeSet::new(),
        lexical_scope: Vec::new(),
        lexical_is_class: Vec::new(),
        lexical_bindings: Vec::new(),
        lambda_bodies: 0,
        conditional_depth: 0,
        scope: Scope {
            private: is_private_module(path, project_root, reexports),
            ..Scope::default()
        },
        header: None,
        collect_signatures: signatures,
        lines: LineIndex::new(source),
        classes: Vec::new(),
        class_constructs: Vec::new(),
        delegation_protocols: Vec::new(),
        attribute_interceptors: Vec::new(),
        instance_attributes: Vec::new(),
        class_deletions: Vec::new(),
        class_assignments: Vec::new(),
        class_rewraps: Vec::new(),
        method_aliases: Vec::new(),
        signatures: Vec::new(),
        skipped: Vec::new(),
        directives,
        diagnostics: Vec::new(),
    };
    for statement in parsed.suite() {
        checker.visit_stmt(statement);
    }
    checker.finish()
}

/// A `noqa` directive that suppresses this linter's rule.
///
/// Directives that cannot suppress `NOD001`, such as `# noqa: E501`, are not
/// collected at all.
struct Directive {
    /// Range of the line the directive sits on, excluding the line break.
    line: TextRange,
    /// Offset of the `#` that starts the directive.
    start: TextSize,
    /// Whether the directive names `NOD001` explicitly rather than blanketing.
    explicit: bool,
    /// Text to delete to drop `NOD001` from the directive.
    fix: TextRange,
    /// Whether the directive applies to the whole file.
    file_level: bool,
    /// Whether the directive suppressed at least one violation.
    used: bool,
}

/// Comments come from the parser, so a `#` inside a string is never mistaken
/// for a directive.
fn collect_directives(source: &str, tokens: &Tokens) -> Vec<Directive> {
    tokens
        .iter()
        .filter(|token| token.kind() == TokenKind::Comment)
        .filter_map(|token| parse_directive(source, token.start().to_usize()))
        .collect()
}

/// Strip a file-level directive prefix such as `ruff: noqa` from a lowercased
/// comment body, returning what follows it and how many bytes it consumed.
///
/// Ruff and flake8 both accept the form without a space after the colon, and
/// `# ruff:noqa` is common in the wild, so the space is optional here too.
fn file_level_prefix<'a>(body: &'a str, tool: &str) -> Option<(&'a str, usize)> {
    let rest = body
        .strip_prefix(tool)?
        .strip_prefix(':')?
        .trim_start_matches([' ', '\t'])
        .strip_prefix("noqa")?;
    Some((rest, body.len() - rest.len()))
}

/// The offset within a lowercased comment body of its `noqa` marker.
///
/// Ruff and flake8 both search the comment for the marker rather than requiring
/// it first, which is what lets a type-checker pragma and a lint suppression
/// share a line: `# type: ignore[misc]  # noqa: NOD001`. The marker has to be a
/// word of its own, so `noqattention` is not one.
fn noqa_marker(body: &str) -> Option<usize> {
    let boundary = |character: Option<char>| {
        character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
    };
    body.match_indices("noqa")
        .find(|(index, _)| {
            if !boundary(body[..*index].chars().next_back())
                || !boundary(body[index + "noqa".len()..].chars().next())
            {
                return false;
            }
            let rest = &body[index + "noqa".len()..];
            let segment_end = next_segment(rest).unwrap_or(rest.len());
            let segment = rest[..segment_end].trim();
            if segment.is_empty() {
                // A bare `noqa` suppresses everything, so it only counts when
                // it opens its own comment. Otherwise the word appearing in a
                // note after an unrelated directive — `# noqa: E501  keep
                // noqa` — would silence this rule.
                let preceding = &body[..*index];
                let segment_start = preceding.rfind('#').map_or(0, |offset| offset + 1);
                return preceding[segment_start..].trim().is_empty();
            }
            segment.strip_prefix(':').is_some_and(|codes| {
                codes.split(',').any(|part| {
                    part.split_whitespace()
                        .next()
                        .is_some_and(|code| code.eq_ignore_ascii_case("NOD001"))
                })
            })
        })
        .map(|(index, _)| index)
}

fn parse_directive(source: &str, hash: usize) -> Option<Directive> {
    let line_start = previous_line_start(source, hash);
    let break_start = next_line_break(source, hash);
    let content_end = break_start;
    let comment = source.get(hash + 1..content_end)?;
    let body = comment.trim_start();
    let body_start = hash + 1 + (comment.len() - body.len());
    let lower = body.to_ascii_lowercase();
    let alone = source[line_start..hash].trim().is_empty();
    let flake8 = alone.then(|| file_level_prefix(&lower, "flake8")).flatten();
    let ruff = alone.then(|| file_level_prefix(&lower, "ruff")).flatten();
    let (file_level, rest) = if let Some((rest, _)) = flake8 {
        // A `# flake8: noqa` with anything appended is not a directive.
        (rest.trim().is_empty(), None)
    } else if let Some((rest, consumed)) = ruff {
        (true, Some((rest, body_start + consumed)))
    } else {
        let marker = noqa_marker(&lower)? + "noqa".len();
        (false, Some((&lower[marker..], body_start + marker)))
    };
    // A `#` runs to end of line, so `# type: ignore[misc]  # noqa: NOD001` is
    // a single comment token. What the directive owns starts at its own `#`,
    // not at whichever one opened the comment.
    let directive_hash = rest.map_or(hash, |(_, rest_start)| {
        source[hash..rest_start]
            .rfind('#')
            .map_or(hash, |offset| hash + offset)
    });
    let line = TextRange::new(text_size(line_start), text_size(content_end));
    let blanket = || Directive {
        line,
        start: text_size(directive_hash),
        explicit: false,
        fix: TextRange::empty(text_size(directive_hash)),
        file_level,
        used: false,
    };
    let Some((rest, rest_start)) = rest else {
        // A `# flake8: noqa` with anything appended is not a directive.
        return file_level.then(blanket);
    };
    if rest.trim().is_empty() {
        return Some(blanket());
    }
    if next_segment(rest).is_some_and(|offset| rest[..offset].trim().is_empty()) {
        return Some(blanket());
    }
    let codes_start = rest_start + rest.len() - rest.trim_start().len();
    let codes = rest.trim_start().strip_prefix(':')?;
    let tokens = code_tokens(codes, codes_start + 1);
    // A code list that omits this rule cannot suppress it, so it is not a
    // directive this linter tracks.
    let index = tokens
        .iter()
        .position(|(_, code)| code.eq_ignore_ascii_case("NOD001"))?;
    let fix = if tokens.len() == 1 {
        // Anything after the code list on the line is another `#` segment —
        // a `# type: ignore`, a `# pylint: disable`, an explanation — and
        // taking the line to its end would delete it along with the directive.
        let after_codes = tokens[index].0.end().to_usize();
        next_segment(&source[after_codes..content_end]).map_or_else(
            || whole_directive_range(source, line_start, directive_hash, break_start, content_end),
            |offset| TextRange::new(text_size(directive_hash), text_size(after_codes + offset)),
        )
    } else {
        argument_removal_range(
            tokens[index].0,
            tokens.get(index + 1).map(|(range, _)| *range),
            index
                .checked_sub(1)
                .and_then(|previous| tokens.get(previous))
                .map(|(range, _)| *range),
        )
    };
    Some(Directive {
        explicit: true,
        fix,
        ..blanket()
    })
}

/// Where the next comment segment starts in `rest`, if it does.
///
/// A `#` runs to end of line, so `# noqa: NOD001  # type: ignore` is one
/// comment made of two segments. A `#` inside the prose of a segment does not
/// start a new one: an issue reference such as `see #35` is text, not a pragma,
/// and stopping there would leave a mangled fragment behind. A segment opens
/// with a `#` followed by whitespace, or by a pragma's `word:`.
fn next_segment(rest: &str) -> Option<usize> {
    rest.match_indices('#').find_map(|(offset, _)| {
        let after = &rest[offset + 1..];
        let opens = after
            .chars()
            .next()
            .is_none_or(|character| character.is_whitespace() || character == '#')
            || after.split_once(':').is_some_and(|(word, _)| {
                !word.is_empty()
                    && word
                        .chars()
                        .all(|character| character.is_alphanumeric() || character == '_')
            });
        opens.then_some(offset)
    })
}

/// The range to delete to remove a directive that names only `NOD001`.
///
/// A directive on its own line takes the line with it; a trailing directive
/// takes the whitespace that separated it from the code.
fn whole_directive_range(
    source: &str,
    line_start: usize,
    hash: usize,
    break_start: usize,
    content_end: usize,
) -> TextRange {
    if source[line_start..hash].trim().is_empty() {
        let line_end = line_break_end(source, break_start);
        return TextRange::new(text_size(line_start), text_size(line_end));
    }
    TextRange::new(
        text_size(line_start + source[line_start..hash].trim_end().len()),
        text_size(content_end),
    )
}

/// The codes of a `noqa` directive, with their offsets in the source.
fn code_tokens(codes: &str, codes_start: usize) -> Vec<(TextRange, &str)> {
    let mut tokens = Vec::new();
    let mut cursor = codes_start;
    for part in codes.split(',') {
        let trimmed = part.trim_start();
        let token = trimmed.split_whitespace().next().unwrap_or_default();
        if !token.is_empty() {
            let start = cursor + (part.len() - trimmed.len());
            tokens.push((
                TextRange::new(text_size(start), text_size(start + token.len())),
                token,
            ));
        }
        cursor += part.len() + 1;
    }
    tokens
}

fn text_size(value: usize) -> TextSize {
    TextSize::new(u32::try_from(value).unwrap_or(u32::MAX))
}

struct Checker<'a> {
    path: &'a Path,
    source: &'a str,
    tokens: &'a Tokens,
    private_only: bool,
    /// The names the enclosing package re-exports, which are public API
    /// whichever module they are defined in. Empty unless `respect_reexports`
    /// is on.
    reexports: &'a Reexports,
    /// The base classes whose subclasses carry fields, alongside `@dataclass`.
    field_bases: &'a FieldBases,
    /// What the file's renaming imports bound to `dataclasses` and `pydantic`
    /// members, so an aliased `@dataclass` is still recognised.
    aliases: Aliases,
    /// Names bound by the module itself, which therefore do not resolve to
    /// same-named built-ins.
    module_bindings: BTreeSet<String>,
    /// The class names the file defines, so a base written `Protocol` that is
    /// one of them is not mistaken for the typing construct.
    local_classes: BTreeSet<String>,
    /// Same-file classes whose construction a metaclass controls, directly or
    /// through a base. A metaclass is inherited, so a subclass is built by it
    /// too even when it names no `metaclass=` of its own.
    metaclass_classes: BTreeSet<String>,
    /// What `metaclass_classes` held as each enclosing class body was entered.
    /// A class body is not a closure scope, so a class the body writes stands
    /// for nothing inside a function or a class written there, which reads the
    /// names the class statement began with instead.
    entered_class_metaclass_classes: Vec<BTreeSet<String>>,
    /// Classes defined as subclasses of `type`, whose `__init__` and `mro`
    /// methods class creation invokes implicitly.
    metaclass_definitions: BTreeSet<String>,
    /// The enumerations the file defines, whether they name an imported `Enum`
    /// or another of these. Being an enumeration is inherited, so a subclass
    /// of one creates its members the same implicit way.
    local_enum_classes: BTreeSet<String>,
    /// What `local_enum_classes` held as each enclosing class body was
    /// entered, for the same reason `entered_class_metaclass_classes` keeps
    /// its own: a class body binds names only for itself, so a function or a
    /// class written in one reads the names the class statement began with.
    entered_class_enum_classes: Vec<BTreeSet<String>>,
    /// Same-file classes whose metaclass can replace class attributes.
    metaclass_intercepted_classes: BTreeSet<String>,
    /// Module-level functions defined more than once cannot share one safe
    /// call-site signature, so their defaults are reported but retained.
    repeated_functions: BTreeSet<String>,
    /// Methods another class aliases as a property, by `(class, method)`.
    property_aliased_methods: BTreeSet<(String, String)>,
    /// Classes already visited in this scope that carry fields through a
    /// configured base, so their local subclasses carry fields too.
    base_field_classes: BTreeSet<String>,
    /// What each field-carrying class of this file's own contributes to a
    /// subclass's constructor, by the name it was defined under.
    shapes: BTreeMap<String, Option<Shape>>,
    /// Module-level names rebound to namespace instances whose class members
    /// have known field shapes.
    shape_namespaces: BTreeSet<String>,
    /// Unconditional module assignments whose truth value is statically known.
    known_truthiness: BTreeMap<String, Truthiness>,
    /// Names some body has declared `global` and then rebound. What they were
    /// imported as is gone from the module namespace from then on, and unlike
    /// the shapes and the truthiness flags the alias table is saved and put
    /// back around every nested body, so the loss has to be recorded to
    /// outlive that.
    rebound_globals: BTreeSet<String>,
    /// Enclosing definitions, used to keep same-named nested class shapes
    /// separate from classes in other lexical scopes.
    lexical_scope: Vec<String>,
    /// Whether each entry of `lexical_scope` is a class body. A class
    /// namespace is not a closure scope, so code in a nested definition never
    /// reads names from it.
    lexical_is_class: Vec<bool>,
    /// The names each entry of `lexical_scope` binds. A scope that binds a
    /// name hides whatever the scopes outside it call by the same name, even
    /// where nothing about the class it now stands for is known.
    lexical_bindings: Vec<BTreeSet<String>>,
    /// How many lambda bodies enclose the expression being visited. A walrus
    /// in one binds in the lambda's own scope, which nothing outside it sees,
    /// and `lexical_scope` has no entry to say so.
    lambda_bodies: usize,
    /// Unknown control-flow branches do not describe one reliable constructor.
    conditional_depth: usize,
    scope: Scope,
    /// Start of the `def` or `class` line that owns the violations being
    /// reported, so one directive there can cover every parameter of a
    /// signature or every field of a dataclass.
    header: Option<TextSize>,
    /// Whether to record what each fixed callable looks like. Only `--fix` and
    /// `--diff` use it, and building it allocates per parameter and per field.
    collect_signatures: bool,
    /// Where each line of `source` starts, so reporting a diagnostic does not
    /// rescan the file from the top.
    lines: LineIndex,
    classes: Vec<ClassCollector>,
    class_constructs: Vec<bool>,
    /// Whether each enclosing class directly defines the iterator methods that
    /// make `yield from` forward protocol calls to it.
    delegation_protocols: Vec<bool>,
    /// Whether each enclosing class can replace ordinary method attributes.
    attribute_interceptors: Vec<bool>,
    /// Attributes visibly assigned on instances of each enclosing class.
    instance_attributes: Vec<BTreeSet<String>>,
    class_deletions: Vec<BTreeSet<String>>,
    class_assignments: Vec<BTreeMap<String, Vec<TextSize>>>,
    /// Class-body assignments that put the same function back under its own
    /// name, which replace nothing.
    class_rewraps: Vec<BTreeMap<String, Vec<TextSize>>>,
    /// Direct `alias = method` bindings for each enclosing class, keyed by the
    /// original method name.
    method_aliases: Vec<BTreeMap<String, Vec<MethodAlias>>>,
    signatures: Vec<Signature>,
    skipped: Vec<Skipped>,
    directives: Vec<Directive>,
    diagnostics: Vec<Diagnostic>,
}

/// What the enclosing definitions say about the statement being visited.
#[derive(Clone, Copy, Default)]
struct Scope {
    /// Whether an enclosing module, class, or function is private.
    private: bool,
    /// How assignments here declare fields, if they do at all.
    fields: Option<FieldStyle>,
    /// What kind of class body definitions here sit directly in, if any.
    class: ClassScope,
    /// Whether this is an `enum.Enum` body whose members are initialized
    /// implicitly while the class is created.
    enum_class: bool,
    /// Whether a field of this class has kept its default, which forces every
    /// field after it to keep its own: `dataclasses` rejects a field without a
    /// default following one with it.
    kept_default: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ClassScope {
    #[default]
    None,
    Ordinary,
    Metaclass,
}

/// What made a class carry fields, which is what its violations are called
/// after.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldStyle {
    /// A `@dataclass` decorator.
    Dataclass,
    /// A configured base class, such as `pydantic.BaseModel`.
    Base,
}

impl FieldStyle {
    fn noun(self) -> &'static str {
        match self {
            Self::Dataclass => "dataclass field",
            Self::Base => "class field",
        }
    }
}

/// The fields of a class body, gathered so a class that carries them can be
/// given a signature once its body has been walked.
struct ClassCollector {
    name: String,
    qualified: String,
    style: Option<FieldStyle>,
    /// Whether the class inherits fields this file cannot see, which makes its
    /// constructor unknown. A base that declares no fields does not count, nor
    /// does the base that made the class carry fields at all: `BaseModel`
    /// contributes none of its own. A base that is a dataclass in this file
    /// does not either — its fields are prepended instead.
    inherits: bool,
    /// Whether the decorator generates a constructor at all.
    constructs: bool,
    /// Whether every field from here on is keyword-only, because the decorator
    /// said `kw_only=True` or a `_: KW_ONLY` marker has been passed.
    kw_only: bool,
    /// The fields the constructor takes positionally, in order. A keyword-only
    /// field is left out: `dataclasses` moves it after the `*`, so it holds no
    /// position and every field after it in the source keeps the position the
    /// source suggests.
    fields: Vec<String>,
    removed: Vec<Removed>,
}

impl ClassCollector {
    fn normalize_repeated_fields(&mut self) {
        let mut fields = BTreeSet::new();
        self.fields.retain(|field| fields.insert(field.clone()));
        let mut removed = BTreeSet::new();
        self.removed.reverse();
        self.removed
            .retain(|item| removed.insert(item.parameter.clone()));
        self.removed.reverse();
    }
}

impl Checker<'_> {
    fn visit_import_statement<'a>(&mut self, statement: &'a Stmt)
    where
        Self: Visitor<'a>,
    {
        self.aliases.collect(std::slice::from_ref(statement));
        walk_stmt(self, statement);
    }

    /// What a class's bases contribute to its constructor.
    fn inherited_fields(&self, class: &ast::StmtClassDef, style: Option<FieldStyle>) -> Inherited {
        let carrying: Vec<&Expr> = class_bases(class)
            .filter(|base| {
                !carries_no_fields(
                    base,
                    &self.aliases,
                    &self.local_classes,
                    &self.module_bindings,
                )
            })
            // The base that made the class carry fields contributes none itself.
            .filter(|base| {
                !(matches!(style, Some(FieldStyle::Base))
                    && self.field_bases.matches(base, &self.aliases))
            })
            .collect();
        match carrying.as_slice() {
            [] => Inherited::Nothing,
            // `dataclasses` walks the reverse MRO to order the fields of several
            // bases, and writing them in the wrong order is worse than not writing
            // them, so one base is as far as this goes.
            [Expr::Name(name)] => {
                let qualified = qualified_class_name(&self.lexical_scope, name.id.as_str());
                match self.shapes.get(&qualified) {
                    // An unqualified name is the only form that can be tied to a
                    // class of this file's own. A name two classes share resolves
                    // to neither, and a base whose own constructor is unknown
                    // makes this one unknown too.
                    Some(Some(shape)) if shape.complete => Inherited::Known(shape.clone()),
                    _ => Inherited::Unknown,
                }
            }
            [Expr::Attribute(attribute)] => {
                let Expr::Name(module) = attribute.value.as_ref() else {
                    return Inherited::Unknown;
                };
                if !self.shape_namespaces.contains(module.id.as_str()) {
                    return Inherited::Unknown;
                }
                let qualified = format!("{}.{}", module.id, attribute.attr);
                match self.shapes.get(&qualified) {
                    Some(Some(shape)) if shape.complete => Inherited::Known(shape.clone()),
                    _ => Inherited::Unknown,
                }
            }
            _ => Inherited::Unknown,
        }
    }

    /// Record what a class contributes to a subclass's constructor. A name two
    /// classes in one scope share resolves to neither.
    fn record_shape(&mut self, name: String, shape: Shape) {
        match self.shapes.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(Some(shape));
            }
            Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    /// The class shape a bare name reaches from the scope being visited.
    ///
    /// Python resolves a free name against the enclosing functions and then
    /// the module. A class namespace is not one of those scopes, so a body
    /// written inside a class sees nothing an outer class body bound; only the
    /// scope the name is written in answers when that scope is a class body.
    ///
    /// The search stops at the nearest scope that binds the name, whether or
    /// not a shape was recorded there. A parameter or a rebinding stands
    /// between the name and the enclosing class it would otherwise have
    /// reached, and reading past it would give a subclass the fields of a
    /// class its constructor never sees.
    fn visible_shape(&self, name: &str) -> Option<&Option<Shape>> {
        (0..=self.lexical_scope.len())
            .rev()
            .filter(|depth| {
                *depth == self.lexical_scope.len()
                    || *depth == 0
                    || self.lexical_is_class.get(depth - 1) != Some(&true)
            })
            .find_map(|depth| {
                if let Some(shape) = self
                    .shapes
                    .get(&qualified_class_name(&self.lexical_scope[..depth], name))
                {
                    return Some(Some(shape));
                }
                let hidden = depth > 0
                    && self
                        .lexical_bindings
                        .get(depth - 1)
                        .is_some_and(|bindings| bindings.contains(name));
                hidden.then_some(None)
            })
            .flatten()
    }

    fn unknown_base_may_end_in_default(&self, class: &ast::StmtClassDef) -> bool {
        class_bases(class).any(|base| {
            // A parameter shadowing the import hides the base further rather
            // than revealing it, so the name it took over counts too.
            if base_root_name(base).is_some_and(|name| {
                self.aliases.import_bindings.contains(name)
                    || self.aliases.invalidated_import_bindings.contains(name)
            }) {
                return true;
            }
            // `Middle[int]` names the class `Middle` names, so a local base
            // that may end in a default is one whether or not the subclass
            // parameterizes it.
            let base = match base {
                Expr::Subscript(subscript) => subscript.value.as_ref(),
                expression => expression,
            };
            let Expr::Name(name) = base else {
                return false;
            };
            let qualified = qualified_class_name(&self.lexical_scope, name.id.as_str());
            self.shapes
                .get(&qualified)
                .and_then(Option::as_ref)
                .is_some_and(|shape| shape.kept_default)
        })
    }

    fn class_field_style(&self, class: &ast::StmtClassDef) -> Option<FieldStyle> {
        field_style(
            class,
            self.field_bases,
            &self.aliases,
            &self.base_field_classes,
            &self.module_bindings,
            &self.local_classes,
        )
    }

    fn record_base_field_class(&mut self, name: &str, style: Option<FieldStyle>) {
        if style == Some(FieldStyle::Base) {
            self.base_field_classes.insert(name.to_owned());
        } else {
            self.base_field_classes.remove(name);
        }
    }

    fn invalidate_statement_aliases(&mut self, statement: &Stmt) {
        let mut bound = BoundNames::default();
        match statement {
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    bound.bind(target);
                }
            }
            // `name: int` declares a type without rebinding anything, so the
            // alias, truthiness and dataclass shape already recorded for the
            // name still describe it.
            Stmt::AnnAssign(assign) => {
                if assign.value.is_some() {
                    bound.bind(&assign.target);
                }
            }
            Stmt::AugAssign(assign) => bound.bind(&assign.target),
            _ => return,
        }
        self.invalidate_bound_names(bound.names.iter().map(String::as_str));
        // An annotated assignment binds its target every bit as much as a plain
        // one does, so an annotation is no reason to forget what the name was
        // just given. Reading only plain assignments left `Alias: object = Base`
        // cleared above and never restored, handing every subclass of `Alias` an
        // unknown constructor.
        let assigned = match statement {
            Stmt::Assign(assign) => assign
                .targets
                .first()
                .filter(|_| assign.targets.len() == 1)
                .and_then(|target| match target {
                    Expr::Name(name) => Some((name, assign.value.as_ref())),
                    _ => None,
                }),
            Stmt::AnnAssign(assign) => match (assign.target.as_ref(), assign.value.as_deref()) {
                (Expr::Name(name), Some(value)) => Some((name, value)),
                _ => None,
            },
            _ => None,
        };
        if let Some((target, value)) = assigned {
            if self.conditional_depth == 0 && self.lexical_scope.is_empty() {
                let truth = Truthiness::from_expr(value, |_| false);
                if truth != Truthiness::Unknown {
                    self.known_truthiness.insert(target.id.to_string(), truth);
                }
            }
            let target = qualified_class_name(&self.lexical_scope, target.id.as_str());
            if let Expr::Name(value) = value {
                if let Some(shape) = self.visible_shape(value.id.as_str()).cloned() {
                    self.shapes.insert(target, shape);
                }
            } else if self.lexical_scope.is_empty() {
                if let Expr::Call(call) = value {
                    if let Expr::Name(namespace) = call.func.as_ref() {
                        let prefix = format!("{}.", namespace.id);
                        let aliases: Vec<(String, Option<Shape>)> = self
                            .shapes
                            .iter()
                            .filter_map(|(name, shape)| {
                                name.strip_prefix(&prefix)
                                    .map(|member| (format!("{target}.{member}"), shape.clone()))
                            })
                            .collect();
                        if !aliases.is_empty() {
                            self.shape_namespaces.insert(target.clone());
                        }
                        self.shapes.extend(aliases);
                    }
                }
            }
        }
    }

    /// Forget everything a name was known to stand for, because something
    /// else now stands there. A stale shape would write a rebound base's old
    /// fields into a subclass's constructor, and stale truthiness would take
    /// a branch the rebinding decided against.
    fn invalidate_bound_names<'n>(&mut self, names: impl IntoIterator<Item = &'n str>) {
        // Truthiness is recorded for module-level names and consulted only
        // where a module-level test can see them. A binding in a function or
        // class body names something else that leaves the module name alone,
        // so forgetting the flag there would make a later module branch
        // uncertain about a value still true when it runs.
        let rebinds_module_name =
            self.scope.class == ClassScope::None && self.lexical_scope.is_empty();
        for name in names {
            self.aliases.invalidate(name);
            if rebinds_module_name {
                self.known_truthiness.remove(name);
            }
            self.shapes
                .remove(&qualified_class_name(&self.lexical_scope, name));
            self.local_enum_classes.remove(name);
            if let Some(bindings) = self.lexical_bindings.last_mut() {
                bindings.insert(name.to_owned());
            }
        }
    }

    /// Forget everything recorded about every name `body` declares `global`
    /// and then binds, because such a binding lands in the module namespace
    /// and not in the body's own.
    ///
    /// A function body and a class body both need this, for opposite reasons.
    /// A function body may never run at all, so nothing there can be trusted
    /// to leave the module flag as it was; a class body always runs when the
    /// class statement does, so its rebinding definitely happens. Either way
    /// what a later module-level test would read is no longer the value
    /// recorded here, and a branch settled by the stale flag picked a base the
    /// subclass never had.
    ///
    /// The shape needs the same treatment for the same reason. A name bound
    /// inside a body ordinarily stands for something only that body can see,
    /// so shapes are keyed by the enclosing scope and the module entry rightly
    /// survives; `global` breaks that, and a module-level class built on the
    /// stale entry inherited fields the rebound base no longer has.
    ///
    /// So does what the name was imported as, except that the alias table is
    /// the one piece of this state a nested body saves and puts back. Dropping
    /// the name here is therefore not enough on its own, so the name is also
    /// recorded for `restore_aliases` to drop again; otherwise an imported
    /// `Protocol` or `ABC` came back as a structural base, and a dataclass
    /// built on the rebound name was called with only the fields its own body
    /// wrote.
    ///
    /// Only a binding that puts something behind the name counts. `global X`
    /// followed by `X: int` declares a type and assigns nothing, so the module
    /// name still stands for whatever it was imported or assigned as, and
    /// forgetting it there left a later subclass with no base it could name.
    fn forget_globals_rebound_in<'b>(&mut self, body: &'b [Stmt])
    where
        BoundNames: Visitor<'b>,
    {
        let mut declared = BoundNames::default();
        declared.visit_body(body);
        for name in declared.globals.intersection(&declared.names) {
            self.aliases.invalidate(name);
            self.known_truthiness.remove(name);
            self.shapes.remove(name);
            self.rebound_globals.insert(name.clone());
        }
    }

    /// Put back the alias table a nested body was entered with, minus every
    /// name a `global` rebinding has taken away in the meantime.
    ///
    /// A rebinding directly inside the body is forgotten before the table is
    /// saved, so the saved copy already lacks it. One a scope deeper is not:
    /// the enclosing body saved its table before the inner body was reached,
    /// so restoring it verbatim handed a later module-level subclass an
    /// import the module namespace no longer holds.
    fn restore_aliases(&mut self, outer: Aliases) {
        self.aliases = outer;
        for name in &self.rebound_globals {
            self.aliases.invalidate(name);
        }
    }

    fn invalidate_target_aliases(&mut self, target: &Expr) {
        let mut bound = BoundNames::default();
        bound.bind(target);
        self.invalidate_bound_names(bound.names.iter().map(String::as_str));
    }

    /// Forget what a walrus in a `def` or `class` header rebinds.
    ///
    /// A header is evaluated where the statement is written, not in the scope
    /// the statement opens: the decorators, the type parameters, a function's
    /// parameter defaults and annotations, and a class's bases all run in the
    /// enclosing namespace before there is anything to enter. The traversal
    /// reaches them only after that scope has been pushed, which recorded a
    /// module-level rebinding against the definition's own name and left the
    /// stale module shape to write a vanished base's fields into a subclass's
    /// constructor. The walk below stops at the body of a nested lambda, which
    /// binds its own names; the defaults of that lambda's parameters are
    /// evaluated in the header itself and count as part of it.
    fn invalidate_function_header(&mut self, function: &ast::StmtFunctionDef) {
        let mut bound = BoundNames::default();
        for decorator in &function.decorator_list {
            bound.visit_decorator(decorator);
        }
        if let Some(type_params) = &function.type_params {
            bound.visit_type_params(type_params);
        }
        bound.visit_parameters(&function.parameters);
        if let Some(returns) = &function.returns {
            bound.visit_annotation(returns);
        }
        self.invalidate_bound_names(bound.names.iter().map(String::as_str));
    }

    /// Forget what a walrus in a class header rebinds, as
    /// `invalidate_function_header` does for a `def`.
    fn invalidate_class_header(&mut self, class: &ast::StmtClassDef) {
        let mut bound = BoundNames::default();
        for decorator in &class.decorator_list {
            bound.visit_decorator(decorator);
        }
        if let Some(type_params) = &class.type_params {
            bound.visit_type_params(type_params);
        }
        if let Some(arguments) = &class.arguments {
            bound.visit_arguments(arguments);
        }
        self.invalidate_bound_names(bound.names.iter().map(String::as_str));
    }

    fn visit_loop<'a>(&mut self, loop_: &'a ast::StmtFor)
    where
        Self: Visitor<'a>,
    {
        let statically_empty = match loop_.iter.as_ref() {
            Expr::Tuple(tuple) => tuple.elts.is_empty(),
            Expr::List(list) => list.elts.is_empty(),
            Expr::Set(set) => set.elts.is_empty(),
            // `{}` is the empty mapping, and the only empty literal a set
            // cannot be written as.
            Expr::Dict(dict) => dict.items.is_empty(),
            _ => false,
        };
        if statically_empty {
            self.visit_body(&loop_.orelse);
        } else {
            // The iterable is evaluated before the target is assigned. The
            // body sees the target binding, and an import in that body may
            // establish the name that remains after an iteration.
            self.visit_expr(&loop_.iter);
            self.invalidate_target_aliases(&loop_.target);
            self.conditional_depth += 1;
            self.visit_expr(&loop_.target);
            self.visit_body(&loop_.body);
            self.visit_body(&loop_.orelse);
            self.conditional_depth -= 1;
        }
    }

    fn visit_with<'a>(&mut self, block: &'a ast::StmtWith)
    where
        Self: Visitor<'a>,
    {
        for item in &block.items {
            self.visit_expr(&item.context_expr);
            if let Some(target) = &item.optional_vars {
                self.visit_expr(target);
                self.invalidate_target_aliases(target);
            }
        }
        self.visit_body(&block.body);
    }

    fn visit_match<'a>(&mut self, block: &'a ast::StmtMatch)
    where
        Self: Visitor<'a>,
    {
        self.visit_expr(&block.subject);
        self.conditional_depth += 1;
        for case in &block.cases {
            self.visit_pattern(&case.pattern);
            let mut captures = BoundNames::default();
            captures.visit_pattern(&case.pattern);
            self.invalidate_bound_names(captures.names.iter().map(String::as_str));
            if let Some(guard) = &case.guard {
                self.visit_expr(guard);
            }
            self.visit_body(&case.body);
        }
        self.conditional_depth -= 1;
    }

    fn visit_while<'a>(&mut self, loop_: &'a ast::StmtWhile, statement: &'a Stmt)
    where
        Self: Visitor<'a>,
    {
        match Truthiness::from_expr(&loop_.test, |_| false) {
            Truthiness::False | Truthiness::Falsey | Truthiness::None => {
                self.visit_body(&loop_.orelse);
            }
            _ => self.visit_uncertain(statement),
        }
    }

    fn visit_uncertain<'a>(&mut self, statement: &'a Stmt)
    where
        Self: Visitor<'a>,
    {
        self.conditional_depth += 1;
        walk_stmt(self, statement);
        self.conditional_depth -= 1;
    }

    /// Whether a test is the `TYPE_CHECKING` guard, whose block a type checker
    /// reads but the interpreter never runs.
    fn is_type_checking(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Name(name) => self.aliases.type_checking.contains(name.id.as_str()),
            Expr::Attribute(attribute) if attribute.attr.as_str() == "TYPE_CHECKING" => {
                matches!(attribute.value.as_ref(), Expr::Name(module) if matches!(module.id.as_str(), "typing" | "typing_extensions") || self.aliases.typing_modules.contains(module.id.as_str()))
            }
            _ => false,
        }
    }

    fn class_constructs_safely(&self, class: &ast::StmtClassDef) -> bool {
        class_constructs_safely(class, &self.aliases, &self.metaclass_classes)
            && !self.inherits_unseen_import(class)
    }

    /// Whether a base of this class is an import whose fields, and whatever
    /// metaclass builds it, this file cannot see.
    ///
    /// A parameter shadowing the import hides the base further rather than
    /// revealing it, so the name it took over counts too.
    fn inherits_unseen_import(&self, class: &ast::StmtClassDef) -> bool {
        class_bases(class).any(|base| {
            base_root_name(base).is_some_and(|name| {
                self.aliases.import_bindings.contains(name)
                    || self.aliases.invalidated_import_bindings.contains(name)
            }) && !self.field_bases.matches(base, &self.aliases)
                && !carries_no_fields(
                    base,
                    &self.aliases,
                    &self.local_classes,
                    &self.module_bindings,
                )
        })
    }

    fn generates_init(&self, class: &ast::StmtClassDef) -> bool {
        generates_init(class, &self.aliases, &self.metaclass_classes)
    }

    /// Whether the class statement itself creates the class's members, which
    /// it does for an enumeration: each member assignment calls the body's
    /// initializer with the value assigned, through no call site the fixer can
    /// rewrite. A base written in this file that is already an enumeration
    /// makes this class one too, because that is inherited like any other
    /// class behaviour.
    fn is_enum_class(&self, class: &ast::StmtClassDef) -> bool {
        class_bases(class).any(|base| match base {
            Expr::Name(name) => {
                self.aliases.enum_classes.contains(name.id.as_str())
                    || self.local_enum_classes.contains(name.id.as_str())
            }
            Expr::Attribute(attribute) if attribute.attr.as_str() == "Enum" => {
                matches!(attribute.value.as_ref(), Expr::Name(module) if self.aliases.enum_modules.contains(module.id.as_str()))
            }
            _ => false,
        })
    }

    /// Note that a metaclass builds this class, so a later subclass of it is
    /// built by that metaclass too. An unseen imported base counts as one: the
    /// metaclass it may carry reaches every class beneath it alike, and a
    /// subclass written here sees no more of it than this class does. A later
    /// redefinition without either clears the name so a sibling that inherits
    /// the new class is not treated as metaclass-built.
    fn record_metaclass_construction(
        &mut self,
        class: &ast::StmtClassDef,
        unseen_import_base: bool,
    ) {
        if declares_metaclass(class)
            || inherits_metaclass(class, &self.metaclass_classes)
            || unseen_import_base
        {
            self.metaclass_classes.insert(class.name.to_string());
        } else {
            self.metaclass_classes.remove(class.name.as_str());
        }
    }

    fn visit_conditional<'a>(&mut self, branch: &'a ast::StmtIf)
    where
        Self: Visitor<'a>,
    {
        let clauses = std::iter::once((Some(branch.test.as_ref()), branch.body.as_slice())).chain(
            branch
                .elif_else_clauses
                .iter()
                .map(|clause| (clause.test.as_ref(), clause.body.as_slice())),
        );
        let mut uncertain = false;
        for (test, body) in clauses {
            // A clause's test is ordinary code that runs whenever the clause
            // is reached, so a default written in it — on a lambda, say — is
            // reported like any other. Tests after a statically true clause
            // are never reached, and the loop returns before them.
            if let Some(test) = test {
                self.visit_expr(test);
            }
            if test.is_some_and(|test| self.is_type_checking(test)) {
                // The block does not run, so an annotation in it declares
                // nothing the constructor takes. The imports in it still bind
                // the names the rest of the file is written against, so the
                // body is walked with field collection off rather than
                // skipped, and a later clause still runs.
                //
                // Nothing the block defines exists at runtime either, so a
                // class written inside it has no live constructor to rewrite
                // calls against. Its defaults are reported and left alone.
                let fields = self.scope.fields.take();
                self.conditional_depth += 1;
                self.visit_body(body);
                self.conditional_depth -= 1;
                self.scope.fields = fields;
                continue;
            }
            let truth = test.map_or(Truthiness::True, |test| match test {
                Expr::Name(name)
                    if self.scope.class == ClassScope::None && self.lexical_scope.is_empty() =>
                {
                    self.known_truthiness
                        .get(name.id.as_str())
                        .copied()
                        .unwrap_or(Truthiness::Unknown)
                }
                _ => Truthiness::from_expr(test, |_| false),
            });
            match truth {
                Truthiness::False | Truthiness::Falsey | Truthiness::None => {}
                Truthiness::True | Truthiness::Truthy => {
                    if uncertain {
                        self.conditional_depth += 1;
                    }
                    self.visit_body(body);
                    if uncertain {
                        self.conditional_depth -= 1;
                    }
                    return;
                }
                Truthiness::Unknown => {
                    uncertain = true;
                    self.conditional_depth += 1;
                    self.visit_body(body);
                    self.conditional_depth -= 1;
                }
            }
        }
    }

    fn visit_function_statement<'a>(
        &mut self,
        function: &'a ast::StmtFunctionDef,
        statement: &'a Stmt,
    ) where
        Self: Visitor<'a>,
    {
        self.check_function(function);
        self.invalidate_function_header(function);
        self.forget_globals_rebound_in(&function.body);
        let outer = self.scope;
        let outer_aliases = self.aliases.clone();
        let outer_local_classes = self.local_classes.clone();
        let outer_metaclass_classes = self.metaclass_classes.clone();
        let outer_metaclass_definitions = self.metaclass_definitions.clone();
        let outer_enum_classes = self.local_enum_classes.clone();
        // Written straight in a class body, this function sees none of the
        // names that body binds: a class written above it there is not in
        // scope here, and reading a base through it would answer with a class
        // the method never builds on. What the class statement started with is
        // what the body reaches instead.
        if self.lexical_is_class.last() == Some(&true) {
            if let Some(entered) = self.entered_class_metaclass_classes.last() {
                self.metaclass_classes.clone_from(entered);
            }
            if let Some(entered) = self.entered_class_enum_classes.last() {
                self.local_enum_classes.clone_from(entered);
            }
        }
        let mut parameters = BoundNames::default();
        parameters.parameters(&function.parameters);
        // A parameter is bound the moment the body starts, so it hides an
        // enclosing class of the same name for everything the body does.
        let parameter_names = parameters.names.clone();
        for name in parameters.names {
            self.aliases.invalidate_parameter(&name);
            self.local_enum_classes.remove(&name);
        }
        self.scope = Scope {
            private: self.encloses_private(function.name.as_str(), outer),
            fields: None,
            class: ClassScope::None,
            enum_class: false,
            kept_default: false,
        };
        let outer_repeated_functions = self.repeated_functions.clone();
        let mut function_names = BTreeSet::new();
        let mut repeated_functions = BTreeSet::new();
        collect_repeated_functions(&function.body, &mut function_names, &mut repeated_functions);
        self.repeated_functions = repeated_functions;
        self.lexical_scope.push(function.name.to_string());
        self.lexical_is_class.push(false);
        self.lexical_bindings.push(parameter_names);
        walk_stmt(self, statement);
        self.lexical_bindings.pop();
        self.lexical_is_class.pop();
        self.lexical_scope.pop();
        self.repeated_functions = outer_repeated_functions;
        self.restore_aliases(outer_aliases);
        self.local_classes = outer_local_classes;
        self.metaclass_classes = outer_metaclass_classes;
        self.metaclass_definitions = outer_metaclass_definitions;
        self.local_enum_classes = outer_enum_classes;
        self.aliases.invalidate(function.name.as_str());
        self.scope = outer;
    }

    fn is_stub(&self) -> bool {
        is_stub(self.path)
    }

    fn qualified_class(&self, name: &str, direct_class_member: bool) -> String {
        if direct_class_member {
            qualified_name(
                self.classes.last().map(|parent| parent.qualified.as_str()),
                name,
            )
        } else {
            qualified_lexical_name(&self.lexical_scope, name)
        }
    }

    fn leave_class(&mut self) {
        self.class_constructs.pop();
        self.delegation_protocols.pop();
        self.attribute_interceptors.pop();
        self.instance_attributes.pop();
        self.class_deletions.pop();
        self.class_assignments.pop();
        self.class_rewraps.pop();
        self.method_aliases.pop();
    }

    /// Whether the rule applies to something with no name of its own, such as
    /// a lambda. It takes the privacy of the scope holding it.
    fn enabled_unnamed(&self) -> bool {
        !self.private_only || self.scope.private
    }

    fn enabled(&self, name: &str) -> bool {
        if !self.private_only {
            return true;
        }
        // A re-exported name is public API however private its module is.
        !self.reexports.covers(name) && (self.scope.private || is_private(name))
    }

    fn is_delegation_protocol_method(&self, name: &str) -> bool {
        self.scope.class != ClassScope::None
            && self.delegation_protocols.last() == Some(&true)
            && matches!(name, "close" | "send" | "throw")
    }

    /// Whether definitions inside `name` are private, given the scope holding
    /// it. A re-exported class or function carries everything it contains into
    /// the public API with it.
    fn encloses_private(&self, name: &str, outer: Scope) -> bool {
        !self.reexports.covers(name) && (outer.private || is_private(name))
    }

    /// Mark the directives that suppress a violation at `offset`, preferring
    /// directives that name the code so blanket ones stay unclaimed.
    fn suppress(&mut self, offset: TextSize) -> bool {
        let file_level = self.select(|directive| directive.file_level);
        let inline =
            self.select(|directive| !directive.file_level && directive.line.contains(offset));
        let header = self.header.and_then(|start| {
            self.select(|directive| !directive.file_level && directive.line.start() == start)
        });
        let mut suppressed = false;
        for index in [file_level, inline, header].into_iter().flatten() {
            self.directives[index].used = true;
            suppressed = true;
        }
        suppressed
    }

    fn select(&self, applies: impl Fn(&Directive) -> bool) -> Option<usize> {
        let candidates = || {
            self.directives
                .iter()
                .enumerate()
                .filter(|(_, directive)| applies(directive))
        };
        candidates()
            .find(|(_, directive)| directive.explicit)
            .or_else(|| candidates().next())
            .map(|(index, _)| index)
    }

    /// Record a violation, returning whether it survived `noqa` suppression.
    /// Record a violation. Returns whether it was reported *and* is fixable,
    /// which is what decides whether the default is one a call site has to be
    /// given back.
    fn report_with(&mut self, offset: TextSize, message: String, fix: Option<TextRange>) -> bool {
        if self.suppress(offset) {
            return false;
        }
        let (line, column) = self.lines.locate(self.source, offset);
        self.diagnostics.push(Diagnostic {
            path: self.path.to_path_buf(),
            line,
            column,
            code: "NOD001",
            message,
            fix,
        });
        fix.is_some()
    }

    /// The range to delete for a default, or `None` where deleting it would
    /// not preserve behaviour.
    ///
    /// In a stub, `x: int = ...` does not declare a default *value*: it is the
    /// convention for "this parameter has a default, unspecified here".
    /// Deleting it makes the parameter required, so the stub stops matching
    /// the implementation it describes and type checkers reject callers that
    /// legitimately omit the argument. Nothing in the source code changes to
    /// match, because a stub has no runtime behaviour to change.
    fn fixable(&self, default: &Expr, fix: TextRange) -> Option<TextRange> {
        if self.is_stub() && matches!(default, Expr::EllipsisLiteral(_)) {
            return None;
        }
        Some(fix)
    }

    /// Include redundant parentheses that belong to a default in its deletion.
    ///
    /// Expression ranges omit wrapping parentheses. Deleting only through the
    /// expression in `value=(1)` therefore leaves the closing `)` behind. The
    /// opening parentheses between the parameter and expression tell us how
    /// many following closing parentheses belong to the default rather than to
    /// the function signature.
    fn default_fix_range(&self, parameter_end: TextSize, default: &Expr) -> TextRange {
        let wrapping = self
            .tokens
            .in_range(TextRange::new(parameter_end, default.start()))
            .iter()
            .filter(|token| token.kind() == TokenKind::Lpar)
            .count();
        let mut end = default.end();
        let mut remaining = wrapping;
        for token in self
            .tokens
            .after(default.end())
            .iter()
            .filter(|token| !token.kind().is_trivia())
        {
            if remaining == 0 || token.kind() != TokenKind::Rpar {
                break;
            }
            end = token.end();
            remaining -= 1;
        }
        TextRange::new(parameter_end, end)
    }

    /// Add a diagnostic for every directive that named `NOD001` without
    /// suppressing anything, then return the file's diagnostics in order.
    fn finish(mut self) -> Checked {
        for directive in &self.directives {
            if !directive.explicit || directive.used {
                continue;
            }
            let (line, column) = self.lines.locate(self.source, directive.start);
            self.diagnostics.push(Diagnostic {
                path: self.path.to_path_buf(),
                line,
                column,
                code: "NOD002",
                message: "unused `noqa` directive for `NOD001`".to_owned(),
                fix: Some(directive.fix),
            });
        }
        self.diagnostics
            .sort_by_key(|diagnostic| (diagnostic.line, diagnostic.column));
        Checked {
            diagnostics: self.diagnostics,
            signatures: self.signatures,
            skipped: self.skipped,
        }
    }

    /// Report the defaults on a lambda's parameters.
    ///
    /// A lambda takes the same parameter kinds as a `def` and carries the same
    /// late-binding hazard, so the rule covers it. Its call sites cannot be
    /// updated the way a `def`'s are — an anonymous function has no name to
    /// resolve a call through — so removing one is reported as a call `--fix`
    /// left alone, rather than passing silently.
    fn check_lambda(&mut self, lambda: &ast::ExprLambda) {
        if !self.enabled_unnamed() {
            return;
        }
        let Some(parameters) = &lambda.parameters else {
            return;
        };
        let mut removed = false;
        let mut kept = false;
        for parameter in parameters
            .posonlyargs
            .iter()
            .chain(&parameters.args)
            .map(|parameter| (parameter, true))
            .chain(
                parameters
                    .kwonlyargs
                    .iter()
                    .map(|parameter| (parameter, false)),
            )
        {
            let (parameter, positional) = parameter;
            let Some(default) = &parameter.default else {
                continue;
            };
            let range = self.default_fix_range(parameter.parameter.end(), default);
            let fix = if self.is_stub() || (positional && kept) {
                None
            } else {
                self.fixable(default, range)
            };
            let was_removed = self.report_with(
                default.start(),
                format!(
                    "parameter `{}` of lambda has a default",
                    parameter.parameter.name
                ),
                fix,
            );
            removed |= was_removed;
            if positional && !was_removed {
                kept = true;
            }
        }
        if !removed || !self.collect_signatures {
            return;
        }
        let (line, column) = self.lines.locate(self.source, lambda.start());
        self.skipped.push(Skipped {
            path: self.path.to_path_buf(),
            line,
            column,
            callable: "<lambda>".to_owned(),
            reason: "an anonymous function has no name to resolve a call through".to_owned(),
        });
    }

    /// Whether reading an attribute runs `function`, leaving no call site a
    /// fixer could update. That holds for a `@property`, for a property alias
    /// in the same class body, and for one in another class that names this
    /// method.
    fn descriptor_invokes(&self, function: &ast::StmtFunctionDef) -> bool {
        if self.scope.class == ClassScope::None {
            return false;
        }
        is_property(function, &self.aliases, &self.module_bindings)
            || self.method_aliases.last().is_some_and(|aliases| {
                aliases.get(function.name.as_str()).is_some_and(|aliases| {
                    aliases
                        .iter()
                        .any(|alias| alias.kind == MethodAliasKind::Property)
                })
            })
            || self.classes.last().is_some_and(|class| {
                self.property_aliased_methods
                    .contains(&(class.name.clone(), function.name.to_string()))
            })
    }

    fn check_function(&mut self, function: &ast::StmtFunctionDef) {
        if !self.enabled(function.name.as_str()) {
            return;
        }
        // The function's own range starts at its first decorator, so the name
        // locates the `def` line that a signature-wide directive sits on.
        let enclosing = self.header;
        self.header = Some(line_start(self.source, function.name.start()));
        let descriptor_invoked = self.descriptor_invokes(function);
        let implicitly_called = self.scope.class != ClassScope::None
            && self.generated_code_calls(function.name.as_str());
        let mut removed = Vec::new();
        let known_descriptor = |expression: &Expr| match expression {
            Expr::Name(name) => {
                matches!(name.id.as_str(), "staticmethod" | "classmethod")
                    || self.aliases.staticmethods.contains(name.id.as_str())
                    || self.aliases.classmethods.contains(name.id.as_str())
            }
            Expr::Attribute(attribute)
                if matches!(attribute.attr.as_str(), "staticmethod" | "classmethod") =>
            {
                matches!(attribute.value.as_ref(), Expr::Name(name) if self.aliases.builtins_modules.contains(name.id.as_str()))
            }
            _ => false,
        };
        let unknown_decorator = function
            .decorator_list
            .iter()
            .any(|decorator| !known_descriptor(&decorator.expression));
        // A parameter without a default cannot follow one with a default, so
        // once a default has to stay, every positional default after it stays
        // too. Keyword-only parameters sit after the `*`, where order does not
        // constrain them, so they are judged on their own.
        let mut kept = false;
        for (parameter, positional) in function
            .parameters
            .posonlyargs
            .iter()
            .chain(&function.parameters.args)
            .map(|parameter| (parameter, true))
            .chain(
                function
                    .parameters
                    .kwonlyargs
                    .iter()
                    .map(|parameter| (parameter, false)),
            )
        {
            let Some(default) = &parameter.default else {
                continue;
            };
            let range = self.default_fix_range(parameter.parameter.end(), default);
            let fix = if implicitly_called
                || self.method_has_implicit_alias(function.name.as_str())
                || descriptor_invoked
                || unknown_decorator
                || self.is_stub()
                || (self.scope.class != ClassScope::None
                    && implicitly_called_method(function.name.as_str()))
                || self.method_is_intercepted(function.name.as_str())
                || self.method_is_rebound_later(function)
                || self.is_delegation_protocol_method(function.name.as_str())
                || (self.scope.class == ClassScope::Metaclass
                    && matches!(function.name.as_str(), "__init__" | "mro"))
                || (self.scope.enum_class
                    && matches!(
                        function.name.as_str(),
                        "__init__" | "_missing_" | "_generate_next_value_"
                    ))
                || (self.scope.class == ClassScope::None
                    && self.lexical_scope.is_empty()
                    && matches!(
                        function.name.as_str(),
                        "__getattr__" | "__dir__" | "__annotate__"
                    ))
                || self.repeated_functions.contains(function.name.as_str())
                || (positional && kept)
            {
                None
            } else {
                self.fixable(default, range)
            };
            let was_removed = self.report_with(
                default.start(),
                format!(
                    "parameter `{}` of function `{}` has a default",
                    parameter.parameter.name, function.name
                ),
                fix,
            );
            if positional && !was_removed {
                kept = true;
            }
            if was_removed && self.collect_signatures {
                removed.push(Removed {
                    parameter: parameter.parameter.name.to_string(),
                    fix: range,
                    value: literal_text(default, self.source),
                });
            }
        }
        self.header = enclosing;
        if removed.is_empty() {
            return;
        }
        self.record_signature(function, removed);
    }

    fn method_is_intercepted(&self, name: &str) -> bool {
        self.scope.class != ClassScope::None
            && (self.attribute_interceptors.last() == Some(&true)
                || self
                    .instance_attributes
                    .last()
                    .is_some_and(|attributes| attributes.contains(name)))
    }

    fn method_has_implicit_alias(&self, name: &str) -> bool {
        self.scope.class != ClassScope::None
            && self.method_aliases.last().is_some_and(|aliases| {
                aliases.get(name).is_some_and(|aliases| {
                    aliases.iter().any(|alias| {
                        implicitly_called_method(&alias.name)
                            || matches!(alias.name.as_str(), "__init__" | "__new__")
                            || self.generated_code_calls(&alias.name)
                    })
                })
            })
    }

    /// Whether a method of this name is reached from code the decorator
    /// generates rather than from a call written in the source.
    ///
    /// `@dataclass` writes an `__init__` that calls `__post_init__` with the
    /// `InitVar` fields and nothing else, so a default there has no call site
    /// to be carried to and has to stay where it is. The same holds when the
    /// hook is a method defined under another name and then aliased, because
    /// the generated call finds it by the name it is bound to.
    fn generated_code_calls(&self, name: &str) -> bool {
        self.scope.fields == Some(FieldStyle::Dataclass) && name == "__post_init__"
    }

    fn method_is_rebound_later(&self, function: &ast::StmtFunctionDef) -> bool {
        if self.scope.class == ClassScope::None {
            return false;
        }
        // `target = staticmethod(target)` assigns the name a second time but
        // keeps the same function behind it, so it replaces nothing. Every
        // other later assignment does, including one that follows a rewrap.
        let rewraps = self
            .class_rewraps
            .last()
            .and_then(|rewraps| rewraps.get(function.name.as_str()));
        self.class_assignments.last().is_some_and(|assignments| {
            assignments
                .get(function.name.as_str())
                .is_some_and(|offsets| {
                    offsets.iter().any(|offset| {
                        *offset > function.start()
                            && !rewraps.is_some_and(|rewraps| rewraps.contains(offset))
                    })
                })
        })
    }

    /// Record what a call to this function has to be given back, under every
    /// name it can be called by.
    fn record_signature(&mut self, function: &ast::StmtFunctionDef, removed: Vec<Removed>) {
        let parameter_name =
            |parameter: &ast::ParameterWithDefault| parameter.parameter.name.to_string();
        let mut signature = Signature {
            name: if self.scope.class == ClassScope::None {
                qualified_lexical_name(&self.lexical_scope, function.name.as_str())
            } else {
                function.name.to_string()
            },
            positional: function
                .parameters
                .posonlyargs
                .iter()
                .chain(&function.parameters.args)
                .map(parameter_name)
                .collect(),
            positional_only: function.parameters.posonlyargs.len(),
            path: self.path.to_path_buf(),
            kind: match self.classes.last() {
                Some(class) if self.scope.class != ClassScope::None => Callable::Method {
                    class: class.qualified.clone(),
                    receiver: method_receiver(function, &self.aliases, &self.module_bindings),
                },
                _ => Callable::Function,
            },
            complete: true,
            removed,
        };
        if self.scope.class != ClassScope::None {
            if let Some(aliases) = self
                .method_aliases
                .last()
                .and_then(|aliases| aliases.get(function.name.as_str()))
                .cloned()
            {
                // Another name still takes the receiver the method itself
                // declared, so read that before a same-name wrapper rewrites
                // it below.
                let declared = match &signature.kind {
                    Callable::Method { receiver, .. } => *receiver,
                    _ => Receiver::None,
                };
                // `target = staticmethod(target)` rebinds the method's own
                // name rather than adding a second one. The wrapper decides
                // how the one name is called, so it belongs on the signature
                // already being emitted: a second signature under that name
                // would collide with it and lose both.
                if let Some(rebound) = aliases
                    .iter()
                    .rev()
                    .find(|alias| alias.name == function.name.as_str())
                {
                    if let Callable::Method { receiver, .. } = &mut signature.kind {
                        *receiver = alias_receiver(rebound.kind, declared);
                    }
                }
                self.signatures.extend(aliases.iter().filter_map(|alias| {
                    if alias.original_class.is_some()
                        || alias.kind == MethodAliasKind::Property
                        || alias.name == function.name.as_str()
                    {
                        return None;
                    }
                    let mut alias_signature = signature.clone();
                    alias_signature.name.clone_from(&alias.name);
                    if let Callable::Method { receiver, .. } = &mut alias_signature.kind {
                        *receiver = alias_receiver(alias.kind, declared);
                    }
                    Some(alias_signature)
                }));
            }
        }
        if function.name.as_str() == "__init__" {
            if let Callable::Method { class, .. } = &signature.kind {
                self.signatures.push(Signature {
                    name: class.clone(),
                    kind: Callable::Constructor,
                    ..signature.clone()
                });
            }
        }
        self.signatures.push(signature);
    }

    fn check_field(&mut self, style: FieldStyle, statement: &Stmt) {
        let Stmt::AnnAssign(assign) = statement else {
            return;
        };
        let Expr::Name(name) = &*assign.target else {
            return;
        };
        if (style == FieldStyle::Base && name.id.starts_with('_'))
            || is_class_var(statement, &self.aliases)
            || is_pydantic_private_attr(assign.value.as_deref(), &self.aliases)
        {
            return;
        }
        // A `_: KW_ONLY` marker is not a field, so it takes no place in the
        // constructor's positional order. It also makes every field after it
        // keyword-only, which does hold for the rest of the class body.
        let pseudo_field = annotates_kw_only(&assign.annotation, &self.aliases);
        // A field declared `init=False` is not a constructor parameter, so a
        // call must never be given it.
        let constructs = assign.value.as_deref().is_none_or(|value| {
            !field_excluded_from_init(value, &self.aliases, &self.module_bindings)
        });
        // `field(kw_only=...)` decides for one field, over whatever the
        // decorator or a marker said for the class.
        let kw_only = assign
            .value
            .as_deref()
            .and_then(|value| field_says_kw_only(value, &self.aliases, &self.module_bindings))
            .unwrap_or_else(|| self.classes.last().is_some_and(|class| class.kw_only));
        if let Some(class) = self.classes.last_mut() {
            if pseudo_field {
                class.kw_only = true;
            }
            // Every other field that the constructor takes positionally counts
            // towards the order, even one without a default or one this run is
            // not enforcing. A keyword-only field holds no position.
            else if constructs && !kw_only && self.conditional_depth == 0 {
                class.fields.push(name.id.to_string());
            }
        }
        let Some(value) = assign.value.as_deref() else {
            return;
        };
        if style == FieldStyle::Dataclass && is_dataclasses_missing(value, &self.aliases) {
            return;
        }
        if !self.enabled(name.id.as_str()) {
            return;
        }
        let Some(mut default) = field_default(
            value,
            assign.annotation.end(),
            self.source,
            &self.aliases,
            &self.module_bindings,
        ) else {
            return;
        };
        if default.fix.start() == assign.annotation.end() {
            default.fix = self.default_fix_range(assign.annotation.end(), value);
        }
        // As in a signature, a field that keeps its default forces every field
        // after it to keep its own. A keyword-only field is exempt, since
        // `dataclasses` moves it past the `*` where order does not constrain it.
        let rebound_later = self.class_assignments.last().is_some_and(|assignments| {
            assignments
                .get(name.id.as_str())
                .is_some_and(|offsets| offsets.iter().any(|offset| *offset > assign.start()))
        });
        let deleted_later = self
            .class_deletions
            .last()
            .is_some_and(|deleted| deleted.contains(name.id.as_str()));
        let fix = if rebound_later
            || deleted_later
            || self.conditional_depth > 0
            || !constructs
            || !self.class_constructs.last().copied().unwrap_or(true)
            || matches!(value, Expr::Call(call) if matches!(call.func.as_ref(), Expr::Name(name) if self.aliases.invalidated_field_helpers.contains(name.id.as_str())))
            || (style == FieldStyle::Base
                && pydantic_field_has_validation_alias(value, &self.aliases, &self.module_bindings))
            || (self.scope.kept_default && !kw_only)
        {
            None
        } else {
            self.fixable(value, default.fix)
        };
        let was_removed = self.report_with(
            value.start(),
            format!("{} `{}` has a {}", style.noun(), name.id, default.kind),
            fix,
        );
        if !kw_only && !was_removed {
            self.scope.kept_default = true;
        }
        if was_removed && self.collect_signatures && constructs {
            if let Some(class) = self.classes.last_mut() {
                class.removed.push(Removed {
                    parameter: name.id.to_string(),
                    fix: default.fix,
                    value: default.value,
                });
            }
        }
    }

    /// Index aliases copied from a same-file class whose signature has already
    /// been collected. Unknown and forward-referenced classes stay unresolved.
    ///
    /// `direct_class_member` says whether the class being collected is written
    /// straight in a class body, which is what decides how the class it copies
    /// from is named.
    fn record_inherited_method_aliases(&mut self, direct_class_member: bool) {
        let (Some(class), Some(aliases)) = (self.classes.last(), self.method_aliases.last()) else {
            return;
        };
        let mut inherited = Vec::new();
        for (method, aliases) in aliases {
            for alias in aliases {
                let Some(original_class) = &alias.original_class else {
                    continue;
                };
                // Reading the attribute runs the getter, so a property alias
                // names no call for the fixer to rewrite.
                if alias.kind == MethodAliasKind::Property {
                    continue;
                }
                // The class an alias copies from is written beside the one
                // being collected, so it is identified the way that one is:
                // under the class holding both where both are written straight
                // in a class body, and under the scopes around them where they
                // are not. A function body is one of those scopes, and naming a
                // class in there as though it sat beside a module-level one
                // matches nothing, or matches a namesake written elsewhere and
                // takes its parameters.
                let original_class = if direct_class_member {
                    qualified_name(
                        self.classes
                            .len()
                            .checked_sub(2)
                            .and_then(|parent| self.classes.get(parent))
                            .map(|parent| parent.qualified.as_str()),
                        original_class,
                    )
                } else {
                    qualified_lexical_name(&self.lexical_scope, original_class)
                };
                let Some(signature) = self.signatures.iter().find(|signature| {
                    signature.name == *method
                        && matches!(
                            &signature.kind,
                            Callable::Method { class, .. } if class == &original_class
                        )
                }) else {
                    continue;
                };
                let mut signature = signature.clone();
                signature.name.clone_from(&alias.name);
                if let Callable::Method {
                    class: owner,
                    receiver,
                } = &mut signature.kind
                {
                    owner.clone_from(&class.qualified);
                    *receiver = alias_receiver(alias.kind, *receiver);
                }
                inherited.push(signature);
            }
        }
        self.signatures.extend(inherited);
    }
}

fn instance_attributes(class: &ast::StmtClassDef) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for statement in &class.body {
        let Stmt::FunctionDef(function) = statement else {
            continue;
        };
        let Some(receiver) = function
            .parameters
            .posonlyargs
            .first()
            .or_else(|| function.parameters.args.first())
        else {
            continue;
        };
        let mut collector = InstanceAttributeCollector {
            receiver: receiver.parameter.name.as_str(),
            names: &mut names,
        };
        for statement in &function.body {
            collector.visit_stmt(statement);
        }
    }
    names
}

struct InstanceAttributeCollector<'a> {
    receiver: &'a str,
    names: &'a mut BTreeSet<String>,
}

impl InstanceAttributeCollector<'_> {
    fn bind(&mut self, target: &Expr) {
        match target {
            Expr::Attribute(attribute) if matches!(attribute.value.as_ref(), Expr::Name(name) if name.id.as_str() == self.receiver) =>
            {
                self.names.insert(attribute.attr.to_string());
            }
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|target| self.bind(target)),
            Expr::List(list) => list.elts.iter().for_each(|target| self.bind(target)),
            _ => {}
        }
    }
}

impl<'a> Visitor<'a> for InstanceAttributeCollector<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Assign(assign) => {
                assign.targets.iter().for_each(|target| self.bind(target));
                self.visit_expr(&assign.value);
            }
            Stmt::AnnAssign(assign) => {
                self.bind(&assign.target);
                if let Some(value) = &assign.value {
                    self.visit_expr(value);
                }
            }
            Stmt::AugAssign(assign) => {
                self.bind(&assign.target);
                self.visit_expr(&assign.value);
            }
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            _ => walk_stmt(self, statement),
        }
    }
}

/// A method Python may call through syntax or a built-in rather than through
/// an explicit attribute call that the fixer can rewrite.
#[allow(
    clippy::too_many_lines,
    reason = "keeping the exhaustive protocol-name catalogue together makes omissions visible"
)]
fn implicitly_called_method(name: &str) -> bool {
    matches!(
        name,
        "__new__"
            | "__del__"
            | "__getattribute__"
            | "__getattr__"
            | "__setattr__"
            | "__delattr__"
            | "__dir__"
            | "__get__"
            | "__set__"
            | "__delete__"
            | "__set_name__"
            | "__instancecheck__"
            | "__subclasscheck__"
            | "__subclasshook__"
            | "__class_getitem__"
            | "__mro_entries__"
            | "__prepare__"
            | "__init_subclass__"
            | "__annotate__"
            | "find_spec"
            | "create_module"
            | "exec_module"
            | "persistent_id"
            | "reducer_override"
            | "persistent_load"
            | "get_code"
            | "__call__"
            | "__enter__"
            | "__exit__"
            | "__aenter__"
            | "__aexit__"
            | "__await__"
            | "__iter__"
            | "__next__"
            | "__aiter__"
            | "__anext__"
            | "__len__"
            | "__length_hint__"
            | "__getitem__"
            | "__setitem__"
            | "__delitem__"
            | "__missing__"
            | "__contains__"
            | "__reversed__"
            | "__bool__"
            | "__str__"
            | "__repr__"
            | "__bytes__"
            | "__buffer__"
            | "__release_buffer__"
            | "__format__"
            | "__hash__"
            | "__sizeof__"
            | "__copy__"
            | "__replace__"
            | "__deepcopy__"
            | "__reduce__"
            | "__reduce_ex__"
            | "__getnewargs__"
            | "__getnewargs_ex__"
            | "__getstate__"
            | "__setstate__"
            | "__index__"
            | "__int__"
            | "__float__"
            | "__complex__"
            | "__round__"
            | "__trunc__"
            | "__floor__"
            | "__ceil__"
            | "__fspath__"
            | "__lt__"
            | "__le__"
            | "__eq__"
            | "__ne__"
            | "__gt__"
            | "__ge__"
            | "__add__"
            | "__sub__"
            | "__mul__"
            | "__matmul__"
            | "__truediv__"
            | "__floordiv__"
            | "__mod__"
            | "__divmod__"
            | "__pow__"
            | "__lshift__"
            | "__rshift__"
            | "__and__"
            | "__xor__"
            | "__or__"
            // Reflected operands, which Python tries when the left operand's
            // method is missing or returns `NotImplemented`.
            | "__radd__"
            | "__rsub__"
            | "__rmul__"
            | "__rmatmul__"
            | "__rtruediv__"
            | "__rfloordiv__"
            | "__rmod__"
            | "__rdivmod__"
            | "__rpow__"
            | "__rlshift__"
            | "__rrshift__"
            | "__rand__"
            | "__rxor__"
            | "__ror__"
            // Augmented assignment, e.g. `total += item`.
            | "__iadd__"
            | "__isub__"
            | "__imul__"
            | "__imatmul__"
            | "__itruediv__"
            | "__ifloordiv__"
            | "__imod__"
            | "__ipow__"
            | "__ilshift__"
            | "__irshift__"
            | "__iand__"
            | "__ixor__"
            | "__ior__"
            // Unary operators and `abs()`.
            | "__neg__"
            | "__pos__"
            | "__abs__"
            | "__invert__"
    )
}

impl<'a> Visitor<'a> for Checker<'a> {
    #[allow(
        clippy::too_many_lines,
        reason = "class traversal saves and restores every piece of lexical resolution state"
    )]
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::If(branch) => self.visit_conditional(branch),
            Stmt::Try(_) => self.visit_uncertain(statement),
            Stmt::Match(block) => self.visit_match(block),
            Stmt::For(loop_) => self.visit_loop(loop_),
            Stmt::While(loop_) => self.visit_while(loop_, statement),
            Stmt::With(block) => self.visit_with(block),
            Stmt::Import(_) | Stmt::ImportFrom(_) => self.visit_import_statement(statement),
            Stmt::FunctionDef(function) => self.visit_function_statement(function, statement),
            Stmt::ClassDef(class) => {
                self.invalidate_class_header(class);
                self.forget_globals_rebound_in(&class.body);
                let outer = self.scope;
                let outer_aliases = self.aliases.clone();
                let outer_local_classes = self.local_classes.clone();
                let outer_metaclass_classes = self.metaclass_classes.clone();
                let outer_metaclass_definitions = self.metaclass_definitions.clone();
                let outer_enum_classes = self.local_enum_classes.clone();
                let enum_class = self.is_enum_class(class);
                let outer_repeated_functions = self.repeated_functions.clone();
                let mut method_names = BTreeSet::new();
                let mut repeated_methods = BTreeSet::new();
                collect_repeated_functions(&class.body, &mut method_names, &mut repeated_methods);
                self.repeated_functions = repeated_methods;
                let old_header = self.header;
                self.header = Some(line_start(self.source, class.name.start()));
                let style = self.class_field_style(class);
                self.scope = Scope {
                    private: self.encloses_private(class.name.as_str(), outer),
                    fields: style,
                    class: if defines_metaclass(class, &self.metaclass_definitions) {
                        ClassScope::Metaclass
                    } else {
                        ClassScope::Ordinary
                    },
                    enum_class,
                    // Each class body starts fresh; a base's fields are not
                    // written here.
                    kept_default: false,
                };
                // Read where the header is: the tables the predicate consults
                // are the enclosing scope's until the body has been walked,
                // and a name the body binds says nothing about the base the
                // header already resolved.
                let unseen_import_base = self.inherits_unseen_import(class);
                self.class_constructs
                    .push(self.class_constructs_safely(class));
                let defines_iterator_method = |expected: &str| {
                    class.body.iter().any(|statement| {
                        matches!(statement, ast::Stmt::FunctionDef(function) if function.name.as_str() == expected)
                    })
                };
                self.delegation_protocols.push(
                    defines_iterator_method("__iter__") && defines_iterator_method("__next__"),
                );
                let shape_name = qualified_class_name(&self.lexical_scope, class.name.as_str());
                self.attribute_interceptors.push(
                    defines_iterator_method("__getattribute__")
                        || self.metaclass_intercepted_classes.contains(&shape_name),
                );
                self.instance_attributes.push(instance_attributes(class));
                self.class_deletions.push(deleted_names(&class.body));
                self.class_assignments.push(class_assignments(&class.body));
                self.class_rewraps.push(class_rewraps(
                    &class.body,
                    &self.aliases,
                    &self.module_bindings,
                ));
                let context = AliasContext {
                    aliases: &self.aliases,
                    module_bindings: &self.module_bindings,
                };
                self.method_aliases
                    .push(class_method_aliases(class, context));
                if self.collect_signatures {
                    let inherited = self.inherited_fields(class, style);
                    // A base of this file's own contributes its fields ahead of
                    // the body's, which is where `dataclasses` puts them, and
                    // its removed defaults too: a subclass constructed with
                    // none of them still needs every one back.
                    let (fields, removed) = match &inherited {
                        Inherited::Known(shape) => (shape.fields.clone(), shape.removed.clone()),
                        Inherited::Nothing | Inherited::Unknown => (Vec::new(), Vec::new()),
                    };
                    self.scope.kept_default = match &inherited {
                        Inherited::Known(shape) => shape.kept_default,
                        // An unseen base may end in a positional default. A
                        // child field without one would then make dataclass
                        // construction fail before any call can be rewritten.
                        Inherited::Unknown => self.unknown_base_may_end_in_default(class),
                        Inherited::Nothing => false,
                    };
                    let qualified =
                        self.qualified_class(class.name.as_str(), outer.class != ClassScope::None);
                    self.classes.push(ClassCollector {
                        name: class.name.to_string(),
                        qualified,
                        style,
                        inherits: matches!(inherited, Inherited::Unknown),
                        constructs: self.generates_init(class),
                        kw_only: decorator_says_kw_only(class, &self.aliases),
                        fields,
                        removed,
                    });
                    self.record_inherited_method_aliases(outer.class != ClassScope::None);
                }
                // A class written in another class body is built from the
                // names that body holds, so its bases were read above with
                // them in place. Its own body reaches past them, though:
                // neither class scope is a closure, so what the enclosing
                // class statement started with is what this body starts with.
                let body_metaclass_classes = if self.lexical_is_class.last() == Some(&true) {
                    self.entered_class_metaclass_classes
                        .last()
                        .cloned()
                        .unwrap_or_else(|| outer_metaclass_classes.clone())
                } else {
                    outer_metaclass_classes.clone()
                };
                let body_enum_classes = if self.lexical_is_class.last() == Some(&true) {
                    self.entered_class_enum_classes
                        .last()
                        .cloned()
                        .unwrap_or_else(|| outer_enum_classes.clone())
                } else {
                    outer_enum_classes.clone()
                };
                self.lexical_scope.push(class.name.to_string());
                self.lexical_is_class.push(true);
                self.lexical_bindings.push(BTreeSet::new());
                self.metaclass_classes.clone_from(&body_metaclass_classes);
                self.entered_class_metaclass_classes
                    .push(body_metaclass_classes);
                self.local_enum_classes.clone_from(&body_enum_classes);
                self.entered_class_enum_classes.push(body_enum_classes);
                walk_stmt(self, statement);
                self.entered_class_enum_classes.pop();
                self.entered_class_metaclass_classes.pop();
                self.lexical_bindings.pop();
                self.lexical_is_class.pop();
                self.lexical_scope.pop();
                // The class name becomes visible only after its body has
                // executed. A later class with the same name must not change
                // how this class's bases were resolved.
                self.local_classes = outer_local_classes;
                self.local_classes.insert(class.name.to_string());
                // The statement binds the name in the scope around it whether
                // or not the class it writes has a shape worth recording. A
                // plain class stands between the name and an enclosing
                // dataclass of the same spelling exactly as a parameter or a
                // rebinding does, and reading past it would hand a subclass
                // fields its base has never had.
                if let Some(bindings) = self.lexical_bindings.last_mut() {
                    bindings.insert(class.name.to_string());
                }
                self.metaclass_classes = outer_metaclass_classes;
                self.record_metaclass_construction(class, unseen_import_base);
                self.metaclass_definitions = outer_metaclass_definitions;
                self.local_enum_classes = outer_enum_classes;
                // A later class of the same name that is no enumeration takes
                // the name back, so a subclass written on it below is built
                // the ordinary way.
                if enum_class {
                    self.local_enum_classes.insert(class.name.to_string());
                } else {
                    self.local_enum_classes.remove(class.name.as_str());
                }
                self.repeated_functions = outer_repeated_functions;
                if defines_metaclass(class, &self.metaclass_definitions) {
                    self.metaclass_definitions.insert(class.name.to_string());
                } else {
                    self.metaclass_definitions.remove(class.name.as_str());
                }
                self.record_base_field_class(class.name.as_str(), style);
                self.leave_class();
                let collector = self
                    .collect_signatures
                    .then(|| self.classes.pop())
                    .flatten();
                if let Some(mut collector) = collector {
                    collector.normalize_repeated_fields();
                    if collector.style.is_some() {
                        // Recorded whether or not anything was removed: a base
                        // contributes its field order either way. A name two
                        // classes share resolves to neither.
                        let shape = Shape {
                            fields: collector.fields.clone(),
                            removed: collector.removed.clone(),
                            complete: !collector.inherits,
                            kept_default: self.scope.kept_default,
                        };
                        self.record_shape(shape_name, shape);
                    }
                    if collector.style.is_some()
                        && collector.constructs
                        && !collector.removed.is_empty()
                    {
                        self.signatures.push(Signature {
                            name: qualified_lexical_name(&self.lexical_scope, &collector.name),
                            path: self.path.to_path_buf(),
                            positional: collector.fields,
                            positional_only: 0,
                            kind: Callable::Dataclass,
                            complete: !collector.inherits,
                            removed: collector.removed,
                        });
                    }
                }
                self.header = old_header;
                self.restore_aliases(outer_aliases);
                self.aliases.invalidate(class.name.as_str());
                self.scope = outer;
            }
            _ => {
                if let Some(style) = self.scope.fields {
                    self.check_field(style, statement);
                }
                walk_stmt(self, statement);
                self.invalidate_statement_aliases(statement);
            }
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Named(named) = expression {
            self.visit_expr(&named.value);
            self.visit_expr(&named.target);
            // A walrus in a lambda binds in the lambda's own scope, which
            // nothing outside the lambda ever reads, so the name it shares a
            // spelling with still stands for what the enclosing scope gave it.
            if self.lambda_bodies == 0 {
                self.invalidate_target_aliases(&named.target);
            }
            return;
        }
        if let Expr::Lambda(lambda) = expression {
            self.check_lambda(lambda);
            // Only the body runs in the lambda's scope. Its parameter defaults
            // and annotations are evaluated where the lambda is written, so a
            // walrus among them rebinds there.
            if let Some(parameters) = &lambda.parameters {
                self.visit_parameters(parameters);
            }
            self.lambda_bodies += 1;
            self.visit_expr(&lambda.body);
            self.lambda_bodies -= 1;
            return;
        }
        walk_expr(self, expression);
    }
}

fn is_private(name: &str) -> bool {
    name.starts_with('_') && !(name.starts_with("__") && name.ends_with("__"))
}

/// Whether the file's own path keeps everything in it out of the public API.
///
/// A private module or package that a package above re-exports under its own
/// name — `from . import _upload` — is reachable as `package._upload`, so what
/// it holds is public despite the underscore.
/// Whether a file sits in a private module or package.
///
/// Only the part of the path below the project root is import path, so the
/// walk starts there. A checkout living under a directory whose name starts
/// with an underscore — `_work/proj` — says nothing about whether the code in
/// it is private, and judging from the whole path also made the answer depend
/// on whether a relative or an absolute path was passed on the command line.
fn is_private_module(path: &Path, project_root: &Path, reexports: &Reexports) -> bool {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = resolved.strip_prefix(project_root).unwrap_or(&resolved);
    relative.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        let name = component
            .strip_suffix(".py")
            .or_else(|| component.strip_suffix(".pyi"))
            .unwrap_or(&component);
        is_private(name) && !reexports.module && !reexports.covers(name)
    })
}

/// What makes a class carry fields, if anything does. The decorator wins where
/// both apply, because a `@dataclass` built on a configured base still follows
/// the `dataclasses` rules.
fn field_style(
    class: &ast::StmtClassDef,
    bases: &FieldBases,
    aliases: &Aliases,
    base_field_classes: &BTreeSet<String>,
    module_bindings: &BTreeSet<String>,
    local_classes: &BTreeSet<String>,
) -> Option<FieldStyle> {
    if has_dataclass_decorator(class, aliases, module_bindings) {
        return Some(FieldStyle::Dataclass);
    }
    class_bases(class)
        .any(|base| {
            bases.matches_unshadowed(base, aliases, local_classes)
                || matches!(base, Expr::Name(name) if base_field_classes.contains(name.id.as_str()))
        })
        .then_some(FieldStyle::Base)
}

/// Whether the class takes fields from somewhere this class body cannot see.
///
/// The base that made it carry fields is not such a place: `BaseModel` declares
/// no fields, so a model naming it directly has its whole constructor here. Any
/// other base might declare some, including another model, which is why a
/// subclass of a subclass is left alone rather than half understood.
/// The constructor a class inherits, as far as this file can see it.
#[derive(Debug)]
enum Inherited {
    /// No base contributes fields, so the constructor is what the body says.
    Nothing,
    /// One base is a class of this file's own whose constructor is known. Its
    /// fields come first in the generated constructor.
    Known(Shape),
    /// A base this file cannot see into, so the constructor is not known.
    Unknown,
}

/// What a class of this file's own contributes to a subclass's constructor.
#[derive(Clone, Debug)]
struct Shape {
    /// The positional constructor parameters, in order.
    fields: Vec<String>,
    /// The defaults `--fix` removed, which a subclass's call sites need too.
    removed: Vec<Removed>,
    /// Whether this class's own constructor is fully known.
    complete: bool,
    /// Whether its positional fields end in a retained default, which forces a
    /// subclass's positional fields to retain their defaults too.
    kept_default: bool,
}

fn qualified_class_name(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{}.{name}", scope.join("."))
    }
}

/// Whether a base cannot contribute fields to the constructor of a class built
/// on it.
///
/// `Generic[T]`, `Protocol`, `ABC`, and `object` are structural: they declare
/// no fields, so a dataclass built on one has exactly the fields written in its
/// own body and its constructor is known from the file that defines it. Without
/// this a generic dataclass could never have its call sites updated, however
/// the project is laid out — the safe path was never escaped.
fn carries_no_fields(
    base: &Expr,
    aliases: &Aliases,
    local: &BTreeSet<String>,
    module_bindings: &BTreeSet<String>,
) -> bool {
    let base = match base {
        Expr::Subscript(subscript) => &*subscript.value,
        expression => expression,
    };
    match base {
        // A class the file defines under one of these names is that class, not
        // the typing construct, and may carry fields of its own.
        Expr::Name(name)
            if local.contains(name.id.as_str())
                || (name.id.as_str() == "object" && module_bindings.contains("object")) =>
        {
            false
        }
        Expr::Name(name) => {
            aliases.structural_bases.contains(name.id.as_str())
                || (!module_bindings.contains(name.id.as_str())
                    && matches!(name.id.as_str(), "Generic" | "Protocol" | "ABC" | "object"))
        }
        Expr::Attribute(attribute) => {
            let Expr::Name(module) = attribute.value.as_ref() else {
                return false;
            };
            match attribute.attr.as_str() {
                "Generic" | "Protocol" => {
                    matches!(module.id.as_str(), "typing" | "typing_extensions")
                        || aliases.typing_modules.contains(module.id.as_str())
                }
                "ABC" => aliases.abc_modules.contains(module.id.as_str()),
                "object" => {
                    module.id.as_str() == "builtins"
                        || aliases.builtins_modules.contains(module.id.as_str())
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn class_bases(class: &ast::StmtClassDef) -> impl Iterator<Item = &Expr> {
    class
        .arguments
        .as_deref()
        .into_iter()
        .flat_map(|arguments| arguments.args.iter())
}

fn base_root_name(base: &Expr) -> Option<&str> {
    let mut expression = match base {
        Expr::Subscript(subscript) => subscript.value.as_ref(),
        expression => expression,
    };
    while let Expr::Attribute(attribute) = expression {
        expression = attribute.value.as_ref();
    }
    match expression {
        Expr::Name(name) => Some(name.id.as_str()),
        _ => None,
    }
}

fn qualified_name(parent: Option<&str>, name: &str) -> String {
    parent.map_or_else(|| name.to_owned(), |parent| format!("{parent}.{name}"))
}

fn qualified_lexical_name(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{}.<locals>.{name}", scope.join(".<locals>."))
    }
}

/// Local names that an aliased import bound to a member of `dataclasses` or
/// `pydantic`.
///
/// Decorators, annotations, and field calls are matched by the name they are
/// written with, so `from dataclasses import dataclass as dc` would otherwise
/// leave `@dc` unrecognised and the whole class unchecked.
/// `None` against a local name marks one the file bound to more than one
/// member, which makes it resolve to neither.
#[derive(Clone, Debug, Default)]
struct Aliases {
    import_bindings: BTreeSet<String>,
    renamed: BTreeMap<String, Option<String>>,
    dataclasses_members: BTreeSet<String>,
    dataclass_fields: BTreeSet<String>,
    invalidated_field_helpers: BTreeSet<String>,
    dataclass_decorators: BTreeSet<String>,
    pydantic_fields: BTreeSet<String>,
    pydantic_private_attrs: BTreeSet<String>,
    dataclasses_modules: BTreeSet<String>,
    pydantic_modules: BTreeSet<String>,
    enum_classes: BTreeSet<String>,
    enum_modules: BTreeSet<String>,
    staticmethods: BTreeSet<String>,
    classmethods: BTreeSet<String>,
    properties: BTreeSet<String>,
    supers: BTreeSet<String>,
    builtins_modules: BTreeSet<String>,
    class_vars: BTreeSet<String>,
    typing_modules: BTreeSet<String>,
    abc_modules: BTreeSet<String>,
    structural_bases: BTreeSet<String>,
    invalidated_structural_bases: BTreeSet<String>,
    invalidated_import_bindings: BTreeSet<String>,
    type_checking: BTreeSet<String>,
    kw_only_markers: BTreeSet<String>,
}

impl Aliases {
    fn invalidate(&mut self, name: &str) {
        self.import_bindings.remove(name);
        self.renamed.remove(name);
        self.dataclasses_members.remove(name);
        if self.dataclass_fields.remove(name) {
            self.invalidated_field_helpers.insert(name.to_owned());
        }
        self.dataclass_decorators.remove(name);
        if self.pydantic_fields.remove(name) {
            self.invalidated_field_helpers.insert(name.to_owned());
        }
        self.pydantic_private_attrs.remove(name);
        self.dataclasses_modules.remove(name);
        self.pydantic_modules.remove(name);
        self.enum_classes.remove(name);
        self.enum_modules.remove(name);
        self.staticmethods.remove(name);
        self.classmethods.remove(name);
        self.supers.remove(name);
        self.builtins_modules.remove(name);
        self.class_vars.remove(name);
        self.typing_modules.remove(name);
        self.abc_modules.remove(name);
        self.structural_bases.remove(name);
        self.type_checking.remove(name);
        self.kw_only_markers.remove(name);
    }

    fn invalidate_parameter(&mut self, name: &str) {
        if self.builtins_modules.contains(name)
            || self.typing_modules.contains(name)
            || self.abc_modules.contains(name)
            || self.structural_bases.contains(name)
        {
            self.invalidated_structural_bases.insert(name.to_owned());
        }
        // Whatever the caller passes is no more visible than the import it
        // covers, so a base spelled with the parameter's name is still one
        // whose fields this file cannot see.
        if self.import_bindings.contains(name) {
            self.invalidated_import_bindings.insert(name.to_owned());
        }
        self.invalidate(name);
    }

    /// The `dataclasses` or `pydantic` member `name` was imported as, or `name`
    /// itself when the file did not rename anything to it, or renamed more
    /// than one thing to it.
    fn resolve<'a>(&'a self, name: &'a str) -> &'a str {
        self.renamed
            .get(name)
            .and_then(Option::as_deref)
            .unwrap_or(name)
    }

    fn collect_builtin_members(&mut self, import: &ast::StmtImportFrom) {
        if import
            .module
            .as_ref()
            .is_none_or(|module| module.as_str() != "builtins")
        {
            return;
        }
        for alias in &import.names {
            let local = alias.asname.as_ref().unwrap_or(&alias.name).to_string();
            match alias.name.as_str() {
                "staticmethod" => {
                    self.staticmethods.insert(local);
                }
                "classmethod" => {
                    self.classmethods.insert(local);
                }
                "property" => {
                    self.properties.insert(local);
                }
                "super" => {
                    self.supers.insert(local);
                }
                "object" => {
                    self.structural_bases.insert(local);
                }
                _ => {}
            }
        }
    }

    fn collect_typing_members(&mut self, import: &ast::StmtImportFrom) {
        if !import
            .module
            .as_ref()
            .is_some_and(|module| matches!(module.as_str(), "typing" | "typing_extensions"))
        {
            return;
        }
        for alias in &import.names {
            if alias.name.as_str() == "ClassVar" {
                self.class_vars
                    .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
            } else if alias.name.as_str() == "TYPE_CHECKING" {
                self.type_checking
                    .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
            } else if matches!(alias.name.as_str(), "Generic" | "Protocol") {
                self.structural_bases
                    .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
            }
        }
    }

    fn collect_abc_members(&mut self, import: &ast::StmtImportFrom) {
        if import
            .module
            .as_ref()
            .is_none_or(|module| module.as_str() != "abc")
        {
            return;
        }
        for alias in &import.names {
            if alias.name.as_str() == "ABC" {
                self.structural_bases
                    .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
            }
        }
    }

    fn collect_enum_members(&mut self, import: &ast::StmtImportFrom) {
        if import
            .module
            .as_ref()
            .is_none_or(|module| module.as_str() != "enum")
        {
            return;
        }
        for alias in &import.names {
            if alias.name.as_str() == "Enum" {
                self.enum_classes
                    .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
            }
        }
    }

    /// Collect the renaming imports visible in one lexical scope, including
    /// those nested in control-flow blocks but not nested definitions.
    /// The names a plain `import` binds for the modules whose members matter.
    fn collect_module_aliases(&mut self, import: &ast::StmtImport) {
        for alias in &import.names {
            self.import_bindings
                .insert(alias.asname.as_ref().map_or_else(
                    || alias.name.split('.').next().unwrap_or_default().to_owned(),
                    ToString::to_string,
                ));
            if alias.name.as_str() == "dataclasses" {
                self.dataclasses_modules.insert(
                    alias
                        .asname
                        .as_ref()
                        .map_or_else(|| "dataclasses".to_owned(), ToString::to_string),
                );
            } else if alias.name.as_str() == "pydantic" {
                self.pydantic_modules.insert(
                    alias
                        .asname
                        .as_ref()
                        .map_or_else(|| "pydantic".to_owned(), ToString::to_string),
                );
            } else if alias.name.as_str() == "enum" {
                self.enum_modules.insert(
                    alias
                        .asname
                        .as_ref()
                        .map_or_else(|| "enum".to_owned(), ToString::to_string),
                );
            } else if alias.name.as_str() == "builtins" {
                self.builtins_modules.insert(
                    alias
                        .asname
                        .as_ref()
                        .map_or_else(|| "builtins".to_owned(), ToString::to_string),
                );
            } else if matches!(alias.name.as_str(), "typing" | "typing_extensions") {
                self.typing_modules.insert(
                    alias
                        .asname
                        .as_ref()
                        .map_or_else(|| alias.name.to_string(), ToString::to_string),
                );
            } else if alias.name.as_str() == "abc" {
                self.abc_modules.insert(
                    alias
                        .asname
                        .as_ref()
                        .map_or_else(|| "abc".to_owned(), ToString::to_string),
                );
            }
        }
    }

    fn collect(&mut self, statements: &[Stmt]) {
        for statement in statements {
            match statement {
                Stmt::Import(import) => self.collect_module_aliases(import),
                Stmt::ImportFrom(import) => {
                    self.import_bindings.extend(
                        import
                            .names
                            .iter()
                            .filter(|alias| alias.name.as_str() != "*")
                            .map(|alias| alias.asname.as_ref().unwrap_or(&alias.name).to_string()),
                    );
                    self.collect_typing_members(import);
                    self.collect_builtin_members(import);
                    self.collect_abc_members(import);
                    self.collect_enum_members(import);
                    let carries_fields = import.module.as_ref().is_some_and(|module| {
                        matches!(module.split('.').next(), Some("dataclasses" | "pydantic"))
                    });
                    if !carries_fields {
                        continue;
                    }
                    let dataclasses = import
                        .module
                        .as_ref()
                        .is_some_and(|module| module.as_str() == "dataclasses");
                    let pydantic = import
                        .module
                        .as_ref()
                        .is_some_and(|module| module.split('.').next() == Some("pydantic"));
                    for alias in &import.names {
                        let local = alias.asname.as_ref().unwrap_or(&alias.name).to_string();
                        if dataclasses && alias.name.as_str() == "field" {
                            self.invalidated_field_helpers.remove(&local);
                            self.dataclass_fields.insert(local.clone());
                        }
                        if pydantic && alias.name.as_str() == "Field" {
                            self.invalidated_field_helpers.remove(&local);
                            self.pydantic_fields.insert(local.clone());
                        }
                        if pydantic && alias.name.as_str() == "PrivateAttr" {
                            self.pydantic_private_attrs.insert(local.clone());
                        }
                        if dataclasses && alias.name.as_str() == "dataclass" {
                            self.dataclass_decorators.insert(local.clone());
                        }
                        if dataclasses && alias.name.as_str() == "MISSING" {
                            self.dataclasses_members.insert(local.clone());
                        }
                        if dataclasses && alias.name.as_str() == "KW_ONLY" {
                            self.kw_only_markers.insert(local.clone());
                        }
                        let Some(local) = &alias.asname else {
                            continue;
                        };
                        match self.renamed.entry(local.to_string()) {
                            Entry::Vacant(entry) => {
                                entry.insert(Some(alias.name.to_string()));
                            }
                            Entry::Occupied(mut entry) => {
                                if entry.get().as_deref() != Some(alias.name.as_str()) {
                                    entry.insert(None);
                                }
                            }
                        }
                    }
                }
                Stmt::If(branch) => {
                    self.collect(&branch.body);
                    for clause in &branch.elif_else_clauses {
                        self.collect(&clause.body);
                    }
                }
                Stmt::Try(block) => {
                    self.collect(&block.body);
                    self.collect(&block.orelse);
                    self.collect(&block.finalbody);
                    for handler in &block.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        self.collect(&handler.body);
                    }
                }
                Stmt::For(loop_) => {
                    self.collect(&loop_.body);
                    self.collect(&loop_.orelse);
                }
                Stmt::While(loop_) => {
                    self.collect(&loop_.body);
                    self.collect(&loop_.orelse);
                }
                Stmt::Match(block) => {
                    for case in &block.cases {
                        self.collect(&case.body);
                    }
                }
                Stmt::ClassDef(class) => self.collect(&class.body),
                Stmt::With(block) => self.collect(&block.body),
                // In particular, imports in a nested function are local to
                // that function and cannot change names used by this scope.
                _ => {}
            }
        }
    }
}

/// The name a decorator, annotation, or call expression is matched by.
///
/// An attribute keeps its last segment, so `dataclasses.field` reads as
/// `field`. A bare name is resolved through the file's imports first, so
/// `dc` reads as `dataclass` where the file wrote `dataclass as dc`.
fn matched_name<'a>(expression: &'a Expr, aliases: &'a Aliases) -> Option<&'a str> {
    match expression {
        Expr::Name(name) => Some(aliases.resolve(name.id.as_str())),
        Expr::Attribute(attribute) => Some(attribute.attr.as_str()),
        _ => None,
    }
}

fn has_dataclass_decorator(
    class: &ast::StmtClassDef,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> bool {
    class.decorator_list.iter().any(|decorator| {
        let expression = match &decorator.expression {
            Expr::Call(call) => &*call.func,
            expression => expression,
        };
        if matched_name(expression, aliases) != Some("dataclass") {
            return false;
        }
        match expression {
            Expr::Name(name) => {
                !module_bindings.contains(name.id.as_str())
                    || aliases.dataclass_decorators.contains(name.id.as_str())
            }
            Expr::Attribute(attribute) => match attribute.value.as_ref() {
                Expr::Name(module) => {
                    !module_bindings.contains(module.id.as_str())
                        || aliases.dataclasses_modules.contains(module.id.as_str())
                }
                _ => false,
            },
            _ => false,
        }
    })
}

/// Whether the class's decorators leave the generated constructor in place.
///
/// Only `dataclass` itself is known to. Every other decorator is assumed to
/// replace the class or install an `__init__` of its own, which is what makes
/// removing a field default unsafe: the constructor the call sites are
/// rewritten against would not be the one that runs.
///
/// The form the decorator is written in says nothing about what it does. A
/// factory — `@replace()` — returns the callable that is applied, and an
/// attribute — `@mod.replace` — is the same callable reached through its
/// module, so both are judged exactly as a bare name is. This matches how a
/// decorator on a function is treated, where anything but a known descriptor
/// keeps the defaults.
fn class_defaults_are_fixable(class: &ast::StmtClassDef, aliases: &Aliases) -> bool {
    !class.decorator_list.iter().any(|decorator| {
        let expression = match &decorator.expression {
            Expr::Call(call) => &*call.func,
            expression => expression,
        };
        matched_name(expression, aliases) != Some("dataclass")
    })
}

fn class_constructs_safely(
    class: &ast::StmtClassDef,
    aliases: &Aliases,
    metaclass_classes: &BTreeSet<String>,
) -> bool {
    generates_init(class, aliases, metaclass_classes)
        && class_defaults_are_fixable(class, aliases)
        && class_bases_are_fixable(class, aliases)
}

/// A base whose imported structural identity was shadowed in this lexical
/// scope may contribute runtime fields, so its constructor cannot be rewritten.
fn class_bases_are_fixable(class: &ast::StmtClassDef, aliases: &Aliases) -> bool {
    class_bases(class).all(|base| {
        let base = match base {
            Expr::Subscript(subscript) => subscript.value.as_ref(),
            expression => expression,
        };
        let root = match base {
            Expr::Name(name) => Some(name.id.as_str()),
            Expr::Attribute(attribute) => match attribute.value.as_ref() {
                Expr::Name(name) => Some(name.id.as_str()),
                _ => None,
            },
            _ => None,
        };
        root.is_none_or(|name| !aliases.invalidated_structural_bases.contains(name))
    })
}

/// Whether a class names a metaclass of its own.
fn declares_metaclass(class: &ast::StmtClassDef) -> bool {
    class.arguments.as_deref().is_some_and(|arguments| {
        arguments.keywords.iter().any(|keyword| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|name| name.as_str() == "metaclass")
        })
    })
}

/// Whether a class is itself a metaclass, directly or through a local base.
fn defines_metaclass(class: &ast::StmtClassDef, known: &BTreeSet<String>) -> bool {
    class.arguments.as_ref().is_some_and(|arguments| {
        arguments.args.iter().any(|base| {
            matches!(base, Expr::Name(name) if name.id.as_str() == "type" || known.contains(name.id.as_str()))
        })
    })
}

/// A class the file defines, the name it is spelled with from the module, the
/// scopes holding it, and whether a function body defers its creation — one
/// written in a function is built when that function runs, so it sees names
/// the module binds after it.
type LexicalClass<'a> = (&'a ast::StmtClassDef, String, Vec<String>, bool);

fn collect_lexical_classes<'a>(
    suite: &'a [Stmt],
    scope: &mut Vec<String>,
    deferred: bool,
    classes: &mut Vec<LexicalClass<'a>>,
) {
    for statement in suite {
        match statement {
            Stmt::ClassDef(class) => {
                let qualified = qualified_class_name(scope, class.name.as_str());
                classes.push((class, qualified, scope.clone(), deferred));
                scope.push(class.name.to_string());
                collect_lexical_classes(&class.body, scope, deferred, classes);
                scope.pop();
            }
            Stmt::FunctionDef(function) => {
                scope.push(function.name.to_string());
                collect_lexical_classes(&function.body, scope, true, classes);
                scope.pop();
            }
            Stmt::If(branch) => {
                collect_lexical_classes(&branch.body, scope, deferred, classes);
                for clause in &branch.elif_else_clauses {
                    collect_lexical_classes(&clause.body, scope, deferred, classes);
                }
            }
            Stmt::For(loop_) => {
                collect_lexical_classes(&loop_.body, scope, deferred, classes);
                collect_lexical_classes(&loop_.orelse, scope, deferred, classes);
            }
            Stmt::While(loop_) => {
                collect_lexical_classes(&loop_.body, scope, deferred, classes);
                collect_lexical_classes(&loop_.orelse, scope, deferred, classes);
            }
            Stmt::With(block) => collect_lexical_classes(&block.body, scope, deferred, classes),
            Stmt::Try(block) => {
                collect_lexical_classes(&block.body, scope, deferred, classes);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_lexical_classes(&handler.body, scope, deferred, classes);
                }
                collect_lexical_classes(&block.orelse, scope, deferred, classes);
                collect_lexical_classes(&block.finalbody, scope, deferred, classes);
            }
            Stmt::Match(block) => {
                for case in &block.cases {
                    collect_lexical_classes(&case.body, scope, deferred, classes);
                }
            }
            _ => {}
        }
    }
}

/// Same-file classes whose metaclass can replace attributes read on the class.
/// The name a base expression is written with, seeing through a subscript.
///
/// `Parent[int]` builds on `Parent`, so a subclass written that way inherits
/// whatever `Parent` carries, and `pkg.Parent[int]` keeps its dotted spelling
/// for the scope lookup to resolve.
fn base_dotted_name(base: &Expr) -> Option<String> {
    match base {
        Expr::Subscript(subscript) => base_dotted_name(&subscript.value),
        _ => dotted_name(base),
    }
}

fn metaclass_intercepted_classes(suite: &[Stmt]) -> BTreeSet<String> {
    let mut classes = Vec::new();
    collect_lexical_classes(suite, &mut Vec::new(), false, &mut classes);
    let resolve = |name: &str, scope: &[String], before: usize| {
        (0..=scope.len()).rev().find_map(|length| {
            let candidate = qualified_class_name(&scope[..length], name);
            let matches: Vec<usize> = classes[..before]
                .iter()
                .enumerate()
                .filter_map(|(index, (_, qualified, _, _))| {
                    (qualified == &candidate).then_some(index)
                })
                .collect();
            (!matches.is_empty()).then_some(matches)
        })
    };
    let mut metaclasses = BTreeSet::new();
    let mut intercepting_metaclasses = BTreeSet::new();
    let mut intercepted = BTreeSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for (index, (class, _, scope, deferred)) in classes.iter().enumerate() {
            // A class built when a function runs sees every module-level name,
            // including those bound after the function was written.
            let visible = if *deferred { classes.len() } else { index };
            let bases = class
                .arguments
                .as_deref()
                .into_iter()
                .flat_map(|arguments| &arguments.args)
                .filter_map(base_dotted_name);
            let base_names: Vec<String> = bases.collect();
            let is_metaclass = base_names.iter().any(|base| {
                base == "type"
                    || resolve(base, scope, visible)
                        .is_some_and(|bases| bases.iter().any(|base| metaclasses.contains(base)))
            });
            if is_metaclass && metaclasses.insert(index) {
                changed = true;
            }
            let defines_getattribute = class.body.iter().any(|statement| {
                matches!(statement, Stmt::FunctionDef(function) if function.name.as_str() == "__getattribute__")
            });
            if is_metaclass
                && (defines_getattribute
                    || base_names.iter().any(|base| {
                        resolve(base, scope, visible).is_some_and(|bases| {
                            bases
                                .iter()
                                .any(|base| intercepting_metaclasses.contains(base))
                        })
                    }))
                && intercepting_metaclasses.insert(index)
            {
                changed = true;
            }
            let declared_interceptor = class.arguments.as_deref().is_some_and(|arguments| {
                arguments.keywords.iter().any(|keyword| {
                    keyword
                        .arg
                        .as_ref()
                        .is_some_and(|name| name.as_str() == "metaclass")
                        && dotted_name(&keyword.value).is_some_and(|name| {
                            resolve(&name, scope, visible).is_some_and(|bases| {
                                bases
                                    .iter()
                                    .any(|base| intercepting_metaclasses.contains(base))
                            })
                        })
                })
            });
            if (declared_interceptor
                || base_names.iter().any(|base| {
                    resolve(base, scope, visible)
                        .is_some_and(|bases| bases.iter().any(|base| intercepted.contains(base)))
                }))
                && intercepted.insert(index)
            {
                changed = true;
            }
        }
    }
    intercepted
        .into_iter()
        .map(|index| classes[index].1.clone())
        .collect()
}

/// Whether a class takes a metaclass from a base defined in the same file.
fn inherits_metaclass(class: &ast::StmtClassDef, metaclass_classes: &BTreeSet<String>) -> bool {
    class.arguments.as_deref().is_some_and(|arguments| {
        arguments
            .args
            .iter()
            .filter_map(base_dotted_name)
            .any(|base| metaclass_classes.contains(&base))
    })
}

/// Whether the decorator leaves the class with a generated `__init__`.
fn generates_init(
    class: &ast::StmtClassDef,
    aliases: &Aliases,
    metaclass_classes: &BTreeSet<String>,
) -> bool {
    // `dataclasses` checks the completed class namespace, not just method
    // definitions. An assignment, import, or nested definition under this
    // name suppresses generation just as `def __init__` does.
    let defines_init = BoundNames::of_body(&class.body).names.contains("__init__");
    // A metaclass controls construction before the generated initializer is
    // reached. Its `__call__` signature is not recoverable from this class
    // body, so field arguments cannot safely be added. A metaclass is
    // inherited, so a base carrying one makes the subclass unsafe too.
    let has_metaclass = declares_metaclass(class) || inherits_metaclass(class, metaclass_classes);
    !defines_init
        && !has_metaclass
        && !class.decorator_list.iter().any(|decorator| {
            let Expr::Call(call) = &decorator.expression else {
                return false;
            };
            if matched_name(&call.func, aliases) != Some("dataclass") {
                return false;
            }
            call.arguments.keywords.iter().any(|keyword| {
                keyword.arg.as_ref().is_none_or(|name| {
                    name.as_str() == "init"
                        && matches!(
                            Truthiness::from_expr(&keyword.value, |_| false),
                            Truthiness::False | Truthiness::Falsey | Truthiness::None
                        )
                })
            })
        })
}

/// Direct method aliases created in a class namespace. Python applies the
/// descriptor protocol to both names, so calls through either spelling have
/// the same implicit receiver and parameters.
/// How a call through an alias passes its receiver. A `staticmethod` wrapper
/// takes none and a `classmethod` wrapper takes the class, whatever the
/// original method declared.
fn alias_receiver(kind: MethodAliasKind, original: Receiver) -> Receiver {
    match kind {
        MethodAliasKind::Direct | MethodAliasKind::Property => original,
        MethodAliasKind::Static => Receiver::None,
        MethodAliasKind::Class => Receiver::Class,
    }
}

/// What class-body alias collection reads to recognise a descriptor wrapper
/// written under an alias or qualified through a `builtins` import.
#[derive(Clone, Copy)]
struct AliasContext<'a> {
    aliases: &'a Aliases,
    module_bindings: &'a BTreeSet<String>,
}

fn class_method_aliases(
    class: &ast::StmtClassDef,
    context: AliasContext<'_>,
) -> BTreeMap<String, Vec<MethodAlias>> {
    let mut aliases = BTreeMap::<String, Vec<MethodAlias>>::new();
    let mut origins = BTreeMap::<String, (String, MethodAliasKind, Option<String>)>::new();
    collect_class_method_aliases(&class.body, context, &mut aliases, &mut origins);
    aliases
}

fn collect_class_method_aliases(
    statements: &[Stmt],
    context: AliasContext<'_>,
    aliases: &mut BTreeMap<String, Vec<MethodAlias>>,
    origins: &mut BTreeMap<String, (String, MethodAliasKind, Option<String>)>,
) {
    for statement in statements {
        match statement {
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    record_method_alias(target, &assign.value, context, aliases, origins);
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = &assign.value {
                    record_method_alias(&assign.target, value, context, aliases, origins);
                }
            }
            Stmt::Expr(expression) => {
                if let Expr::Named(named) = expression.value.as_ref() {
                    record_method_alias(&named.target, &named.value, context, aliases, origins);
                }
            }
            Stmt::If(branch) => {
                collect_alias_branches(
                    std::iter::once(branch.body.as_slice()).chain(
                        branch
                            .elif_else_clauses
                            .iter()
                            .map(|clause| clause.body.as_slice()),
                    ),
                    context,
                    aliases,
                    origins,
                );
            }
            Stmt::For(loop_) => collect_alias_branches(
                [loop_.body.as_slice(), loop_.orelse.as_slice()],
                context,
                aliases,
                origins,
            ),
            Stmt::While(loop_) => collect_alias_branches(
                [loop_.body.as_slice(), loop_.orelse.as_slice()],
                context,
                aliases,
                origins,
            ),
            Stmt::With(block) => {
                let mut branch_origins = origins.clone();
                collect_class_method_aliases(&block.body, context, aliases, &mut branch_origins);
            }
            Stmt::Try(block) => {
                let handlers = block.handlers.iter().map(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    handler.body.as_slice()
                });
                collect_alias_branches(
                    [
                        block.body.as_slice(),
                        block.orelse.as_slice(),
                        block.finalbody.as_slice(),
                    ]
                    .into_iter()
                    .chain(handlers),
                    context,
                    aliases,
                    origins,
                );
            }
            Stmt::Match(block) => collect_alias_branches(
                block.cases.iter().map(|case| case.body.as_slice()),
                context,
                aliases,
                origins,
            ),
            _ => {}
        }
    }
}

fn collect_alias_branches<'a>(
    branches: impl IntoIterator<Item = &'a [Stmt]>,
    context: AliasContext<'_>,
    aliases: &mut BTreeMap<String, Vec<MethodAlias>>,
    origins: &BTreeMap<String, (String, MethodAliasKind, Option<String>)>,
) {
    for branch in branches {
        let mut branch_origins = origins.clone();
        collect_class_method_aliases(branch, context, aliases, &mut branch_origins);
    }
}

fn record_method_alias(
    target: &Expr,
    value: &Expr,
    context: AliasContext<'_>,
    aliases: &mut BTreeMap<String, Vec<MethodAlias>>,
    origins: &mut BTreeMap<String, (String, MethodAliasKind, Option<String>)>,
) {
    match (target, value) {
        (Expr::Name(alias), value) => {
            let Some((original, kind, original_class)) =
                method_alias_origin(value, context, origins)
            else {
                return;
            };
            aliases
                .entry(original.clone())
                .or_default()
                .push(MethodAlias {
                    name: alias.id.to_string(),
                    kind,
                    original_class: original_class.clone(),
                });
            origins.insert(alias.id.to_string(), (original, kind, original_class));
        }
        (Expr::Tuple(targets), Expr::Tuple(values)) if targets.elts.len() == values.elts.len() => {
            for (target, value) in targets.elts.iter().zip(&values.elts) {
                record_method_alias(target, value, context, aliases, origins);
            }
        }
        (Expr::List(targets), Expr::List(values)) if targets.elts.len() == values.elts.len() => {
            for (target, value) in targets.elts.iter().zip(&values.elts) {
                record_method_alias(target, value, context, aliases, origins);
            }
        }
        _ => {}
    }
}

fn method_alias_origin(
    value: &Expr,
    context: AliasContext<'_>,
    origins: &BTreeMap<String, (String, MethodAliasKind, Option<String>)>,
) -> Option<(String, MethodAliasKind, Option<String>)> {
    match value {
        Expr::Name(original) => Some(
            origins
                .get(original.id.as_str())
                .cloned()
                .unwrap_or_else(|| (original.id.to_string(), MethodAliasKind::Direct, None)),
        ),
        Expr::Attribute(attribute) => {
            let Expr::Name(class) = attribute.value.as_ref() else {
                return None;
            };
            Some((
                attribute.attr.to_string(),
                MethodAliasKind::Direct,
                Some(class.id.to_string()),
            ))
        }
        Expr::Call(call)
            if call.arguments.args.len() == 1 && call.arguments.keywords.is_empty() =>
        {
            let kind =
                descriptor_wrapper_kind(&call.func, context.aliases, context.module_bindings)?;
            // The wrapper decides how the alias is called; what it wraps
            // decides which method it names, so resolve that the same way a
            // bare value is resolved. `staticmethod(Base.target)` names the
            // same method as a plain `Base.target`.
            let (root, _, original_class) =
                method_alias_origin(&call.arguments.args[0], context, origins)?;
            Some((root, kind, original_class))
        }
        _ => None,
    }
}

/// Whether the decorator says `kw_only=True`, making every field of the class
/// keyword-only in the generated constructor.
fn decorator_says_kw_only(class: &ast::StmtClassDef, aliases: &Aliases) -> bool {
    class.decorator_list.iter().any(|decorator| {
        let Expr::Call(call) = &decorator.expression else {
            return false;
        };
        if matched_name(&call.func, aliases) != Some("dataclass") {
            return false;
        }
        call.arguments
            .keywords
            .iter()
            .any(|keyword| keyword_is(keyword, "kw_only") == Some(true))
    })
}

/// Whether a `field(...)` call says `kw_only=`, and what it said. `None` where
/// it does not say, so the class-wide setting stands.
fn field_says_kw_only(
    value: &Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> Option<bool> {
    let Expr::Call(call) = value else {
        return None;
    };
    if !is_dataclass_field(&call.func, aliases, module_bindings) {
        return None;
    }
    call.arguments
        .keywords
        .iter()
        .find_map(|keyword| keyword_is(keyword, "kw_only"))
}

/// The truth value a keyword argument was given, when it is named `name` and
/// its value has constant truthiness.
fn keyword_is(keyword: &ast::Keyword, name: &str) -> Option<bool> {
    if keyword.arg.as_ref()?.as_str() != name {
        return None;
    }
    match Truthiness::from_expr(&keyword.value, |_| false) {
        Truthiness::True | Truthiness::Truthy => Some(true),
        Truthiness::False | Truthiness::Falsey | Truthiness::None => Some(false),
        Truthiness::Unknown => None,
    }
}

/// Whether an annotation is the `KW_ONLY` marker, which declares no field.
fn annotates_kw_only(annotation: &Expr, aliases: &Aliases) -> bool {
    match annotation {
        Expr::Name(name) => aliases.kw_only_markers.contains(name.id.as_str()),
        Expr::Attribute(attribute) if attribute.attr.as_str() == "KW_ONLY" => {
            matches!(attribute.value.as_ref(), Expr::Name(name) if aliases.dataclasses_modules.contains(name.id.as_str()))
        }
        Expr::StringLiteral(literal) => {
            parse_expression(literal.value.to_str().trim()).is_ok_and(|parsed| {
                match parsed.expr() {
                    Expr::StringLiteral(_) => false,
                    expression => annotates_kw_only(expression, aliases),
                }
            })
        }
        _ => false,
    }
}

/// Whether a field is declared `init=False`, which keeps it out of the
/// constructor even though it is still a field.
fn field_excluded_from_init(
    value: &Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> bool {
    let Expr::Call(call) = value else {
        return false;
    };
    is_dataclass_field(&call.func, aliases, module_bindings)
        && call.arguments.keywords.iter().any(|keyword| {
            keyword.arg.as_ref().is_none_or(|name| {
                name.as_str() == "init"
                    && matches!(
                        Truthiness::from_expr(&keyword.value, |_| false),
                        Truthiness::False | Truthiness::Falsey | Truthiness::None
                    )
            })
        })
}

/// Whether Pydantic accepts a name other than the Python field name when
/// validating constructor input. Without evaluating model configuration, the
/// original field default is safer than inserting a keyword that may fail.
fn pydantic_field_has_validation_alias(
    value: &Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> bool {
    let Some((call, FieldCall::Pydantic)) = field_call(value, aliases, module_bindings) else {
        return false;
    };
    call.arguments.keywords.iter().any(|keyword| {
        keyword.arg.as_ref().is_some_and(|name| {
            matches!(name.as_str(), "alias" | "validation_alias")
                && !matches!(
                    Truthiness::from_expr(&keyword.value, |_| false),
                    Truthiness::None
                )
        })
    })
}

fn is_class_var(statement: &Stmt, aliases: &Aliases) -> bool {
    let Stmt::AnnAssign(assign) = statement else {
        return false;
    };
    annotates_class_var(&assign.annotation, aliases)
}

/// `PrivateAttr` initializes per-instance private state but does not declare a
/// model field or constructor parameter.
fn is_pydantic_private_attr(value: Option<&Expr>, aliases: &Aliases) -> bool {
    let Some(Expr::Call(call)) = value else {
        return false;
    };
    match call.func.as_ref() {
        Expr::Name(name) => aliases.pydantic_private_attrs.contains(name.id.as_str()),
        Expr::Attribute(attribute) if attribute.attr.as_str() == "PrivateAttr" => {
            matches!(attribute.value.as_ref(), Expr::Name(module) if aliases.pydantic_modules.contains(module.id.as_str()))
        }
        _ => false,
    }
}

/// Whether an annotation names `ClassVar`, bare, qualified, or quoted.
///
/// `dataclasses` resolves a string annotation textually, so
/// `x: "ClassVar[int]" = 1` really is a class variable rather than a field.
fn annotates_class_var(annotation: &Expr, aliases: &Aliases) -> bool {
    match annotation {
        Expr::Name(name) => aliases.class_vars.contains(name.id.as_str()),
        Expr::Attribute(attribute) if attribute.attr.as_str() == "ClassVar" => {
            matches!(attribute.value.as_ref(), Expr::Name(name) if aliases.typing_modules.contains(name.id.as_str()))
        }
        Expr::Subscript(subscript) => annotates_class_var(&subscript.value, aliases),
        // Quoted annotations are only one level deep: the contents of
        // `"ClassVar[int]"` are an expression, not another string. Surrounding
        // whitespace is trimmed because `dataclasses` accepts it and the
        // parser would reject the leading indentation.
        Expr::StringLiteral(literal) => {
            parse_expression(literal.value.to_str().trim()).is_ok_and(|parsed| {
                match parsed.expr() {
                    Expr::StringLiteral(_) => false,
                    expression => annotates_class_var(expression, aliases),
                }
            })
        }
        _ => false,
    }
}

/// Which library's field helper a call is, which decides what its arguments
/// mean.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldCall {
    /// `dataclasses.field`, where the first argument is a real default.
    Dataclasses,
    /// `pydantic.Field`, where that argument may be `...` for "required".
    Pydantic,
}

/// The call declaring a field's default, if the value is one. Recognised by
/// name, as decorators are, so `field`, `dataclasses.field`, `Field`, and
/// `pydantic.Field` all count.
fn field_call<'a>(
    value: &'a Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> Option<(&'a ast::ExprCall, FieldCall)> {
    let Expr::Call(call) = value else {
        return None;
    };
    match matched_name(&call.func, aliases)? {
        "field" if is_dataclass_field(&call.func, aliases, module_bindings) => {
            Some((call, FieldCall::Dataclasses))
        }
        "Field" if is_pydantic_field(&call.func, aliases, module_bindings) => {
            Some((call, FieldCall::Pydantic))
        }
        _ => None,
    }
}

fn is_dataclass_field(
    function: &Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> bool {
    if matched_name(function, aliases) != Some("field") {
        return false;
    }
    match function {
        Expr::Name(name) => {
            !module_bindings.contains(name.id.as_str())
                || aliases.dataclass_fields.contains(name.id.as_str())
        }
        Expr::Attribute(attribute) => match attribute.value.as_ref() {
            Expr::Name(module) => {
                !module_bindings.contains(module.id.as_str())
                    || aliases.dataclasses_modules.contains(module.id.as_str())
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_pydantic_field(
    function: &Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> bool {
    match function {
        Expr::Name(name) => {
            !module_bindings.contains(name.id.as_str())
                || aliases.pydantic_fields.contains(name.id.as_str())
        }
        Expr::Attribute(attribute) => match attribute.value.as_ref() {
            Expr::Name(module) => {
                !module_bindings.contains(module.id.as_str())
                    || aliases.pydantic_modules.contains(module.id.as_str())
            }
            _ => false,
        },
        _ => false,
    }
}

struct FieldDefault {
    kind: &'static str,
    fix: TextRange,
    /// Source text to pass at call sites, when it can be reproduced.
    value: Option<String>,
}

fn field_default(
    value: &Expr,
    annotation_end: TextSize,
    source: &str,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> Option<FieldDefault> {
    let plain = || FieldDefault {
        kind: "default",
        fix: TextRange::new(annotation_end, value.end()),
        value: literal_text(value, source),
    };
    let Some((call, style)) = field_call(value, aliases, module_bindings) else {
        return Some(plain());
    };
    if let Some(first) = call.arguments.args.first() {
        if style == FieldCall::Dataclasses && is_dataclasses_missing(first, aliases) {
            return None;
        }
        // `x: int = Field(...)` is pydantic's way of writing a field with no
        // default at all, so there is nothing to report or remove.
        if style == FieldCall::Pydantic && first.is_ellipsis_literal_expr() {
            return None;
        }
        return Some(FieldDefault {
            kind: "default",
            fix: field_argument_removal_range(
                first.range(),
                call.arguments
                    .args
                    .get(1)
                    .map(Ranged::range)
                    .or_else(|| call.arguments.keywords.first().map(Ranged::range)),
                None,
                call.arguments.range(),
                source,
            ),
            value: literal_text(first, source),
        });
    }
    let (index, keyword) = call
        .arguments
        .keywords
        .iter()
        .enumerate()
        .find(|(_, keyword)| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|name| matches!(name.as_str(), "default" | "default_factory"))
        })?;
    if style == FieldCall::Dataclasses && is_dataclasses_missing(&keyword.value, aliases) {
        return None;
    }
    // `Field(default=...)` says required just as `Field(...)` does.
    if style == FieldCall::Pydantic && keyword.value.is_ellipsis_literal_expr() {
        return None;
    }
    let factory = keyword
        .arg
        .as_ref()
        .is_some_and(|name| name.as_str() == "default_factory");
    let kind = if factory {
        "default factory"
    } else {
        "default"
    };
    let value = if factory {
        factory_call_text(&keyword.value, module_bindings)
    } else {
        literal_text(&keyword.value, source)
    };
    Some(FieldDefault {
        kind,
        value,
        fix: field_argument_removal_range(
            keyword.range(),
            call.arguments.keywords.get(index + 1).map(Ranged::range),
            index
                .checked_sub(1)
                .and_then(|previous| call.arguments.keywords.get(previous))
                .map(Ranged::range),
            call.arguments.range(),
            source,
        ),
    })
}

fn is_dataclasses_missing(expression: &Expr, aliases: &Aliases) -> bool {
    match expression {
        Expr::Name(name) => aliases.dataclasses_members.contains(name.id.as_str()),
        Expr::Attribute(attribute) if attribute.attr.as_str() == "MISSING" => {
            matches!(attribute.value.as_ref(), Expr::Name(name) if aliases.dataclasses_modules.contains(name.id.as_str()))
        }
        _ => false,
    }
}

fn argument_removal_range(
    target: TextRange,
    next: Option<TextRange>,
    previous: Option<TextRange>,
) -> TextRange {
    if let Some(next) = next {
        TextRange::new(target.start(), next.start())
    } else if let Some(previous) = previous {
        TextRange::new(previous.end(), target.end())
    } else {
        target
    }
}

fn field_argument_removal_range(
    target: TextRange,
    next: Option<TextRange>,
    previous: Option<TextRange>,
    arguments: TextRange,
    source: &str,
) -> TextRange {
    let range = argument_removal_range(target, next, previous);
    if next.is_some() || previous.is_some() {
        return range;
    }
    let tail = &source[target.end().to_usize()..arguments.end().to_usize()];
    let Some(comma) = tail.find(',') else {
        return range;
    };
    if !tail[..comma].chars().all(char::is_whitespace) {
        return range;
    }
    TextRange::new(target.start(), target.end() + text_size(comma + 1))
}

/// The descriptor a class-body wrapper call applies, written bare, under an
/// alias, or qualified through a `builtins` import — the same spellings the
/// decorator forms accept.
fn descriptor_wrapper_kind(
    expression: &Expr,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> Option<MethodAliasKind> {
    match expression {
        Expr::Name(name) => {
            let bare = !module_bindings.contains(name.id.as_str());
            match name.id.as_str() {
                "staticmethod" if bare => Some(MethodAliasKind::Static),
                "classmethod" if bare => Some(MethodAliasKind::Class),
                "property" if bare => Some(MethodAliasKind::Property),
                _ if aliases.staticmethods.contains(name.id.as_str()) => {
                    Some(MethodAliasKind::Static)
                }
                _ if aliases.classmethods.contains(name.id.as_str()) => {
                    Some(MethodAliasKind::Class)
                }
                _ if aliases.properties.contains(name.id.as_str()) => {
                    Some(MethodAliasKind::Property)
                }
                _ => None,
            }
        }
        Expr::Attribute(attribute) => {
            let Expr::Name(module) = attribute.value.as_ref() else {
                return None;
            };
            if !aliases.builtins_modules.contains(module.id.as_str()) {
                return None;
            }
            match attribute.attr.as_str() {
                "staticmethod" => Some(MethodAliasKind::Static),
                "classmethod" => Some(MethodAliasKind::Class),
                "property" => Some(MethodAliasKind::Property),
                _ => None,
            }
        }
        _ => None,
    }
}

/// What a method is given ahead of its written arguments, from its decorators.
fn method_receiver(
    function: &ast::StmtFunctionDef,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> Receiver {
    let is_staticmethod = |expression: &Expr| match expression {
        Expr::Name(name) => {
            (name.id.as_str() == "staticmethod" && !module_bindings.contains("staticmethod"))
                || aliases.staticmethods.contains(name.id.as_str())
        }
        Expr::Attribute(attribute) if attribute.attr.as_str() == "staticmethod" => {
            matches!(attribute.value.as_ref(), Expr::Name(name) if aliases.builtins_modules.contains(name.id.as_str()))
        }
        _ => false,
    };
    let is_classmethod = |expression: &Expr| match expression {
        Expr::Name(name) => {
            (name.id.as_str() == "classmethod" && !module_bindings.contains("classmethod"))
                || aliases.classmethods.contains(name.id.as_str())
        }
        Expr::Attribute(attribute) if attribute.attr.as_str() == "classmethod" => {
            matches!(attribute.value.as_ref(), Expr::Name(name) if aliases.builtins_modules.contains(name.id.as_str()))
        }
        _ => false,
    };
    if function
        .decorator_list
        .iter()
        .any(|decorator| is_staticmethod(&decorator.expression))
    {
        Receiver::None
    } else if function
        .decorator_list
        .iter()
        .any(|decorator| is_classmethod(&decorator.expression))
    {
        Receiver::Class
    } else {
        Receiver::Instance
    }
}

/// Whether attribute access invokes this method through Python's `property`
/// descriptor, leaving no explicit argument list that the fixer can update.
fn is_property(
    function: &ast::StmtFunctionDef,
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> bool {
    function.decorator_list.iter().any(|decorator| {
        match &decorator.expression {
            Expr::Name(name) => {
                (name.id.as_str() == "property" && !module_bindings.contains("property"))
                    || aliases.properties.contains(name.id.as_str())
            }
            Expr::Attribute(attribute) if attribute.attr.as_str() == "property" => {
                matches!(attribute.value.as_ref(), Expr::Name(name) if aliases.builtins_modules.contains(name.id.as_str()))
            }
            _ => false,
        }
    })
}

/// The source text of a default that a call site can repeat verbatim.
///
/// Only self-contained literals qualify. A default such as `SENTINEL` or
/// `Path.cwd()` depends on names the caller may not have imported, and copying
/// it would change what the call means, so those are left to the reader.
fn literal_text(expression: &Expr, source: &str) -> Option<String> {
    is_repeatable_literal(expression).then(|| {
        source[expression.range().start().to_usize()..expression.range().end().to_usize()]
            .to_owned()
    })
}

fn is_repeatable_literal(expression: &Expr) -> bool {
    match expression {
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::UnaryOp(unary) => is_repeatable_literal(&unary.operand),
        // This includes container literals: each evaluation creates a new
        // object, unlike the single default object created at definition time.
        _ => false,
    }
}

/// The value a `default_factory` produces, for factories whose result can be
/// written as a literal. Any other factory is left alone.
fn factory_call_text(factory: &Expr, module_bindings: &BTreeSet<String>) -> Option<String> {
    let Expr::Name(name) = factory else {
        return None;
    };
    if module_bindings.contains(name.id.as_str()) {
        return None;
    }
    match name.id.as_str() {
        "list" => Some("[]".to_owned()),
        "dict" => Some("{}".to_owned()),
        "tuple" => Some("()".to_owned()),
        _ => None,
    }
}

/// Module-level names that something other than an import binds.
///
/// The module body has run in full before any function it defines is called,
/// so a `def`, a `class`, or an assignment that takes a name over leaves the
/// import behind however the two are ordered. The names a module ends up
/// bound to cannot tell the two apart on their own, which is why the imports
/// are the one kind of statement passed over here. A conditional import is
/// still an import, so the branches that can hold one are looked into rather
/// than counted whole.
fn module_rebindings(statements: &[Stmt]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    collect_module_rebindings(statements, &mut names);
    names
}

fn collect_module_rebindings(statements: &[Stmt], found: &mut BTreeSet<String>) {
    for statement in statements {
        match statement {
            Stmt::Import(_) | Stmt::ImportFrom(_) => {}
            Stmt::If(branch) => {
                collect_module_rebindings(&branch.body, found);
                for clause in &branch.elif_else_clauses {
                    collect_module_rebindings(&clause.body, found);
                }
            }
            // An `except` target is unbound again as the handler ends, so the
            // handler binds nothing that outlives it.
            Stmt::Try(block) => {
                collect_module_rebindings(&block.body, found);
                collect_module_rebindings(&block.orelse, found);
                collect_module_rebindings(&block.finalbody, found);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_module_rebindings(&handler.body, found);
                }
            }
            _ => found.extend(BoundNames::of_module(std::slice::from_ref(statement))),
        }
    }
}

/// Find the calls in `source` that relied on a default `--fix` removed, and
/// build the edits that pass that default explicitly instead.
fn rewrite_calls(
    path: &Path,
    physical: &Path,
    source: &str,
    definitions: &Definitions,
    known: &BTreeSet<&Path>,
) -> Result<FileCallSites, String> {
    let parsed = parse_module(source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let mut aliases = Aliases::default();
    aliases.collect(parsed.suite());
    let mut rewriter = Rewriter {
        path,
        physical,
        source,
        definitions,
        aliases,
        postponed_annotations: is_stub(path)
            || parsed.suite().iter().any(|statement| {
                matches!(statement, Stmt::ImportFrom(import)
                    if import.module.as_ref().is_some_and(|module| module.as_str() == "__future__")
                        && import.names.iter().any(|alias| alias.name.as_str() == "annotations"))
            }),
        module_bindings: BoundNames::of_module(parsed.suite()),
        module_rebindings: module_rebindings(parsed.suite()),
        bindings: vec![BTreeMap::new()],
        binding_scope_depths: vec![0],
        lambda_scope_depths: Vec::new(),
        class_bindings: Vec::new(),
        conditional_class_definitions: Vec::new(),
        invalidated_bindings: BTreeSet::new(),
        rebound_classes: BTreeSet::new(),
        known,
        classes: Vec::new(),
        class_scope_depths: Vec::new(),
        class_direct_statements: Vec::new(),
        lexical_is_class: Vec::new(),
        implicit_receivers: Vec::new(),
        implicit_receiver_classes: Vec::new(),
        lexical_scope: Vec::new(),
        called: BTreeSet::new(),
        scopes: Vec::new(),
        lines: LineIndex::new(source),
        edits: Vec::new(),
        skipped: Vec::new(),
        retained: BTreeSet::new(),
    };
    for statement in parsed.suite() {
        rewriter.visit_stmt(statement);
    }
    Ok(FileCallSites {
        edits: rewriter.edits,
        skipped: rewriter.skipped,
        retained: rewriter.retained,
    })
}

/// Record what each imported name in a file refers to.
///
/// Imports nested in functions or `if TYPE_CHECKING` blocks bind names just as
/// top-level ones do, so the whole tree is walked.
fn collect_bindings(
    suite: &[Stmt],
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &mut BTreeMap<String, Binding>,
) {
    for statement in suite {
        match statement {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let module = alias.name.as_str();
                    // The binding table is keyed by the expression prefix a
                    // call uses. Although `import a.b` binds `a`, the imported
                    // module is reached through the full `a.b` expression.
                    let bound = match alias.asname.as_ref() {
                        Some(name) => name.to_string(),
                        None => module.to_owned(),
                    };
                    if let Some(file) = resolve_module(module, 0, importer, known) {
                        bindings.insert(bound, Binding::Module(file.clone()));
                        if alias.asname.is_none() {
                            if let Some(top) = module.split('.').next().filter(|top| *top != module)
                            {
                                if let Some(package) = top_package_of_resolved(module, &file, known)
                                {
                                    bindings.insert(top.to_owned(), Binding::Module(package));
                                }
                            }
                        }
                    } else {
                        // The import still replaces the local name at runtime;
                        // an earlier checked binding must not survive merely
                        // because this target is outside the checked file set.
                        bindings.remove(&bound);
                    }
                }
            }
            Stmt::ImportFrom(import) => {
                let module = import.module.as_ref().map_or("", ast::Identifier::as_str);
                let parent = resolve_module(module, import.level, importer, known);
                for alias in &import.names {
                    let name = alias.name.as_str();
                    if name == "*" {
                        continue;
                    }
                    let bound = alias
                        .asname
                        .as_ref()
                        .map_or_else(|| name.to_owned(), ToString::to_string);
                    // `from package import module` names a module, not a
                    // symbol, when it resolves to a file of its own. So does
                    // `from . import module`, the idiomatic way to import a
                    // sibling, where there is no module name to build on and
                    // the level alone says where to look.
                    let dotted = match import.module.as_ref() {
                        Some(parent) => Some(format!("{}.{name}", parent.as_str())),
                        None if import.level > 0 => Some(name.to_owned()),
                        None => None,
                    };
                    let submodule = dotted
                        .and_then(|dotted| resolve_module(&dotted, import.level, importer, known));
                    let package_member = parent
                        .as_ref()
                        .filter(|file| source_binds_name(file, name))
                        .cloned();
                    // `from . import name` reaches the submodule, unless the
                    // package defines `name` itself: the package attribute is
                    // already set by the time the submodule would be imported,
                    // so that is what Python hands out.
                    let implicit_sibling = import.module.is_none()
                        && import.level > 0
                        && !parent
                            .as_ref()
                            .is_some_and(|file| source_defines_symbol(file, name));
                    let binding = match (package_member, submodule, &parent) {
                        (_, Some(file), _) if implicit_sibling => Binding::Module(file),
                        (Some(file), _, _) => Binding::Symbol(file, name.to_owned()),
                        (None, Some(file), _) => Binding::Module(file),
                        (None, None, Some(file)) => Binding::Symbol(file.clone(), name.to_owned()),
                        (None, None, None) => {
                            // Python still replaces the local name when the
                            // imported module is outside the checked file set.
                            bindings.remove(&bound);
                            continue;
                        }
                    };
                    bindings.insert(bound, binding);
                }
            }
            // An annotated binding names the same thing a plain one does, so
            // `target: Final = api.target` has to be followed too.
            Stmt::Assign(_) | Stmt::AnnAssign(_) => {
                collect_assignment_binding(statement, bindings);
            }
            Stmt::If(branch) => {
                collect_conditional_bindings(branch, importer, known, bindings);
            }
            Stmt::Try(block) => {
                collect_try_bindings(block, importer, known, bindings);
            }
            Stmt::For(loop_) => {
                collect_bindings(&loop_.body, importer, known, bindings);
                collect_bindings(&loop_.orelse, importer, known, bindings);
            }
            Stmt::While(loop_) => {
                collect_bindings(&loop_.body, importer, known, bindings);
                collect_bindings(&loop_.orelse, importer, known, bindings);
            }
            Stmt::With(block) => {
                collect_bindings(&block.body, importer, known, bindings);
            }
            Stmt::Match(block) => {
                for case in &block.cases {
                    collect_bindings(&case.body, importer, known, bindings);
                }
            }
            // Definitions introduce lexical scopes whose imports are collected
            // separately when the rewriter enters them.
            _ => {}
        }
    }
}

/// Resolve a simple assignment alias through the imports already executed.
fn assignment_binding(value: &Expr, bindings: &BTreeMap<String, Binding>) -> Option<Binding> {
    match value {
        Expr::Name(name) => bindings.get(name.id.as_str()).cloned(),
        Expr::Attribute(attribute) => {
            let module = dotted_name(&attribute.value)?;
            let Binding::Module(file) = bindings.get(&module)? else {
                return None;
            };
            Some(Binding::Symbol(file.clone(), attribute.attr.to_string()))
        }
        _ => None,
    }
}

fn collect_try_bindings(
    block: &ast::StmtTry,
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &mut BTreeMap<String, Binding>,
) {
    let initial = bindings.clone();
    let mut success = initial.clone();
    collect_bindings(&block.body, importer, known, &mut success);
    collect_bindings(&block.orelse, importer, known, &mut success);
    let mut outcomes = vec![success];
    for handler in &block.handlers {
        let ast::ExceptHandler::ExceptHandler(handler) = handler;
        let mut outcome = initial.clone();
        collect_bindings(&handler.body, importer, known, &mut outcome);
        outcomes.push(outcome);
    }
    for outcome in &mut outcomes {
        collect_bindings(&block.finalbody, importer, known, outcome);
    }
    retain_common_bindings(bindings, &outcomes, initial);
}

fn retain_common_bindings(
    bindings: &mut BTreeMap<String, Binding>,
    outcomes: &[BTreeMap<String, Binding>],
    fallback: BTreeMap<String, Binding>,
) {
    *bindings = outcomes.first().cloned().unwrap_or(fallback);
    bindings.retain(|name, binding| {
        outcomes
            .iter()
            .skip(1)
            .all(|path| path.get(name) == Some(binding))
    });
}

/// Whether none of an `if` statement's suites can run.
fn if_cannot_run(branch: &ast::StmtIf) -> bool {
    let falsey = |test: &Expr| {
        matches!(
            Truthiness::from_expr(test, |_| false),
            Truthiness::False | Truthiness::Falsey | Truthiness::None
        )
    };
    falsey(&branch.test)
        && branch
            .elif_else_clauses
            .iter()
            .all(|clause| clause.test.as_ref().is_some_and(falsey))
}

/// Reconcile the bindings a suite inside a function changed against the ones
/// it started from.
///
/// A suite that cannot run leaves the earlier bindings standing. One that may
/// or may not run leaves every name it rebinds uncertain, and an uncertain
/// name is better left alone than rewritten against a guess.
fn reconcile_suite_bindings(
    statement: &Stmt,
    before: &BTreeMap<String, Binding>,
    after: &mut BTreeMap<String, Binding>,
) {
    let unreachable = match statement {
        Stmt::If(branch) => if_cannot_run(branch),
        // A loop that never iterates still runs its `else`, so only one
        // without that suite leaves nothing behind.
        Stmt::While(loop_) => {
            loop_.orelse.is_empty()
                && matches!(
                    Truthiness::from_expr(&loop_.test, |_| false),
                    Truthiness::False | Truthiness::Falsey | Truthiness::None
                )
        }
        _ => false,
    };
    let changed: Vec<String> = after
        .iter()
        .filter(|(name, binding)| before.get(name.as_str()) != Some(binding))
        .map(|(name, _)| name.clone())
        .collect();
    for name in changed {
        match before.get(&name) {
            Some(binding) if unreachable => {
                after.insert(name, binding.clone());
            }
            _ => {
                after.remove(&name);
            }
        }
    }
}

/// Follow an assignment, annotated or not, into the binding index.
fn collect_assignment_binding(statement: &Stmt, bindings: &mut BTreeMap<String, Binding>) {
    let bound: Option<(&Expr, &[Expr])> = match statement {
        Stmt::Assign(assign) => Some((assign.value.as_ref(), assign.targets.as_slice())),
        Stmt::AnnAssign(assign) => assign
            .value
            .as_deref()
            .map(|value| (value, std::slice::from_ref(&*assign.target))),
        _ => None,
    };
    let Some((value, targets)) = bound else {
        return;
    };
    let binding = assignment_binding(value, bindings);
    for target in targets {
        if let Expr::Name(target) = target {
            if let Some(binding) = &binding {
                bindings.insert(target.id.to_string(), binding.clone());
            } else {
                bindings.remove(target.id.as_str());
            }
        } else {
            // Unpacking binds names this cannot be followed through, so what
            // they held before is no longer what they hold.
            let mut bound = BoundNames::default();
            bound.bind(target);
            for name in bound.names {
                bindings.remove(&name);
            }
        }
    }
}

/// Whether a module defines `name` itself, rather than merely binding it by
/// importing something of that name.
///
/// A package whose `__init__.py` re-exports its own submodule still hands out
/// the submodule, so only a definition or an assignment counts here.
fn source_defines_symbol(path: &Path, name: &str) -> bool {
    let Some(parsed) = read_source(path)
        .ok()
        .and_then(|source| parse_module(&source).ok())
    else {
        return false;
    };
    parsed.suite().iter().any(|statement| match statement {
        Stmt::FunctionDef(function) => function.name.as_str() == name,
        Stmt::ClassDef(class) => class.name.as_str() == name,
        Stmt::Assign(assign) => assign
            .targets
            .iter()
            .any(|target| matches!(target, Expr::Name(target) if target.id.as_str() == name)),
        Stmt::AnnAssign(assign) => {
            assign.value.is_some()
                && matches!(assign.target.as_ref(), Expr::Name(target) if target.id.as_str() == name)
        }
        // `from .impl import name` puts a symbol under the name just as an
        // assignment would. `from . import name` is the submodule itself, and
        // is what the caller is deciding against.
        Stmt::ImportFrom(import) => {
            import.module.is_some()
                && import.names.iter().any(|alias| {
                    alias.asname.as_ref().unwrap_or(&alias.name).as_str() == name
                        && alias.name.as_str() != "*"
                })
        }
        _ => false,
    })
}

fn source_binds_name(path: &Path, name: &str) -> bool {
    read_source(path)
        .ok()
        .and_then(|source| parse_module(&source).ok())
        .is_some_and(|parsed| BoundNames::of_body(parsed.suite()).names.contains(name))
}

/// Keep a binding after an `if` only when every runtime path agrees on it.
fn collect_conditional_bindings(
    branch: &ast::StmtIf,
    importer: &Path,
    known: &BTreeSet<&Path>,
    bindings: &mut BTreeMap<String, Binding>,
) {
    let initial = bindings.clone();
    let mut fallthrough = Some(initial.clone());
    let mut outcomes = Vec::new();
    let clauses = std::iter::once((Some(branch.test.as_ref()), branch.body.as_slice())).chain(
        branch
            .elif_else_clauses
            .iter()
            .map(|clause| (clause.test.as_ref(), clause.body.as_slice())),
    );
    for (test, body) in clauses {
        let Some(base) = fallthrough.take() else {
            break;
        };
        let truth = test.map_or(Truthiness::True, |test| {
            Truthiness::from_expr(test, |_| false)
        });
        match truth {
            Truthiness::True | Truthiness::Truthy => {
                let mut path = base;
                collect_bindings(body, importer, known, &mut path);
                outcomes.push(path);
            }
            Truthiness::False | Truthiness::Falsey | Truthiness::None => {
                fallthrough = Some(base);
            }
            Truthiness::Unknown => {
                let mut path = base.clone();
                collect_bindings(body, importer, known, &mut path);
                outcomes.push(path);
                fallthrough = Some(base);
            }
        }
    }
    if let Some(path) = fallthrough {
        outcomes.push(path);
    }
    retain_common_bindings(bindings, &outcomes, initial);
}

/// Expand star imports only for public fixed callables whose defining checked
/// module is known. Other imported names are irrelevant to call rewriting.
fn collect_star_bindings(
    suite: &[Stmt],
    importer: &Path,
    known: &BTreeSet<&Path>,
    definitions: &Definitions,
    bindings: &mut BTreeMap<String, Binding>,
) -> BTreeSet<String> {
    let mut bound = BTreeSet::new();
    for statement in suite {
        match statement {
            Stmt::ImportFrom(import)
                if import.names.iter().any(|alias| alias.name.as_str() == "*") =>
            {
                let module = import.module.as_ref().map_or("", ast::Identifier::as_str);
                let Some(file) = resolve_module(module, import.level, importer, known) else {
                    continue;
                };
                let explicit = explicit_all_names(&file);
                let mut candidates: BTreeSet<String> = definitions
                    .symbols
                    .get(&file)
                    .into_iter()
                    .flat_map(BTreeMap::keys)
                    .cloned()
                    .collect();
                candidates.extend(
                    definitions
                        .bindings
                        .keys()
                        .filter(|(binding_file, _)| binding_file == &file)
                        .map(|(_, name)| name.clone()),
                );
                for name in candidates.into_iter().filter(|name| {
                    explicit
                        .as_ref()
                        .map_or_else(|| !name.starts_with('_'), |all| all.contains(name))
                }) {
                    let binding = if definitions.symbol(&file, &name).is_some() {
                        Binding::Symbol(file.clone(), name.clone())
                    } else {
                        Binding::Unknown
                    };
                    bound.insert(name.clone());
                    bindings.insert(name, binding);
                }
            }
            Stmt::If(branch) => {
                bound.extend(collect_star_bindings(
                    &branch.body,
                    importer,
                    known,
                    definitions,
                    bindings,
                ));
                for clause in &branch.elif_else_clauses {
                    bound.extend(collect_star_bindings(
                        &clause.body,
                        importer,
                        known,
                        definitions,
                        bindings,
                    ));
                }
            }
            Stmt::Try(block) => {
                bound.extend(collect_star_bindings(
                    &block.body,
                    importer,
                    known,
                    definitions,
                    bindings,
                ));
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    bound.extend(collect_star_bindings(
                        &handler.body,
                        importer,
                        known,
                        definitions,
                        bindings,
                    ));
                }
                bound.extend(collect_star_bindings(
                    &block.orelse,
                    importer,
                    known,
                    definitions,
                    bindings,
                ));
                bound.extend(collect_star_bindings(
                    &block.finalbody,
                    importer,
                    known,
                    definitions,
                    bindings,
                ));
            }
            _ => {}
        }
    }
    bound
}

/// A module's final literal `__all__`, when one is statically declared.
fn explicit_all_names(path: &Path) -> Option<BTreeSet<String>> {
    let source = read_source(path).ok()?;
    let parsed = parse_module(&source).ok()?;
    let mut names = None;
    collect_explicit_all(parsed.suite(), &mut names);
    names
}

fn collect_explicit_all(suite: &[Stmt], names: &mut Option<BTreeSet<String>>) {
    for statement in suite {
        let (value, replace) = match statement {
            Stmt::Assign(assign) if assign.targets.iter().any(is_dunder_all) => {
                (assign.value.as_ref(), true)
            }
            Stmt::AnnAssign(assign) if is_dunder_all(&assign.target) => {
                let Some(value) = assign.value.as_deref() else {
                    // A declaration such as `__all__: list[str]` does not
                    // change the value established by another assignment.
                    continue;
                };
                (value, true)
            }
            Stmt::AugAssign(assign) if is_dunder_all(&assign.target) => {
                (assign.value.as_ref(), false)
            }
            Stmt::Delete(delete) if delete.targets.iter().any(is_dunder_all) => {
                *names = None;
                continue;
            }
            Stmt::If(branch) => {
                collect_conditional_all(branch, names);
                continue;
            }
            Stmt::Try(block) => {
                collect_try_all(block, names);
                continue;
            }
            Stmt::For(loop_) => {
                let initial = names.clone();
                let truth = Truthiness::from_expr(&loop_.iter, |_| false);
                let mut iterated = initial.clone();
                collect_explicit_all(&loop_.body, &mut iterated);
                collect_explicit_all(&loop_.orelse, &mut iterated);
                let mut empty = initial;
                collect_explicit_all(&loop_.orelse, &mut empty);
                *names = match truth {
                    Truthiness::False | Truthiness::Falsey | Truthiness::None => empty,
                    Truthiness::True | Truthiness::Truthy => iterated,
                    Truthiness::Unknown => common_all_state(&[iterated, empty]),
                };
                continue;
            }
            Stmt::While(loop_) => {
                let initial = names.clone();
                let truth = Truthiness::from_expr(&loop_.test, |_| false);
                let mut iterated = initial.clone();
                collect_explicit_all(&loop_.body, &mut iterated);
                let mut completed = iterated.clone();
                collect_explicit_all(&loop_.orelse, &mut completed);
                let mut skipped = initial;
                collect_explicit_all(&loop_.orelse, &mut skipped);
                *names = match truth {
                    Truthiness::False | Truthiness::Falsey | Truthiness::None => skipped,
                    // An always-true loop reaches following statements only
                    // through `break`, which skips its `else` suite.
                    Truthiness::True | Truthiness::Truthy => iterated,
                    Truthiness::Unknown => common_all_state(&[iterated, completed, skipped]),
                };
                continue;
            }
            Stmt::With(block) => {
                collect_explicit_all(&block.body, names);
                continue;
            }
            Stmt::Match(block) => {
                collect_match_all(block, names);
                continue;
            }
            _ => continue,
        };
        let elements = match value {
            Expr::List(list) => &list.elts,
            Expr::Tuple(tuple) => &tuple.elts,
            _ => {
                *names = None;
                continue;
            }
        };
        let literal: Option<BTreeSet<String>> = elements
            .iter()
            .map(|element| match element {
                Expr::StringLiteral(string) => Some(string.value.to_string()),
                _ => None,
            })
            .collect();
        let Some(literal) = literal else {
            *names = None;
            continue;
        };
        if replace {
            *names = Some(literal);
        } else if let Some(names) = names {
            names.extend(literal);
        }
    }
}

fn collect_match_all(block: &ast::StmtMatch, names: &mut Option<BTreeSet<String>>) {
    let initial = names.clone();
    let exhaustive = block
        .cases
        .iter()
        .any(|case| case.guard.is_none() && pattern_is_irrefutable(&case.pattern));
    let mut outcomes = Vec::new();
    if !exhaustive {
        outcomes.push(initial.clone());
    }
    for case in &block.cases {
        let mut outcome = initial.clone();
        collect_explicit_all(&case.body, &mut outcome);
        outcomes.push(outcome);
    }
    *names = common_all_state(&outcomes);
}

fn pattern_is_irrefutable(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::MatchAs(pattern) => pattern
            .pattern
            .as_deref()
            .is_none_or(pattern_is_irrefutable),
        Pattern::MatchOr(pattern) => pattern.patterns.iter().any(pattern_is_irrefutable),
        _ => false,
    }
}

fn collect_try_all(block: &ast::StmtTry, names: &mut Option<BTreeSet<String>>) {
    let initial = names.clone();
    let mut success = initial.clone();
    collect_explicit_all(&block.body, &mut success);
    collect_explicit_all(&block.orelse, &mut success);
    let mut outcomes = vec![success];
    for handler in &block.handlers {
        let ast::ExceptHandler::ExceptHandler(handler) = handler;
        let mut outcome = initial.clone();
        collect_explicit_all(&handler.body, &mut outcome);
        outcomes.push(outcome);
    }
    for outcome in &mut outcomes {
        collect_explicit_all(&block.finalbody, outcome);
    }
    *names = common_all_state(&outcomes);
}

fn collect_conditional_all(branch: &ast::StmtIf, names: &mut Option<BTreeSet<String>>) {
    let initial = names.clone();
    let mut fallthrough = Some(initial.clone());
    let mut outcomes = Vec::new();
    let clauses = std::iter::once((Some(branch.test.as_ref()), branch.body.as_slice())).chain(
        branch
            .elif_else_clauses
            .iter()
            .map(|clause| (clause.test.as_ref(), clause.body.as_slice())),
    );
    for (test, body) in clauses {
        let Some(base) = fallthrough.take() else {
            break;
        };
        match test.map_or(Truthiness::True, |test| {
            Truthiness::from_expr(test, |_| false)
        }) {
            Truthiness::True | Truthiness::Truthy => {
                let mut outcome = base;
                collect_explicit_all(body, &mut outcome);
                outcomes.push(outcome);
            }
            Truthiness::False | Truthiness::Falsey | Truthiness::None => {
                fallthrough = Some(base);
            }
            Truthiness::Unknown => {
                let mut outcome = base.clone();
                collect_explicit_all(body, &mut outcome);
                outcomes.push(outcome);
                fallthrough = Some(base);
            }
        }
    }
    if let Some(outcome) = fallthrough {
        outcomes.push(outcome);
    }
    *names = common_all_state(&outcomes);
}

fn common_all_state(outcomes: &[Option<BTreeSet<String>>]) -> Option<BTreeSet<String>> {
    let first = outcomes.first()?.clone();
    outcomes
        .iter()
        .all(|outcome| outcome == &first)
        .then_some(first)?
}

/// The names a function or class body binds.
///
/// A call to a name bound here does not go to a module-level definition of the
/// same name, so it must not be rewritten as though it did. Where a name cannot
/// be ruled out — a binding in a branch never taken, a comprehension variable
/// that really has a scope of its own — it is collected anyway: over-collecting
/// costs a call left alone with a warning, and under-collecting costs a wrong
/// rewrite.
///
/// A nested `def` or `class` is a scope of its own, and the rewriter pushes one
/// for it on the way in, so what it binds is left to it. Only its own name is
/// taken, because that is what the body being collected binds.
/// The class-body assignments that put the same function back under its own
/// name, such as `target = staticmethod(target)`.
///
/// These are not overwrites: a call still reaches the function defined above,
/// so its default is still the one to give back. Anything else assigned to the
/// name is a replacement, including a later one that follows a rewrap.
fn class_rewraps(
    body: &[Stmt],
    aliases: &Aliases,
    module_bindings: &BTreeSet<String>,
) -> BTreeMap<String, Vec<TextSize>> {
    struct Collector<'a> {
        found: BTreeMap<String, Vec<TextSize>>,
        aliases: &'a Aliases,
        module_bindings: &'a BTreeSet<String>,
    }

    impl<'a> Visitor<'a> for Collector<'_> {
        fn visit_stmt(&mut self, statement: &'a Stmt) {
            match statement {
                Stmt::Assign(assign) => {
                    let [Expr::Name(target)] = assign.targets.as_slice() else {
                        return;
                    };
                    let Expr::Call(call) = assign.value.as_ref() else {
                        return;
                    };
                    // Only the descriptor wrappers are known to leave the
                    // function's own parameters behind the name. Any other
                    // call may return something taking anything at all.
                    let wraps_itself = call.arguments.args.len() == 1
                        && call.arguments.keywords.is_empty()
                        && descriptor_wrapper_kind(&call.func, self.aliases, self.module_bindings)
                            .is_some()
                        && matches!(&call.arguments.args[0], Expr::Name(wrapped) if wrapped.id == target.id);
                    if wraps_itself {
                        self.found
                            .entry(target.id.to_string())
                            .or_default()
                            .push(statement.start());
                    }
                }
                Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
                _ => walk_stmt(self, statement),
            }
        }
    }

    let mut collector = Collector {
        found: BTreeMap::new(),
        aliases,
        module_bindings,
    };
    collector.visit_body(body);
    collector.found
}

fn class_assignments(body: &[Stmt]) -> BTreeMap<String, Vec<TextSize>> {
    #[derive(Default)]
    struct Collector(BTreeMap<String, Vec<TextSize>>);

    impl<'a> Visitor<'a> for Collector {
        fn visit_stmt(&mut self, statement: &'a Stmt) {
            match statement {
                Stmt::Assign(assign) => {
                    let mut bound = BoundNames::default();
                    for target in &assign.targets {
                        bound.bind(target);
                    }
                    for name in bound.names {
                        self.0.entry(name).or_default().push(statement.start());
                    }
                }
                Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
                _ => walk_stmt(self, statement),
            }
        }
    }

    let mut collector = Collector::default();
    collector.visit_body(body);
    collector.0
}

fn deleted_names(body: &[Stmt]) -> BTreeSet<String> {
    #[derive(Default)]
    struct Collector(BTreeSet<String>);

    impl<'a> Visitor<'a> for Collector {
        fn visit_stmt(&mut self, statement: &'a Stmt) {
            match statement {
                Stmt::Delete(delete) => {
                    let mut bound = BoundNames::default();
                    for target in &delete.targets {
                        bound.bind(target);
                    }
                    self.0.extend(bound.names);
                }
                Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
                _ => walk_stmt(self, statement),
            }
        }
    }

    let mut collector = Collector::default();
    collector.visit_body(body);
    collector.0
}

/// Whether an `if` test is the `TYPE_CHECKING` guard.
///
/// The name is read from the source rather than resolved back to `typing`,
/// because this is asked of a class body long after the imports that reached
/// it have been forgotten. A file that binds `TYPE_CHECKING` to something of
/// its own is taken at its word.
fn tests_type_checking(test: &Expr) -> bool {
    match test {
        Expr::Name(name) => name.id.as_str() == "TYPE_CHECKING",
        Expr::Attribute(attribute) => attribute.attr.as_str() == "TYPE_CHECKING",
        _ => false,
    }
}

#[derive(Default)]
struct BoundNames {
    names: BTreeSet<String>,
    /// Targets of bare annotations. `name: int` puts nothing behind the name,
    /// so it rebinds nothing, but it does make the name the scope's own.
    /// `finish` folds these into `names` for whoever asks what a scope holds;
    /// a caller that walks a body without finishing is asking what the body
    /// assigns, and gets only that.
    annotations: BTreeSet<String>,
    globals: BTreeSet<String>,
    nonlocals: BTreeSet<String>,
    functions: BTreeSet<String>,
    classes: BTreeSet<String>,
    /// Whether to collect only the names still bound once the body has run.
    /// An `except ... as` target is deleted when its handler ends, and a
    /// `TYPE_CHECKING` block is read by type checkers and never run at all,
    /// so neither leaves anything behind. A caller asking what a scope holds
    /// wants them anyway, because they are the scope's own while the body is
    /// running; a caller asking what a class ends up carrying does not.
    surviving_only: bool,
}

impl BoundNames {
    /// Collect the names an assignment target binds. An attribute or subscript
    /// target rebinds nothing by its own name.
    fn bind(&mut self, target: &Expr) {
        match target {
            Expr::Name(name) => {
                self.names.insert(name.id.to_string());
            }
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|element| self.bind(element)),
            Expr::List(list) => list.elts.iter().for_each(|element| self.bind(element)),
            Expr::Starred(starred) => self.bind(&starred.value),
            _ => {}
        }
    }

    /// Collect the name a bare annotation declares without assigning to.
    fn declare(&mut self, target: &Expr) {
        let mut declared = Self::default();
        declared.bind(target);
        self.annotations.append(&mut declared.names);
    }

    fn parameters(&mut self, parameters: &ast::Parameters) {
        for parameter in parameters
            .posonlyargs
            .iter()
            .chain(&parameters.args)
            .chain(&parameters.kwonlyargs)
        {
            self.names.insert(parameter.parameter.name.to_string());
        }
        for parameter in [&parameters.vararg, &parameters.kwarg]
            .into_iter()
            .flatten()
        {
            self.names.insert(parameter.name.to_string());
        }
    }

    fn of_comprehension(generators: &[ast::Comprehension]) -> Self {
        let mut collector = Self::default();
        for generator in generators {
            collector.bind(&generator.target);
        }
        collector
    }

    /// The names bound anywhere inside a function, including its parameters.
    fn finish(mut self) -> Self {
        self.names.append(&mut self.annotations);
        for name in &self.globals {
            self.names.remove(name);
            self.functions.remove(name);
            self.classes.remove(name);
        }
        for name in &self.nonlocals {
            self.names.remove(name);
            self.functions.remove(name);
            self.classes.remove(name);
        }
        self
    }

    fn of_function(function: &ast::StmtFunctionDef) -> Self {
        let mut collector = Self::default();
        collector.parameters(&function.parameters);
        for statement in &function.body {
            collector.visit_stmt(statement);
        }
        collector.finish()
    }

    /// The names a signature claims, without reading the body behind it.
    fn of_parameters(parameters: &ast::Parameters) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.parameters(parameters);
        collector.names
    }

    fn of_body(body: &[Stmt]) -> Self {
        let mut collector = Self::default();
        for statement in body {
            collector.visit_stmt(statement);
        }
        collector.finish()
    }

    /// The names a module actually puts something behind.
    ///
    /// A bare annotation is what separates this from `of_body`. Inside a
    /// function or a class body `name: int` still makes the name that
    /// scope's own, so `finish` folds it in; at module level there is no
    /// enclosing scope to be claimed from, only the builtins, and those an
    /// annotation leaves entirely alone. `super: object` next to a
    /// `super()` call reaches the builtin exactly as it would have without
    /// the annotation.
    fn of_module(body: &[Stmt]) -> BTreeSet<String> {
        let mut collector = Self::default();
        for statement in body {
            collector.visit_stmt(statement);
        }
        collector.annotations.clear();
        collector.finish().names
    }

    /// The names a class body leaves on the class once it has run.
    ///
    /// A class body is executed and whatever is still bound at the end of it
    /// becomes an attribute, so a name that does not survive the body cannot
    /// shadow what the bases hold. `finish` is deliberately not called: as at
    /// module level, a bare annotation puts nothing behind the name. A
    /// `global` or `nonlocal` declaration is honoured for the opposite
    /// reason: an assignment under one reaches past the class body entirely.
    fn of_class_attributes(body: &[Stmt]) -> BTreeSet<String> {
        let mut collector = Self {
            surviving_only: true,
            ..Self::default()
        };
        for statement in body {
            collector.visit_stmt(statement);
            // A `del` written straight in the body takes the name back off
            // the class, and one written after a rebinding of it does not,
            // so the statements are read in order. A `del` nested in a branch
            // or a loop may never run, and is left to stand as written.
            if let Stmt::Delete(delete) = statement {
                let mut deleted = Self::default();
                for target in &delete.targets {
                    deleted.bind(target);
                }
                collector.names.retain(|name| !deleted.names.contains(name));
            }
        }
        let declared: BTreeSet<String> = collector
            .globals
            .union(&collector.nonlocals)
            .cloned()
            .collect();
        collector.names.retain(|name| !declared.contains(name));
        collector.names
    }

    fn of_lambda(lambda: &ast::ExprLambda) -> Self {
        let mut collector = Self::default();
        if let Some(parameters) = &lambda.parameters {
            collector.parameters(parameters);
        }
        collector
    }
}

impl<'a> Visitor<'a> for BoundNames {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Assign(assign) => assign.targets.iter().for_each(|target| self.bind(target)),
            Stmt::AnnAssign(assign) => {
                if assign.value.is_some() {
                    self.bind(&assign.target);
                } else {
                    self.declare(&assign.target);
                }
            }
            Stmt::AugAssign(assign) => self.bind(&assign.target),
            Stmt::For(loop_statement) => self.bind(&loop_statement.target),
            Stmt::With(block) => {
                for item in &block.items {
                    if let Some(target) = &item.optional_vars {
                        self.bind(target);
                    }
                }
            }
            // A nested scope binds its own name here and everything else
            // inside itself, so it is not descended into.
            Stmt::FunctionDef(function) => {
                self.names.insert(function.name.to_string());
                self.functions.insert(function.name.to_string());
                return;
            }
            Stmt::ClassDef(class) => {
                self.names.insert(class.name.to_string());
                self.classes.insert(class.name.to_string());
                return;
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().map_or_else(
                        || {
                            alias
                                .name
                                .split('.')
                                .next()
                                .unwrap_or(alias.name.as_str())
                                .to_owned()
                        },
                        ToString::to_string,
                    );
                    self.names.insert(bound);
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map_or_else(|| alias.name.to_string(), ToString::to_string);
                    self.names.insert(bound);
                }
            }
            Stmt::Try(block) if !self.surviving_only => {
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        self.names.insert(name.to_string());
                    }
                }
            }
            // A `TYPE_CHECKING` block is read by type checkers and never run,
            // so the names in it are bound at no point. The guard is
            // recognised by the name it is written under, which is how it is
            // always spelled; anything else is walked like an ordinary branch.
            Stmt::If(branch) if self.surviving_only => {
                self.visit_expr(&branch.test);
                if !tests_type_checking(&branch.test) {
                    self.visit_body(&branch.body);
                }
                for clause in &branch.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.visit_expr(test);
                    }
                    if clause.test.as_ref().is_some_and(tests_type_checking) {
                        continue;
                    }
                    self.visit_body(&clause.body);
                }
                return;
            }
            Stmt::Global(global) => {
                self.globals
                    .extend(global.names.iter().map(ToString::to_string));
            }
            Stmt::Nonlocal(nonlocal) => {
                self.nonlocals
                    .extend(nonlocal.names.iter().map(ToString::to_string));
            }
            _ => {}
        }
        walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Named(named) => self.bind(&named.target),
            // As with a nested `def`, a lambda's parameters belong to the
            // lambda, and the rewriter pushes a scope for it. Only the body
            // runs in that scope: the parameter defaults are evaluated where
            // the lambda is written, so a walrus among them binds out here and
            // is collected before the body is left alone.
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    for default in parameters
                        .posonlyargs
                        .iter()
                        .chain(&parameters.args)
                        .chain(&parameters.kwonlyargs)
                        .filter_map(|parameter| parameter.default.as_deref())
                    {
                        self.visit_expr(default);
                    }
                }
                return;
            }
            _ => {}
        }
        walk_expr(self, expression);
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        match pattern {
            Pattern::MatchMapping(mapping) => {
                if let Some(name) = &mapping.rest {
                    self.names.insert(name.to_string());
                }
            }
            Pattern::MatchStar(star) => {
                if let Some(name) = &star.name {
                    self.names.insert(name.to_string());
                }
            }
            Pattern::MatchAs(as_pattern) => {
                if let Some(name) = &as_pattern.name {
                    self.names.insert(name.to_string());
                }
            }
            _ => {}
        }
        walk_pattern(self, pattern);
    }
}

/// The dotted name an expression spells, for `a`, `a.b`, and `a.b.c`.
fn dotted_name(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attribute) => Some(format!(
            "{}.{}",
            dotted_name(&attribute.value)?,
            attribute.attr
        )),
        _ => None,
    }
}

struct Rewriter<'a> {
    /// The spelling the file was collected under, which is what it reports as.
    path: &'a Path,
    /// The same file's physical path, which is what resolution and the
    /// definition index are keyed by.
    physical: &'a Path,
    source: &'a str,
    definitions: &'a Definitions,
    aliases: Aliases,
    /// Annotation expressions are stored as strings and never evaluated when
    /// the future annotations feature is active. A stub postpones them the
    /// same way without the import, because nothing in it runs.
    postponed_annotations: bool,
    module_bindings: BTreeSet<String>,
    /// What each imported name in this file refers to.
    bindings: Vec<BTreeMap<String, Binding>>,
    /// Lexical scope index owning each non-module binding table.
    binding_scope_depths: Vec<usize>,
    /// Lexical scope indices belonging to lambdas, whose walruses bind there.
    lambda_scope_depths: Vec<usize>,
    /// Imports bound directly in each enclosing class namespace.
    class_bindings: Vec<BTreeMap<String, Binding>>,
    /// Imports in each enclosing class namespace that a `def` or a `class`
    /// nested in control flow may have taken the name of. A call below reaches
    /// the definition or the import, so the import's default has to stay.
    conditional_class_definitions: Vec<BTreeSet<String>>,
    /// Imported module-scope names replaced by an assignment already visited.
    invalidated_bindings: BTreeSet<String>,
    /// Module names that can no longer be assumed to denote an earlier class.
    rebound_classes: BTreeSet<String>,
    /// Module-level names that something other than an import binds, so an
    /// import of the name no longer says what a call to it reaches.
    module_rebindings: BTreeSet<String>,
    known: &'a BTreeSet<&'a Path>,
    /// The class bodies being walked, so `self.method(...)` can be resolved.
    classes: Vec<String>,
    /// Scope-stack depth immediately inside each class body, distinguishing a
    /// direct method from a function nested inside one.
    class_scope_depths: Vec<usize>,
    /// Statements directly in each class suite. A nested control-flow delete
    /// is conditional and cannot definitely remove an earlier class binding.
    class_direct_statements: Vec<BTreeSet<TextSize>>,
    /// Whether each `lexical_scope` entry is a class body rather than a
    /// function, so a lookup can pass over the ones that are not closures.
    lexical_is_class: Vec<bool>,
    /// The implicit receiver of each enclosing function. Static methods and
    /// module functions contribute `None`.
    implicit_receivers: Vec<Option<ImplicitReceiver>>,
    /// The class cell owned by each enclosing function, independently of
    /// whether that function has an implicit descriptor receiver.
    implicit_receiver_classes: Vec<Option<String>>,
    /// Enclosing class and function names, matching the checker's lexical
    /// identity for nested functions.
    lexical_scope: Vec<String>,
    /// Ranges of the expressions being called, so that the same expression is
    /// not later mistaken for a reference that never calls the function.
    called: BTreeSet<(TextSize, TextSize)>,
    /// The names bound by each enclosing function, class, and lambda scope. A
    /// name bound in one of them shadows a module-level definition, so a call
    /// to it does not go where the definition went.
    scopes: Vec<BoundNames>,
    /// Where each line of `source` starts, for the same reason the checker
    /// keeps one.
    lines: LineIndex,
    edits: Vec<Edit>,
    skipped: Vec<Skipped>,
    retained: BTreeSet<FixKey>,
}

impl Rewriter<'_> {
    fn in_class_scope(&self) -> bool {
        self.class_scope_depths.last() == Some(&self.scopes.len())
    }

    /// Bind in the class body what a statement in it binds, once that
    /// statement has run: a class body binds in statement order.
    fn bind_statement_in_class(&mut self, statement: &Stmt) {
        if let Some(scope) = self.scopes.last_mut() {
            let bound = BoundNames::of_body(std::slice::from_ref(statement));
            scope.names.extend(bound.names);
            scope.functions.extend(bound.functions);
            scope.classes.extend(bound.classes);
        }
    }

    fn invalidate_class_bindings(&mut self, names: impl IntoIterator<Item = String>) {
        if let Some(bindings) = self.class_bindings.last_mut() {
            for name in names {
                bindings.remove(&name);
            }
        }
    }

    fn bind_definition_in_class(&mut self, name: &str, is_class: bool, start: TextSize) {
        if !self.in_class_scope() {
            return;
        }
        // A `def` or `class` takes the name over in the class namespace, so an
        // import that bound it earlier is no longer what a call below reaches.
        // A definition nested in class-body control flow may never run, but
        // then the name is whichever of the two the branch left behind, and
        // that is still not knowable from the source alone.
        let replaced_import = self
            .class_bindings
            .last()
            .is_some_and(|bindings| bindings.contains_key(name));
        let direct = self
            .class_direct_statements
            .last()
            .is_some_and(|statements| statements.contains(&start));
        // Declining to rewrite the call is only half of it: the import is
        // still one of the two things the name can hold, so removing its
        // default would leave that call short an argument.
        if replaced_import && !direct {
            if let Some(names) = self.conditional_class_definitions.last_mut() {
                names.insert(name.to_owned());
            }
        }
        self.invalidate_class_bindings([name.to_owned()]);
        if let Some(scope) = self.scopes.last_mut() {
            scope.names.insert(name.to_owned());
            if is_class {
                scope.classes.insert(name.to_owned());
            } else {
                scope.functions.insert(name.to_owned());
            }
        }
    }

    /// Whether the nearest lexical binding for `name` is a nested callable.
    /// `None` means no enclosing scope binds it at all.
    /// Record that a module-level name has been rebound: later calls reach
    /// whatever replaced the import, and no longer the same-file class that
    /// was defined under that name.
    fn rebind_module_name(&mut self, names: impl IntoIterator<Item = String>) {
        for name in names {
            self.invalidated_bindings.insert(name.clone());
            self.rebound_classes.insert(name);
        }
    }

    fn nested_callable(&self, name: &str) -> Option<bool> {
        self.nested_binding(name).map(|(callable, _)| callable)
    }

    fn nested_function(&self, name: &str) -> bool {
        self.nested_binding(name)
            .is_some_and(|(_, index)| self.scopes[index].functions.contains(name))
    }

    fn nested_binding(&self, name: &str) -> Option<(bool, usize)> {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, scope)| {
                let class_frame = self.class_scope_depths.contains(&(index + 1));
                if class_frame && !(self.in_class_scope() && index + 1 == self.scopes.len()) {
                    return None;
                }
                scope.names.contains(name).then(|| {
                    (
                        scope.functions.contains(name) || scope.classes.contains(name),
                        index,
                    )
                })
            })
    }

    fn binding(&self, name: &str) -> Option<&Binding> {
        if self.in_class_scope() {
            if let Some(binding) = self
                .class_bindings
                .last()
                .and_then(|bindings| bindings.get(name))
            {
                return Some(binding);
            }
        }
        for (index, bindings) in self.bindings.iter().enumerate().rev() {
            if let Some(binding) = bindings.get(name) {
                return (!(index == 0 && self.binding_is_replaced(name))).then_some(binding);
            }
        }
        None
    }

    fn function_binding(&self, name: &str) -> Option<&Binding> {
        // `global name` sends the name to the module namespace, so an import
        // an enclosing function made under it is not what the call reaches.
        // `nonlocal` is the opposite: that enclosing binding is exactly it.
        if self
            .scopes
            .last()
            .is_some_and(|scope| scope.globals.contains(name))
        {
            let binding = self.bindings.first()?.get(name)?;
            return (!self.binding_is_replaced(name)).then_some(binding);
        }
        let owner = self.nested_binding(name).map(|(_, index)| index);
        self.bindings
            .iter()
            .zip(&self.binding_scope_depths)
            .skip(1)
            .rev()
            .find_map(|(bindings, depth)| {
                let binding = bindings.get(name)?;
                owner.is_none_or(|owner| owner == *depth).then_some(binding)
            })
    }

    fn function_symbol(&self, name: &str) -> Option<&Signature> {
        let Binding::Symbol(file, symbol) = self.function_binding(name)? else {
            return None;
        };
        self.definitions.symbol(file, symbol)
    }

    /// Whether a module-scope import has been replaced by a later binding.
    ///
    /// `import pkg.api` is keyed by the whole dotted path, but Python binds
    /// only `pkg`. Rebinding that one name replaces what every `pkg.…`
    /// expression reaches, so the dotted entry goes with it.
    fn binding_is_replaced(&self, name: &str) -> bool {
        let head = name.split('.').next().unwrap_or(name);
        self.invalidated_bindings.contains(name) || self.invalidated_bindings.contains(head)
    }

    fn has_unknown_receiver_binding(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Name(name) => matches!(self.binding(name.id.as_str()), Some(Binding::Unknown)),
            Expr::Attribute(attribute) => self.has_unknown_receiver_binding(&attribute.value),
            _ => false,
        }
    }

    /// Whether the call is made on a class whose ancestry two suites of one
    /// statement disagree on. The method it reaches is one of two, and the
    /// default behind whichever it is has to survive a call left unrewritten.
    fn receiver_ancestry_is_uncertain(&self, expression: &Expr) -> bool {
        let Expr::Attribute(attribute) = expression else {
            return false;
        };
        self.class_ancestry_is_uncertain(&attribute.value)
    }

    /// Whether the call builds such a class. `Child()` and `api.Child()` name
    /// the class rather than the `__init__` it inherits, so there is no single
    /// constructor to give them and the class is left without one. That leaves
    /// the call bare of the argument the removed default stood in for, which is
    /// why the deletion has to be held back.
    ///
    /// A construction through a module is spelled exactly as a method call is,
    /// so what tells them apart is whether the callee itself names a class:
    /// `api.Child` does, and `receiver.method` does not.
    fn constructs_uncertain_ancestry(&self, expression: &Expr) -> bool {
        matches!(expression, Expr::Name(_) | Expr::Attribute(_))
            && self.class_ancestry_is_uncertain(expression)
    }

    fn class_ancestry_is_uncertain(&self, receiver: &Expr) -> bool {
        self.receiving_class(receiver)
            .is_some_and(|(file, class, _, _)| {
                self.definitions.ancestry_is_uncertain(&(file, class))
            })
    }

    fn skip(&mut self, offset: TextSize, callable: &str, reason: String) {
        let (line, column) = self.lines.locate(self.source, offset);
        self.skipped.push(Skipped {
            path: self.path.to_path_buf(),
            line,
            column,
            callable: callable.to_owned(),
            reason,
        });
    }

    /// Warn about a fixed function named somewhere other than a call, such as
    /// a bare `@decorator` or a callback passed by name. Python still calls it,
    /// but there is no argument list to add the removed default to.
    fn check_reference(&mut self, expression: &Expr) {
        if self
            .called
            .contains(&(expression.start(), expression.end()))
        {
            return;
        }
        let Some(name) = (match expression {
            Expr::Name(name) => Some(name.id.as_str()),
            Expr::Attribute(attribute) => Some(attribute.attr.as_str()),
            _ => None,
        }) else {
            return;
        };
        if !self
            .resolve(expression)
            .is_some_and(|(signature, _)| signature.kind.is_function())
        {
            return;
        }
        self.skip(
            expression.start(),
            name,
            "it is named here without being called, so the removed default cannot be supplied"
                .to_owned(),
        );
    }

    /// The class a receiver expression stands for, the file defining it, and
    /// whether the receiver is an instance rather than the class itself.
    ///
    /// `self` and `cls` name the enclosing class; `Client` names a class of
    /// this file; and `api.Client` names one of an imported module.
    /// The class a receiver names, whether it was reached through an instance,
    /// and whether it was reached through a zero-argument `super()`, whose
    /// lookup starts after the class the call appears in.
    fn receiving_class(&self, receiver: &Expr) -> Option<(PathBuf, String, bool, bool)> {
        if let Expr::Call(call) = receiver {
            if let Some(class) = self.type_of_implicit_instance(call) {
                return Some(class);
            }
            let implicit_receiver = self.implicit_receiver();
            let zero_argument_super = call.arguments.args.is_empty() && implicit_receiver.is_some();
            let explicit_super = match &*call.arguments.args {
                [Expr::Name(class), Expr::Name(instance)] => {
                    implicit_receiver.is_some_and(|receiver| {
                        receiver.class.rsplit('.').next() == Some(class.id.as_str())
                            && receiver.name == instance.id.as_str()
                    })
                }
                _ => false,
            };
            if call.arguments.keywords.is_empty()
                && self.is_builtin_super_call(call)
                && (zero_argument_super || explicit_super)
            {
                return Some((
                    self.physical.to_path_buf(),
                    implicit_receiver?.class.clone(),
                    true,
                    true,
                ));
            }
            // `C().fetch()` calls a method on a fresh instance, so the
            // receiver is the class the call constructs. Constructing an
            // instance is not reaching through `super`.
            let (file, class, through_instance, _) = self.receiving_class(&call.func)?;
            if !through_instance
                && self
                    .definitions
                    .methods
                    .contains_key(&(file.clone(), class.clone()))
            {
                return Some((file, class, true, false));
            }
            return None;
        }
        if let Expr::Name(name) = receiver {
            if let Some(implicit) = self
                .implicit_receiver()
                .filter(|implicit| implicit.name == name.id.as_str())
            {
                return Some((
                    self.physical.to_path_buf(),
                    implicit.class.clone(),
                    !implicit.is_class,
                    false,
                ));
            }
            if self
                .scopes
                .iter()
                .any(|scope| scope.names.contains(name.id.as_str()))
            {
                return None;
            }
            // Python supplies a `__class__` closure cell to class-owned
            // functions that reference it. It denotes the enclosing class,
            // including in static methods and nested closures.
            if name.id.as_str() == "__class__" && !self.implicit_receivers.is_empty() {
                return self.enclosing_class_cell();
            }
            if self.rebound_classes.contains(name.id.as_str()) {
                return None;
            }
            return match self.binding(name.id.as_str()) {
                // `from api import Client` names a class of another file.
                Some(Binding::Symbol(file, symbol)) => {
                    let (file, class) = self.definitions.class_identity(file, symbol)?;
                    Some((file, class, false, false))
                }
                Some(Binding::Module(_) | Binding::Unknown) => None,
                None => Some((
                    self.physical.to_path_buf(),
                    name.id.to_string(),
                    false,
                    false,
                )),
            };
        }
        if let Some(class) = self.self_class_receiver(receiver) {
            return Some(class);
        }
        let Expr::Attribute(attribute) = receiver else {
            return None;
        };
        // `import a.b` records both `a` and the exact `a.b` module. Prefer the
        // latter: otherwise the attribute form can be mistaken for class `b`
        // stored on package `a`.
        if dotted_name(receiver)
            .is_some_and(|name| matches!(self.binding(&name), Some(Binding::Module(_))))
        {
            return None;
        }
        let dotted = dotted_name(&attribute.value)?;
        match self.binding(&dotted)? {
            Binding::Module(file) => Some((file.clone(), attribute.attr.to_string(), false, false)),
            Binding::Symbol(..) | Binding::Unknown => None,
        }
    }

    fn enclosing_class_cell(&self) -> Option<(PathBuf, String, bool, bool)> {
        Some((
            self.physical.to_path_buf(),
            self.implicit_receiver_classes.last()?.clone()?,
            false,
            false,
        ))
    }

    fn is_builtin_super_call(&self, call: &ast::ExprCall) -> bool {
        match call.func.as_ref() {
            // An import of the builtin is checked before the bare name, since
            // `from builtins import super` binds `super` to the very builtin
            // the name would have reached anyway. Reading that binding as a
            // shadow would leave the call pointing at nothing.
            Expr::Name(name) if self.aliases.supers.contains(name.id.as_str()) => {
                self.nested_binding(name.id.as_str()).is_none()
                    && !self.binding_is_replaced(name.id.as_str())
                    && !self.module_rebindings.contains(name.id.as_str())
            }
            Expr::Name(name) if name.id.as_str() == "super" => {
                self.nested_binding("super").is_none()
                    && self.binding("super").is_none()
                    && !self.module_bindings.contains("super")
            }
            _ => false,
        }
    }

    fn self_class_receiver(&self, receiver: &Expr) -> Option<(PathBuf, String, bool, bool)> {
        let Expr::Attribute(attribute) = receiver else {
            return None;
        };
        let Expr::Name(instance) = attribute.value.as_ref() else {
            return None;
        };
        let implicit = self.implicit_receiver()?;
        if attribute.attr.as_str() == "__class__"
            && implicit.name == instance.id.as_str()
            && !implicit.is_class
        {
            Some((
                self.physical.to_path_buf(),
                implicit.class.clone(),
                false,
                false,
            ))
        } else {
            None
        }
    }

    fn type_of_implicit_instance(
        &self,
        call: &ast::ExprCall,
    ) -> Option<(PathBuf, String, bool, bool)> {
        let [Expr::Name(instance)] = &*call.arguments.args else {
            return None;
        };
        let implicit = self.implicit_receiver()?;
        if !call.arguments.keywords.is_empty()
            || !matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "type")
            || implicit.name != instance.id.as_str()
            || implicit.is_class
            || self.nested_binding("type").is_some()
            || self.binding("type").is_some()
            || self.module_bindings.contains("type")
        {
            return None;
        }
        Some((
            self.physical.to_path_buf(),
            implicit.class.clone(),
            false,
            false,
        ))
    }

    /// The receiver the body being walked sees without naming it.
    fn implicit_receiver(&self) -> Option<&ImplicitReceiver> {
        self.implicit_receivers.last()?.as_ref()
    }

    /// The callable an expression names, when the file's own imports say so,
    /// and how many parameters the call has already been given implicitly.
    /// The signature a bare nested name refers to at this call site.
    ///
    /// A nested definition is visible throughout the scope holding it, so the
    /// search runs outwards from the call site. It stops at the nearest scope
    /// that binds the name, whether or not anything was recorded for it, and
    /// passes over class bodies, which are not closure scopes.
    fn nested_signature(&self, name: &str) -> Option<&Signature> {
        let symbols = self.definitions.symbols.get(self.physical)?;
        // A lambda or comprehension written in a class body has a scope of its
        // own that the class namespace is not part of, so a name bound beside
        // it is out of reach from in there.
        if self
            .scopes
            .iter()
            .skip(self.lexical_scope.len())
            .any(|scope| scope.names.contains(name))
        {
            return None;
        }
        // A nested function is never reached by its bare name, so that last
        // step is only for the other callables.
        let outermost = usize::from(self.nested_function(name));
        let innermost_class_applies = self.in_class_scope();
        for depth in (outermost..=self.lexical_scope.len()).rev() {
            if depth > 0
                && self.lexical_is_class.get(depth - 1) == Some(&true)
                && !(depth == self.lexical_scope.len() && innermost_class_applies)
            {
                continue;
            }
            if let Some(signature) =
                symbols.get(&qualified_lexical_name(&self.lexical_scope[..depth], name))
            {
                return signature.as_ref();
            }
            // Nothing was recorded here, but a scope that binds the name still
            // hides whatever the scopes outside it call by the same name.
            if depth > 0
                && self
                    .scopes
                    .get(depth - 1)
                    .is_some_and(|scope| scope.names.contains(name))
            {
                return None;
            }
        }
        None
    }

    fn resolve(&self, expression: &Expr) -> Option<(&Signature, usize)> {
        match expression {
            Expr::Name(name)
                if self.in_class_scope()
                    && self
                        .class_bindings
                        .last()
                        .is_some_and(|bindings| bindings.contains_key(name.id.as_str())) =>
            {
                let Binding::Symbol(file, symbol) = self.binding(name.id.as_str())? else {
                    return None;
                };
                let signature = self.definitions.symbol(file, symbol)?;
                Some((signature, signature.kind.implicit_bound()))
            }
            Expr::Name(name) if self.function_binding(name.id.as_str()).is_some() => {
                let signature = self.function_symbol(name.id.as_str())?;
                Some((signature, signature.kind.implicit_bound()))
            }
            // A bare name is either defined in this file or imported into it —
            // unless an enclosing scope binds it, in which case it is neither.
            Expr::Name(name) if self.nested_callable(name.id.as_str()) == Some(true) => {
                let signature = self.nested_signature(name.id.as_str())?;
                Some((signature, signature.kind.implicit_bound()))
            }
            Expr::Name(name) if self.invalidated_bindings.contains(name.id.as_str()) => None,
            Expr::Name(name)
                if self.nested_callable(name.id.as_str()) == Some(false)
                    && self.function_binding(name.id.as_str()).is_none() =>
            {
                None
            }
            Expr::Name(name) => match self.binding(name.id.as_str()) {
                Some(Binding::Symbol(file, symbol)) => {
                    let signature = self.definitions.symbol(file, symbol)?;
                    Some((signature, signature.kind.implicit_bound()))
                }
                Some(Binding::Module(_) | Binding::Unknown) => None,
                None => {
                    let signature = self
                        .definitions
                        .symbols
                        .get(self.physical)?
                        .get(name.id.as_str())?
                        .as_ref()?;
                    Some((signature, signature.kind.implicit_bound()))
                }
            },
            Expr::Attribute(attribute) => {
                // A method's receiver type is only known when it is `self`,
                // `cls`, or a class this file can name.
                if let Some((file, class, through_instance, through_super)) =
                    self.receiving_class(&attribute.value)
                {
                    let found = if through_super {
                        self.definitions
                            .super_method(&file, &class, attribute.attr.as_str())
                    } else {
                        self.definitions
                            .method(&file, &class, attribute.attr.as_str())
                    };
                    if let Some(signature) = found {
                        let Callable::Method { receiver, .. } = &signature.kind else {
                            return None;
                        };
                        // Reached through the class itself, an ordinary method
                        // is unbound: `Client.fetch(instance, url)` writes out
                        // the instance that `instance.fetch(url)` implies.
                        let given = match (receiver, through_instance) {
                            (Receiver::None, _) | (Receiver::Instance, false) => 0,
                            (Receiver::Class, _) | (Receiver::Instance, true) => 1,
                        };
                        return Some((signature, given));
                    }
                    if through_instance {
                        return None;
                    }
                    // A class attribute may itself be a nested class. Its
                    // constructor is indexed under the qualified definition
                    // name, so `Outer.Inner()` must look for `Outer.Inner`
                    // after determining which `Outer` the receiver names.
                    let nested = format!("{class}.{}", attribute.attr);
                    if let Some(signature) = self.definitions.symbol(&file, &nested) {
                        return Some((signature, signature.kind.implicit_bound()));
                    }
                }
                let dotted = dotted_name(&attribute.value)?;
                let Some(Binding::Module(file)) = self.binding(&dotted) else {
                    return None;
                };
                let signature = self.definitions.symbol(file, attribute.attr.as_str())?;
                Some((signature, signature.kind.implicit_bound()))
            }
            _ => None,
        }
    }

    /// Report a call that names a fixed callable but reaches something else:
    /// an unrelated `connect`, a method on a receiver whose type is not known,
    /// or a call through an unresolved import. Rewriting it would break
    /// working code, so it is reported instead — and where the name it stands
    /// for is one of two, the default behind it is held back as well, since
    /// nothing was written into the call to stand in for it.
    fn report_unresolved_call(&mut self, call: &ast::ExprCall, name: &str) {
        // The gate is keyed on the name the fixed callable goes by, which a
        // construction does not carry: it is spelled with the class's name,
        // not the inherited `__init__`'s.
        let constructs_uncertain = self.constructs_uncertain_ancestry(&call.func);
        if !self.definitions.names.contains(name) && !constructs_uncertain {
            return;
        }
        let replaced_import =
            dotted_name(&call.func).is_some_and(|binding| self.binding_is_replaced(&binding));
        let local_shadow = matches!(call.func.as_ref(), Expr::Name(name) if self.nested_callable(name.id.as_str()) == Some(false));
        let ambiguous_import = self.has_unknown_receiver_binding(&call.func);
        let uncertain_ancestry =
            constructs_uncertain || self.receiver_ancestry_is_uncertain(&call.func);
        let conditional_definition = matches!(call.func.as_ref(), Expr::Name(name) if self.in_class_scope()
            && self
                .conditional_class_definitions
                .last()
                .is_some_and(|names| names.contains(name.id.as_str())));
        if replaced_import
            || local_shadow
            || ambiguous_import
            || conditional_definition
            || uncertain_ancestry
        {
            if let Some(fixes) = self.definitions.fixes_by_name.get(name) {
                self.retained.extend(fixes.iter().cloned());
            }
            // Asking for the call's own name finds nothing for a construction,
            // so the inherited constructor is asked for under the name it goes
            // by.
            if constructs_uncertain {
                if let Some(fixes) = self.definitions.fixes_by_name.get("__init__") {
                    self.retained.extend(fixes.iter().cloned());
                }
            }
        }
        self.skip(
            call.start(),
            name,
            "this call cannot be tied to the definition that was fixed".to_owned(),
        );
    }

    fn check_call(&mut self, call: &ast::ExprCall) {
        let name = match &*call.func {
            Expr::Name(name) => name.id.as_str(),
            Expr::Attribute(attribute) => attribute.attr.as_str(),
            _ => return,
        };
        let Some((signature, bound)) = self.resolve(&call.func) else {
            self.report_unresolved_call(call, name);
            return;
        };
        if !signature.complete {
            self.skip(
                call.start(),
                name,
                "the dataclass inherits fields, so its constructor is not known from the file \
                 that defines it"
                    .to_owned(),
            );
            return;
        }
        let arguments = match missing_arguments(&call.arguments, signature, bound) {
            Ok(arguments) => arguments,
            Err(reason) => {
                self.skip(call.start(), name, reason);
                return;
            }
        };
        if arguments.positional.is_empty() && arguments.keywords.is_empty() {
            return;
        }
        if !arguments.positional.is_empty() && !call.arguments.keywords.is_empty() {
            let positional = arguments.positional.join(", ");
            self.edits.push(Edit {
                range: TextRange::empty(call.arguments.keywords[0].start()),
                replacement: format!("{positional}, "),
                site: call.start(),
            });
            if arguments.keywords.is_empty() {
                return;
            }
        }
        let arguments = if call.arguments.keywords.is_empty() {
            arguments
                .positional
                .iter()
                .chain(&arguments.keywords)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            arguments.keywords.join(", ")
        };
        let last = call
            .arguments
            .args
            .iter()
            .map(Ranged::end)
            .chain(call.arguments.keywords.iter().map(Ranged::end))
            .max();
        // Inserting after the final argument rather than before the closing
        // parenthesis keeps trailing commas and trailing comments intact.
        let (offset, replacement) = last.map_or_else(
            || {
                (
                    call.arguments.range().end() - TextSize::from(1),
                    arguments.clone(),
                )
            },
            |end| (end, format!(", {arguments}")),
        );
        self.edits.push(Edit {
            range: TextRange::empty(offset),
            replacement,
            site: call.start(),
        });
    }

    /// Walk a comprehension, giving its targets a scope of their own.
    ///
    /// The leftmost iterable is evaluated in the scope containing the
    /// comprehension, so the targets do not shadow anything in it. Walking it
    /// inside the comprehension's scope would skip a call whose name matches a
    /// target, leaving that call unrewritten after its default was removed.
    fn visit_comprehension<'a>(
        &mut self,
        expression: &'a Expr,
        generators: &'a [ast::Comprehension],
    ) where
        Self: Visitor<'a>,
    {
        let Some((first, rest)) = generators.split_first() else {
            return;
        };
        self.visit_expr(&first.iter);
        self.scopes.push(BoundNames::of_comprehension(generators));
        for generator in std::iter::once(first).chain(rest) {
            if !std::ptr::eq(generator, first) {
                self.visit_expr(&generator.iter);
            }
            self.visit_expr(&generator.target);
            for condition in &generator.ifs {
                self.visit_expr(condition);
            }
        }
        match expression {
            Expr::ListComp(comprehension) => self.visit_expr(&comprehension.elt),
            Expr::SetComp(comprehension) => self.visit_expr(&comprehension.elt),
            Expr::Generator(comprehension) => self.visit_expr(&comprehension.elt),
            Expr::DictComp(comprehension) => {
                self.visit_expr(&comprehension.key);
                self.visit_expr(&comprehension.value);
            }
            _ => {}
        }
        self.scopes.pop();
    }

    /// Rewrite a bare decorator as an explicit one-argument wrapper when its
    /// implicit application relies on defaults removed from the decorator.
    fn check_bare_decorator(&mut self, expression: &Expr) {
        let name = match expression {
            Expr::Name(name) => name.id.as_str(),
            Expr::Attribute(attribute) => attribute.attr.as_str(),
            _ => return,
        };
        let Some((signature, bound)) = self.resolve(expression) else {
            if self.definitions.names.contains(name) {
                self.skip(
                    expression.start(),
                    name,
                    "this decorator cannot be tied to the definition that was fixed".to_owned(),
                );
            }
            return;
        };
        if !signature.kind.is_function() {
            return;
        }
        let arguments = match missing_arguments_for(signature, bound, 1, &[]) {
            Ok(arguments) => arguments,
            Err(reason) => {
                self.skip(expression.start(), name, reason);
                return;
            }
        };
        if arguments.positional.is_empty() && arguments.keywords.is_empty() {
            return;
        }
        let supplied = arguments
            .positional
            .iter()
            .chain(&arguments.keywords)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let original = &self.source[expression.start().to_usize()..expression.end().to_usize()];
        let mut parameter = "__no_defaults_decorated".to_owned();
        while original.contains(&parameter) {
            parameter.push('_');
        }
        self.edits.push(Edit {
            range: expression.range(),
            replacement: format!("lambda {parameter}: {original}({parameter}, {supplied})"),
            site: expression.start(),
        });
    }
}

/// The arguments a call must gain to keep meaning what it meant before the
/// defaults were removed, or the reason the call has to be left alone.
fn missing_arguments(
    call: &ast::Arguments,
    signature: &Signature,
    bound: usize,
) -> Result<MissingArguments, String> {
    if call
        .args
        .iter()
        .any(|argument| matches!(argument, Expr::Starred(_)))
        || call.keywords.iter().any(|keyword| keyword.arg.is_none())
    {
        return Err(
            "the call unpacks `*` or `**` arguments, so its arguments are not known".to_owned(),
        );
    }
    let named: Vec<&str> = call
        .keywords
        .iter()
        .filter_map(|keyword| keyword.arg.as_ref().map(ast::Identifier::as_str))
        .collect();
    let found = missing_arguments_for(signature, bound, call.args.len(), &named)?;
    // Python allows a bare generator expression as an argument only when it is
    // the sole one, so nothing can follow it. A call that needs nothing added
    // is not affected, so it is not worth a warning.
    if (!found.positional.is_empty() || !found.keywords.is_empty())
        && call.args.iter().any(
            |argument| matches!(argument, Expr::Generator(generator) if !generator.parenthesized),
        )
    {
        return Err(
            "the call's argument is a bare generator expression, which Python allows only \
             when it is the only one"
                .to_owned(),
        );
    }
    Ok(found)
}

fn missing_arguments_for(
    signature: &Signature,
    bound: usize,
    positional: usize,
    named: &[&str],
) -> Result<MissingArguments, String> {
    let mut appended: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    for removed in &signature.removed {
        if named.contains(&removed.parameter.as_str()) {
            continue;
        }
        let slot = signature
            .positional
            .iter()
            .position(|parameter| *parameter == removed.parameter);
        if slot.is_some_and(|slot| slot < bound) {
            continue;
        }
        if slot.is_some_and(|slot| slot >= bound && slot - bound < positional) {
            continue;
        }
        let Some(value) = &removed.value else {
            return Err(format!(
                "the default removed from `{}` is not a literal, so repeating it here \
                 could change what the call means",
                removed.parameter
            ));
        };
        if slot.is_some_and(|slot| slot < signature.positional_only) {
            // A positional-only argument must fill the very next positional
            // slot; it can be inserted before any existing keywords.
            if slot != Some(bound + positional + appended.len()) {
                return Err(format!(
                    "`{}` is positional-only and cannot be appended to this call",
                    removed.parameter
                ));
            }
            appended.push(value.clone());
        } else {
            keywords.push(format!("{}={}", removed.parameter, value));
        }
    }
    Ok(MissingArguments {
        positional: appended,
        keywords,
    })
}

struct MissingArguments {
    positional: Vec<String>,
    keywords: Vec<String>,
}

/// The names a statement binds itself, not counting what its body binds.
///
/// An import inside a loop is a new binding rather than a replacement of one,
/// so only the statement's own targets replace what an earlier import bound.
/// Statements in the body reach the same handling on their own.
fn rebound_names(statement: &Stmt) -> BTreeSet<String> {
    let mut bound = BoundNames::default();
    match statement {
        Stmt::Assign(assign) => assign.targets.iter().for_each(|target| bound.bind(target)),
        // `name: int` declares a type without binding anything, so whatever
        // the name already refers to still stands.
        Stmt::AnnAssign(assign) => {
            if assign.value.is_some() {
                bound.bind(&assign.target);
            }
        }
        Stmt::AugAssign(assign) => bound.bind(&assign.target),
        Stmt::For(loop_statement) => bound.bind(&loop_statement.target),
        Stmt::With(block) => {
            for item in &block.items {
                if let Some(target) = &item.optional_vars {
                    bound.bind(target);
                }
            }
        }
        // `except … as name` does not replace a module-level binding for later
        // statements: the name is deleted when the handler ends, and if the
        // handler never runs the prior binding is untouched.
        _ => {}
    }
    bound.names
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "each base-ten digit is reduced modulo 10 before conversion"
)]
fn integer_decimal_value(integer: &ast::Int) -> Option<String> {
    let spelling = integer.to_string().replace('_', "");
    let (radix, digits) = if let Some(digits) = spelling
        .strip_prefix("0x")
        .or_else(|| spelling.strip_prefix("0X"))
    {
        (16, digits)
    } else if let Some(digits) = spelling
        .strip_prefix("0o")
        .or_else(|| spelling.strip_prefix("0O"))
    {
        (8, digits)
    } else if let Some(digits) = spelling
        .strip_prefix("0b")
        .or_else(|| spelling.strip_prefix("0B"))
    {
        (2, digits)
    } else {
        (10, spelling.as_str())
    };
    let mut decimal = vec![0_u8];
    for character in digits.chars() {
        let mut carry = character.to_digit(radix)?;
        for digit in &mut decimal {
            let value = u32::from(*digit) * radix + carry;
            *digit = (value % 10) as u8;
            carry = value / 10;
        }
        while carry > 0 {
            decimal.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    Some(
        decimal
            .into_iter()
            .rev()
            .map(|digit| char::from(b'0' + digit))
            .collect(),
    )
}

fn python_int_equals_float(integer: &ast::Int, float: f64) -> bool {
    float.is_finite()
        && float >= 0.0
        && float.fract() == 0.0
        && integer_decimal_value(integer).is_some_and(|integer| integer == format!("{float:.0}"))
}

#[allow(
    clippy::float_cmp,
    reason = "Python literal equality is exact, including signed zero"
)]
fn python_floats_equal(left: f64, right: f64) -> bool {
    left == right
}

fn python_number_equals(left: &ComparableNumber<'_>, right: &ComparableNumber<'_>) -> bool {
    match (left, right) {
        (ComparableNumber::Int(left), ComparableNumber::Int(right)) => left == right,
        (ComparableNumber::Float(left), ComparableNumber::Float(right)) => {
            python_floats_equal(f64::from_bits(*left), f64::from_bits(*right))
        }
        (ComparableNumber::Int(integer), ComparableNumber::Float(float))
        | (ComparableNumber::Float(float), ComparableNumber::Int(integer)) => {
            python_int_equals_float(integer, f64::from_bits(*float))
        }
        (
            ComparableNumber::Complex {
                real: left_real,
                imag: left_imag,
            },
            ComparableNumber::Complex {
                real: right_real,
                imag: right_imag,
            },
        ) => {
            python_floats_equal(f64::from_bits(*left_real), f64::from_bits(*right_real))
                && python_floats_equal(f64::from_bits(*left_imag), f64::from_bits(*right_imag))
        }
        (
            ComparableNumber::Float(real),
            ComparableNumber::Complex {
                real: complex_real,
                imag,
            },
        )
        | (
            ComparableNumber::Complex {
                real: complex_real,
                imag,
            },
            ComparableNumber::Float(real),
        ) => {
            python_floats_equal(f64::from_bits(*imag), 0.0)
                && python_floats_equal(f64::from_bits(*real), f64::from_bits(*complex_real))
        }
        (ComparableNumber::Int(integer), ComparableNumber::Complex { real, imag })
        | (ComparableNumber::Complex { real, imag }, ComparableNumber::Int(integer)) => {
            python_floats_equal(f64::from_bits(*imag), 0.0)
                && python_int_equals_float(integer, f64::from_bits(*real))
        }
    }
}

impl<'a> Rewriter<'a> {
    /// Walk a `match` case body without letting what it binds stand for the
    /// cases after it.
    ///
    /// A later case runs only where this one did not, so an import or a
    /// capture written here is not something those cases can be assumed to
    /// see, and a name left uncertain is better than one taken on trust.
    fn visit_unselected_case_body(&mut self, body: &'a [Stmt]) {
        let before = self.bindings.first().cloned();
        self.visit_body(body);
        let (Some(before), Some(after)) = (before, self.bindings.first_mut()) else {
            return;
        };
        let changed: Vec<String> = after
            .iter()
            .filter(|(name, binding)| before.get(name.as_str()) != Some(binding))
            .map(|(name, _)| name.clone())
            .collect();
        for name in changed {
            after.remove(&name);
        }
    }

    /// Walk a class-body `match` case that cannot be selected statically,
    /// letting what its pattern captures take those names over, and report
    /// which names those were.
    ///
    /// A capture rebinds its name for the whole case body, so an import the
    /// class body made earlier no longer stands there. Walking the body
    /// without recording the capture would let a call in it resolve to that
    /// import and be rewritten against the wrong callable. A case after this
    /// one runs only where this pattern did not match, though, and there the
    /// import does still stand: the capture is undone once its own body has
    /// been walked, and it is only what follows the whole `match` that has to
    /// treat the captured names as uncertain.
    ///
    /// Undoing a capture puts the name back exactly as the case found it,
    /// dropping the binding again where there was none to displace. An import
    /// the body wrote for a captured name reached the later case no more than
    /// the capture itself did, so leaving that import in place would let the
    /// later case resolve the name to something it cannot see.
    fn visit_uncertain_class_case(&mut self, case: &'a ast::MatchCase) -> BTreeSet<String> {
        self.visit_pattern(&case.pattern);
        let mut captures = BoundNames::default();
        captures.visit_pattern(&case.pattern);
        let displaced: Vec<(String, Option<Binding>)> = self
            .class_bindings
            .last()
            .into_iter()
            .flat_map(|bindings| {
                captures
                    .names
                    .iter()
                    .map(|name| (name.clone(), bindings.get(name).cloned()))
            })
            .collect();
        let shadowed: Vec<String> = self
            .scopes
            .last()
            .into_iter()
            .flat_map(|scope| {
                captures
                    .names
                    .iter()
                    .filter(|name| !scope.names.contains(*name))
                    .cloned()
            })
            .collect();
        self.invalidate_class_bindings(captures.names.iter().cloned());
        if let Some(scope) = self.scopes.last_mut() {
            scope.names.extend(captures.names.iter().cloned());
        }
        if let Some(guard) = &case.guard {
            self.visit_expr(guard);
        }
        self.visit_body(&case.body);
        if let Some(bindings) = self.class_bindings.last_mut() {
            for (name, binding) in displaced {
                match binding {
                    Some(binding) => bindings.insert(name, binding),
                    None => bindings.remove(&name),
                };
            }
        }
        if let Some(scope) = self.scopes.last_mut() {
            for name in shadowed {
                scope.names.remove(&name);
            }
        }
        captures.names
    }
}

fn python_number_equals_bool(number: &ComparableNumber<'_>, value: bool) -> bool {
    let value = u8::from(value);
    match number {
        ComparableNumber::Int(integer) => **integer == value,
        ComparableNumber::Float(float) => {
            python_floats_equal(f64::from_bits(*float), f64::from(value))
        }
        ComparableNumber::Complex { real, imag } => {
            python_floats_equal(f64::from_bits(*imag), 0.0)
                && python_floats_equal(f64::from_bits(*real), f64::from(value))
        }
    }
}

fn python_literals_equal(left: &ComparableLiteral<'_>, right: &ComparableLiteral<'_>) -> bool {
    match (left, right) {
        (ComparableLiteral::Number(left), ComparableLiteral::Number(right)) => {
            python_number_equals(left, right)
        }
        (ComparableLiteral::Bool(value), ComparableLiteral::Number(number))
        | (ComparableLiteral::Number(number), ComparableLiteral::Bool(value)) => {
            python_number_equals_bool(number, **value)
        }
        _ => left == right,
    }
}

impl<'a> Visitor<'a> for Rewriter<'a> {
    fn visit_annotation(&mut self, annotation: &'a Expr) {
        if !self.postponed_annotations {
            self.visit_expr(annotation);
        }
    }

    fn visit_except_handler(&mut self, except_handler: &'a ast::ExceptHandler) {
        if self.in_class_scope() {
            let ast::ExceptHandler::ExceptHandler(handler) = except_handler;
            if let Some(type_) = &handler.type_ {
                self.visit_expr(type_);
            }
            let previous = handler.name.as_ref().and_then(|name| {
                self.scopes.last().map(|scope| {
                    (
                        scope.names.contains(name.as_str()),
                        scope.functions.contains(name.as_str()),
                        scope.classes.contains(name.as_str()),
                    )
                })
            });
            let previous_binding = handler.name.as_ref().and_then(|name| {
                self.class_bindings
                    .last_mut()
                    .and_then(|bindings| bindings.remove(name.as_str()))
            });
            if let Some(name) = &handler.name {
                if let Some(scope) = self.scopes.last_mut() {
                    scope.names.insert(name.to_string());
                    scope.functions.remove(name.as_str());
                    scope.classes.remove(name.as_str());
                }
            }
            self.visit_body(&handler.body);
            if let (Some(name), Some((was_name, was_function, was_class))) =
                (&handler.name, previous)
            {
                if let Some(scope) = self.scopes.last_mut() {
                    scope.names.remove(name.as_str());
                    scope.functions.remove(name.as_str());
                    scope.classes.remove(name.as_str());
                    if was_name {
                        scope.names.insert(name.to_string());
                    }
                    if was_function {
                        scope.functions.insert(name.to_string());
                    }
                    if was_class {
                        scope.classes.insert(name.to_string());
                    }
                }
            }
            if let (Some(name), Some(binding)) = (&handler.name, previous_binding) {
                if let Some(bindings) = self.class_bindings.last_mut() {
                    bindings.insert(name.to_string(), binding);
                }
            }
            return;
        }
        if !self.scopes.is_empty() {
            walk_except_handler(self, except_handler);
            return;
        }
        let ast::ExceptHandler::ExceptHandler(handler) = except_handler;
        if let Some(type_) = &handler.type_ {
            self.visit_expr(type_);
        }
        let replaced = handler.name.as_ref().map(ToString::to_string);
        let was_invalidated = replaced
            .as_ref()
            .is_some_and(|name| self.invalidated_bindings.contains(name));
        if let Some(name) = &replaced {
            self.invalidated_bindings.insert(name.clone());
        }
        self.visit_body(&handler.body);
        if let Some(name) = replaced.filter(|_| !was_invalidated) {
            self.invalidated_bindings.remove(&name);
        }
    }

    fn visit_decorator(&mut self, decorator: &'a ast::Decorator) {
        if matches!(decorator.expression, Expr::Name(_) | Expr::Attribute(_)) {
            self.called
                .insert((decorator.expression.start(), decorator.expression.end()));
            self.check_bare_decorator(&decorator.expression);
        }
        self.visit_expr(&decorator.expression);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "statement-order binding and lexical-scope transitions are kept in one dispatch"
    )]
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Import(_) | Stmt::ImportFrom(_) if self.scopes.is_empty() => {
                // An import affects only calls reached after it executes.
                let mut star_names = BTreeSet::new();
                if let Some(bindings) = self.bindings.last_mut() {
                    collect_bindings(
                        std::slice::from_ref(statement),
                        self.physical,
                        self.known,
                        bindings,
                    );
                    star_names = collect_star_bindings(
                        std::slice::from_ref(statement),
                        self.physical,
                        self.known,
                        self.definitions,
                        bindings,
                    );
                }
                for name in star_names {
                    self.invalidated_bindings.remove(&name);
                    self.rebound_classes.remove(&name);
                }
                let (resolutions, replaced_heads): (Vec<(String, bool)>, BTreeSet<String>) =
                    match statement {
                        Stmt::Import(import) => {
                            let mut resolutions: BTreeMap<String, (bool, bool)> = BTreeMap::new();
                            for alias in &import.names {
                                let name = alias.asname.as_ref().map_or_else(
                                    || {
                                        alias
                                            .name
                                            .split('.')
                                            .next()
                                            .unwrap_or(alias.name.as_str())
                                            .to_owned()
                                    },
                                    ToString::to_string,
                                );
                                let module_resolved = resolve_module(
                                    alias.name.as_str(),
                                    0,
                                    self.physical,
                                    self.known,
                                )
                                .is_some();
                                // `import pkg as pkg` binds the same package
                                // the dotted imports are under, so what they
                                // reached through that name is still there.
                                let rebinds = alias
                                    .asname
                                    .as_ref()
                                    .is_some_and(|asname| asname.as_str() != alias.name.as_str());
                                if rebinds {
                                    resolutions.insert(name, (module_resolved, true));
                                    continue;
                                }
                                // Once an alias has taken the head over, the
                                // dotted keys under it name what it used to
                                // be, so they say nothing about this import.
                                let aliased = resolutions
                                    .get(&name)
                                    .is_some_and(|(_, last_was_alias)| *last_was_alias);
                                let sibling_resolved = !aliased
                                    && alias.name.contains('.')
                                    && self.bindings.first().is_some_and(|bindings| {
                                        bindings.keys().any(|binding| {
                                            binding.contains('.')
                                                && binding.split('.').next() == Some(name.as_str())
                                        })
                                    });
                                let resolved = module_resolved || sibling_resolved;
                                match resolutions.entry(name) {
                                    Entry::Occupied(mut entry) => {
                                        let (known, last_was_alias) = entry.get_mut();
                                        if *last_was_alias {
                                            // An import that resolves to
                                            // nothing rebinds nothing, so the
                                            // alias before it still stands.
                                            if resolved {
                                                *known = resolved;
                                                *last_was_alias = false;
                                            }
                                        } else {
                                            *known |= resolved;
                                        }
                                    }
                                    Entry::Vacant(entry) => {
                                        entry.insert((resolved, false));
                                    }
                                }
                            }
                            let replaced = resolutions
                                .iter()
                                .filter(|(_, (_, last_was_alias))| *last_was_alias)
                                .map(|(name, _)| name.clone())
                                .collect();
                            (
                                resolutions
                                    .into_iter()
                                    .map(|(name, (resolved, _))| (name, resolved))
                                    .collect(),
                                replaced,
                            )
                        }
                        Stmt::ImportFrom(import) => {
                            let resolutions: Vec<_> = import
                                .names
                                .iter()
                                .filter(|alias| alias.name.as_str() != "*")
                                .map(|alias| {
                                    let name = alias.asname.as_ref().map_or_else(
                                        || alias.name.to_string(),
                                        ToString::to_string,
                                    );
                                    let resolved =
                                        self.bindings
                                            .first()
                                            .is_some_and(|bindings| bindings.contains_key(&name))
                                            || (import.module.as_ref().is_some_and(|module| {
                                                module.as_str() == "builtins"
                                            }) && alias.name.as_str() == "super");
                                    (name, resolved)
                                })
                                .collect();
                            let replaced =
                                resolutions.iter().map(|(name, _)| name.clone()).collect();
                            (resolutions, replaced)
                        }
                        _ => (Vec::new(), BTreeSet::new()),
                    };
                if let Some(bindings) = self.bindings.first_mut() {
                    bindings.retain(|binding, _| {
                        !replaced_heads.iter().any(|head| {
                            binding
                                .strip_prefix(head)
                                .is_some_and(|suffix| suffix.starts_with('.'))
                        })
                    });
                }
                for (name, resolved) in resolutions {
                    if resolved {
                        self.invalidated_bindings.remove(&name);
                        self.rebound_classes.remove(&name);
                    } else {
                        self.invalidated_bindings.insert(name.clone());
                        self.rebound_classes.insert(name);
                    }
                }
            }
            Stmt::Import(_) | Stmt::ImportFrom(_) if self.in_class_scope() => {
                if let Some(bindings) = self.class_bindings.last_mut() {
                    collect_bindings(
                        std::slice::from_ref(statement),
                        self.physical,
                        self.known,
                        bindings,
                    );
                    collect_star_bindings(
                        std::slice::from_ref(statement),
                        self.physical,
                        self.known,
                        self.definitions,
                        bindings,
                    );
                }
                self.bind_statement_in_class(statement);
            }
            // A suite inside a function records its imports as it walks, so
            // what it leaves behind has to be reconciled with what came before
            // it: an import in a branch that never runs must not replace the
            // binding the rest of the function is written against.
            // A class body has arms of its own further down that bind loop
            // targets and match captures, and a class nested in a function
            // would otherwise be caught here first.
            // `for` and `with` have arms of their own below that invalidate
            // what their targets bind, and their bodies do run, so an import
            // written in one stands afterwards. Only the suites that may not
            // run at all are reconciled here.
            Stmt::If(_) | Stmt::Try(_) | Stmt::While(_) | Stmt::Match(_)
                if self.bindings.len() > 1 && !self.in_class_scope() =>
            {
                let before = self.bindings.last().cloned();
                walk_stmt(self, statement);
                if let (Some(before), Some(after)) = (before, self.bindings.last_mut()) {
                    reconcile_suite_bindings(statement, &before, after);
                }
            }
            Stmt::Import(_) | Stmt::ImportFrom(_) => {
                if let Some(bindings) = self.bindings.last_mut() {
                    collect_bindings(
                        std::slice::from_ref(statement),
                        self.physical,
                        self.known,
                        bindings,
                    );
                    collect_star_bindings(
                        std::slice::from_ref(statement),
                        self.physical,
                        self.known,
                        self.definitions,
                        bindings,
                    );
                }
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::AugAssign(_) if self.in_class_scope() => {
                walk_stmt(self, statement);
                self.invalidate_class_bindings(rebound_names(statement));
                self.bind_statement_in_class(statement);
            }
            Stmt::Delete(delete) if self.in_class_scope() => {
                walk_stmt(self, statement);
                let direct = self
                    .class_direct_statements
                    .last()
                    .is_some_and(|statements| statements.contains(&statement.start()));
                if !direct {
                    return;
                }
                let mut deleted = BoundNames::default();
                for target in &delete.targets {
                    deleted.bind(target);
                }
                self.invalidate_class_bindings(deleted.names.iter().cloned());
                if let Some(scope) = self.scopes.last_mut() {
                    for name in deleted.names {
                        scope.names.remove(&name);
                        scope.functions.remove(&name);
                        scope.classes.remove(&name);
                    }
                }
            }
            Stmt::For(loop_statement) if self.in_class_scope() => {
                self.visit_expr(&loop_statement.iter);
                let statically_empty = match loop_statement.iter.as_ref() {
                    Expr::Tuple(tuple) => tuple.elts.is_empty(),
                    Expr::List(list) => list.elts.is_empty(),
                    Expr::Set(set) => set.elts.is_empty(),
                    Expr::Dict(dict) => dict.items.is_empty(),
                    _ => false,
                };
                if statically_empty {
                    self.visit_body(&loop_statement.orelse);
                    return;
                }
                self.visit_expr(&loop_statement.target);
                let mut target = BoundNames::default();
                target.bind(&loop_statement.target);
                self.invalidate_class_bindings(target.names.iter().cloned());
                if let Some(scope) = self.scopes.last_mut() {
                    scope.names.extend(target.names);
                }
                self.visit_body(&loop_statement.body);
                self.visit_body(&loop_statement.orelse);
            }
            Stmt::With(block) if self.in_class_scope() => {
                for item in &block.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(target) = &item.optional_vars {
                        self.visit_expr(target);
                        let mut bound = BoundNames::default();
                        bound.bind(target);
                        self.invalidate_class_bindings(bound.names.iter().cloned());
                        if let Some(scope) = self.scopes.last_mut() {
                            scope.names.extend(bound.names);
                        }
                    }
                }
                self.visit_body(&block.body);
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::AugAssign(_) if self.scopes.is_empty() => {
                // The assigned value is evaluated before its module-level
                // target replaces whatever an import bound there.
                walk_stmt(self, statement);
                let rebound = rebound_names(statement);
                self.invalidated_bindings.extend(rebound.iter().cloned());
                self.rebound_classes.extend(rebound);
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::AugAssign(_) => {
                walk_stmt(self, statement);
                if let Some(bindings) = self.bindings.last_mut() {
                    for name in rebound_names(statement) {
                        bindings.remove(&name);
                    }
                }
            }
            Stmt::For(loop_statement) if self.scopes.is_empty() => {
                // The iterable is evaluated before the first target binding.
                // Once an item is assigned, the body sees the target rather
                // than an import that previously used the same name.
                self.visit_expr(&loop_statement.iter);
                let statically_empty = match loop_statement.iter.as_ref() {
                    Expr::Tuple(tuple) => tuple.elts.is_empty(),
                    Expr::List(list) => list.elts.is_empty(),
                    Expr::Set(set) => set.elts.is_empty(),
                    Expr::Dict(dict) => dict.items.is_empty(),
                    _ => false,
                };
                if statically_empty {
                    self.visit_body(&loop_statement.orelse);
                    return;
                }
                self.visit_expr(&loop_statement.target);
                self.rebind_module_name(rebound_names(statement));
                self.visit_body(&loop_statement.body);
                let statically_nonempty = match loop_statement.iter.as_ref() {
                    Expr::Tuple(tuple) => !tuple.elts.is_empty(),
                    Expr::List(list) => !list.elts.is_empty(),
                    Expr::Set(set) => !set.elts.is_empty(),
                    Expr::Dict(dict) => !dict.items.is_empty(),
                    _ => false,
                };
                let definitely_breaks = loop_statement
                    .body
                    .last()
                    .is_some_and(|statement| matches!(statement, Stmt::Break(_)));
                if !(statically_nonempty && definitely_breaks) {
                    self.visit_body(&loop_statement.orelse);
                }
            }
            Stmt::With(block) if self.scopes.is_empty() => {
                // With-items enter from left to right. Each context expression
                // sees targets bound by preceding items, and the suite sees
                // every optional target.
                for item in &block.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(target) = &item.optional_vars {
                        self.visit_expr(target);
                        let mut rebound = BoundNames::default();
                        rebound.bind(target);
                        self.rebind_module_name(rebound.names);
                    }
                }
                self.visit_body(&block.body);
            }
            Stmt::For(loop_statement) => {
                // The iterable still sees the import; a non-empty loop target
                // replaces it before the body and later function statements.
                self.visit_expr(&loop_statement.iter);
                let statically_empty = match loop_statement.iter.as_ref() {
                    Expr::Tuple(tuple) => tuple.elts.is_empty(),
                    Expr::List(list) => list.elts.is_empty(),
                    Expr::Set(set) => set.elts.is_empty(),
                    Expr::Dict(dict) => dict.items.is_empty(),
                    _ => false,
                };
                if statically_empty {
                    self.visit_body(&loop_statement.orelse);
                    return;
                }
                self.visit_expr(&loop_statement.target);
                if let Some(bindings) = self.bindings.last_mut() {
                    for name in rebound_names(statement) {
                        bindings.remove(&name);
                    }
                }
                self.visit_body(&loop_statement.body);
                // The `else` runs only where the loop was not broken out of,
                // so what it binds is uncertain and is left alone rather than
                // taken as replacing what the body left behind.
                let before = self.bindings.last().cloned();
                self.visit_body(&loop_statement.orelse);
                if let (Some(before), Some(after)) = (before, self.bindings.last_mut()) {
                    let changed: Vec<String> = after
                        .iter()
                        .filter(|(name, binding)| before.get(name.as_str()) != Some(binding))
                        .map(|(name, _)| name.clone())
                        .collect();
                    for name in changed {
                        after.remove(&name);
                    }
                }
            }
            Stmt::With(block) => {
                // With-items enter left to right, and each optional target is
                // bound before the next context expression is evaluated.
                for item in &block.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(target) = &item.optional_vars {
                        self.visit_expr(target);
                        let mut rebound = BoundNames::default();
                        rebound.bind(target);
                        if let Some(bindings) = self.bindings.last_mut() {
                            for name in rebound.names {
                                bindings.remove(&name);
                            }
                        }
                    }
                }
                self.visit_body(&block.body);
            }
            Stmt::If(branch) if self.scopes.is_empty() => {
                let clauses = std::iter::once((Some(branch.test.as_ref()), branch.body.as_slice()))
                    .chain(
                        branch
                            .elif_else_clauses
                            .iter()
                            .map(|clause| (clause.test.as_ref(), clause.body.as_slice())),
                    );
                for (test, body) in clauses {
                    if let Some(test) = test {
                        self.visit_expr(test);
                    }
                    match test.map_or(Truthiness::True, |test| {
                        Truthiness::from_expr(test, |_| false)
                    }) {
                        Truthiness::False | Truthiness::Falsey | Truthiness::None => {}
                        Truthiness::True | Truthiness::Truthy => {
                            self.visit_body(body);
                            break;
                        }
                        Truthiness::Unknown => {
                            // Until paths are merged, preserve the conservative
                            // traversal for a condition whose result is not known.
                            self.visit_body(body);
                        }
                    }
                }
            }
            Stmt::Try(block)
                if self.scopes.is_empty()
                    && block
                        .body
                        .iter()
                        .all(|statement| matches!(statement, Stmt::Pass(_))) =>
            {
                // A suite made only of `pass` cannot transfer control to an
                // exception handler. Its else suite runs, followed by finally.
                self.visit_body(&block.body);
                self.visit_body(&block.orelse);
                self.visit_body(&block.finalbody);
            }
            Stmt::While(loop_statement) if self.scopes.is_empty() => {
                self.visit_expr(&loop_statement.test);
                match Truthiness::from_expr(&loop_statement.test, |_| false) {
                    Truthiness::False | Truthiness::Falsey | Truthiness::None => {
                        self.visit_body(&loop_statement.orelse);
                    }
                    truth => {
                        self.visit_body(&loop_statement.body);
                        let definitely_breaks = loop_statement
                            .body
                            .last()
                            .is_some_and(|statement| matches!(statement, Stmt::Break(_)));
                        let definitely_enters =
                            matches!(truth, Truthiness::True | Truthiness::Truthy);
                        if !(definitely_enters && definitely_breaks) {
                            self.visit_body(&loop_statement.orelse);
                        }
                    }
                }
            }
            Stmt::Match(match_statement) if self.in_class_scope() => {
                self.visit_expr(&match_statement.subject);
                let subject = match_statement
                    .subject
                    .as_literal_expr()
                    .map(ComparableLiteral::from);
                // A capture in a case that cannot be selected statically holds
                // only for that case's own body, but the class body after the
                // `match` is reached whichever case ran, so there every name
                // any of them captured is uncertain.
                let mut uncertain = BTreeSet::new();
                for case in &match_statement.cases {
                    let selected = match (&subject, &case.pattern) {
                        (Some(subject), Pattern::MatchSingleton(pattern)) => match pattern.value {
                            ast::Singleton::None => matches!(subject, ComparableLiteral::None),
                            ast::Singleton::True => {
                                matches!(subject, ComparableLiteral::Bool(value) if **value)
                            }
                            ast::Singleton::False => {
                                matches!(subject, ComparableLiteral::Bool(value) if !**value)
                            }
                        },
                        (Some(subject), Pattern::MatchValue(pattern)) => {
                            let Some(pattern) = pattern.value.as_literal_expr() else {
                                uncertain.extend(self.visit_uncertain_class_case(case));
                                continue;
                            };
                            python_literals_equal(subject, &ComparableLiteral::from(pattern))
                        }
                        (_, Pattern::MatchAs(pattern)) if pattern.pattern.is_none() => true,
                        _ => {
                            uncertain.extend(self.visit_uncertain_class_case(case));
                            continue;
                        }
                    };
                    if !selected {
                        continue;
                    }
                    self.visit_pattern(&case.pattern);
                    let mut captures = BoundNames::default();
                    captures.visit_pattern(&case.pattern);
                    self.invalidate_class_bindings(captures.names.iter().cloned());
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.names.extend(captures.names);
                    }
                    if let Some(guard) = &case.guard {
                        self.visit_expr(guard);
                        match Truthiness::from_expr(guard, |_| false) {
                            Truthiness::False | Truthiness::Falsey | Truthiness::None => {
                                continue;
                            }
                            Truthiness::Unknown => {
                                self.visit_body(&case.body);
                                continue;
                            }
                            Truthiness::True | Truthiness::Truthy => {}
                        }
                    }
                    self.visit_body(&case.body);
                    break;
                }
                self.invalidate_class_bindings(uncertain.iter().cloned());
                if let Some(scope) = self.scopes.last_mut() {
                    scope.names.extend(uncertain);
                }
            }
            Stmt::Match(match_statement) if self.scopes.is_empty() => {
                self.visit_expr(&match_statement.subject);
                let subject = match_statement
                    .subject
                    .as_literal_expr()
                    .map(ComparableLiteral::from);
                for case in &match_statement.cases {
                    let matches = match (&subject, &case.pattern) {
                        (Some(subject), Pattern::MatchSingleton(pattern)) => match pattern.value {
                            ast::Singleton::None => matches!(subject, ComparableLiteral::None),
                            ast::Singleton::True => {
                                matches!(subject, ComparableLiteral::Bool(value) if **value)
                            }
                            ast::Singleton::False => {
                                matches!(subject, ComparableLiteral::Bool(value) if !**value)
                            }
                        },
                        (Some(subject), Pattern::MatchValue(pattern)) => {
                            let Some(pattern) = pattern.value.as_literal_expr() else {
                                // A non-literal value pattern cannot be compared
                                // statically, so retain the conservative walk.
                                self.visit_pattern(&case.pattern);
                                if let Some(guard) = &case.guard {
                                    self.visit_expr(guard);
                                }
                                self.visit_body(&case.body);
                                continue;
                            };
                            python_literals_equal(subject, &ComparableLiteral::from(pattern))
                        }
                        (_, Pattern::MatchAs(pattern)) if pattern.pattern.is_none() => true,
                        _ => {
                            // A dynamic subject or pattern is not safe to
                            // select statically, so retain the conservative walk.
                            self.visit_pattern(&case.pattern);
                            if let Some(guard) = &case.guard {
                                self.visit_expr(guard);
                            }
                            self.visit_body(&case.body);
                            continue;
                        }
                    };
                    if !matches {
                        continue;
                    }
                    self.visit_pattern(&case.pattern);
                    let mut captures = BoundNames::default();
                    captures.visit_pattern(&case.pattern);
                    self.rebind_module_name(captures.names);
                    if let Some(guard) = &case.guard {
                        self.visit_expr(guard);
                        match Truthiness::from_expr(guard, |_| false) {
                            Truthiness::False | Truthiness::Falsey | Truthiness::None => {
                                continue;
                            }
                            Truthiness::Unknown => {
                                self.visit_unselected_case_body(&case.body);
                                continue;
                            }
                            Truthiness::True | Truthiness::Truthy => {}
                        }
                    }
                    self.visit_body(&case.body);
                    break;
                }
            }
            Stmt::ClassDef(class) => {
                let module_scope = self.scopes.is_empty();
                // The class header is evaluated before its local namespace is
                // populated, so body bindings cannot shadow header calls.
                for decorator in &class.decorator_list {
                    self.visit_decorator(decorator);
                }
                if let Some(type_params) = &class.type_params {
                    self.visit_type_params(type_params);
                }
                if let Some(arguments) = &class.arguments {
                    self.visit_arguments(arguments);
                }
                let qualified = if self.in_class_scope() {
                    qualified_name(self.classes.last().map(String::as_str), class.name.as_str())
                } else {
                    qualified_lexical_name(&self.lexical_scope, class.name.as_str())
                };
                self.classes.push(qualified);
                self.lexical_scope.push(class.name.to_string());
                self.lexical_is_class.push(true);
                self.scopes.push(BoundNames::default());
                self.class_bindings.push(BTreeMap::new());
                self.conditional_class_definitions.push(BTreeSet::new());
                self.class_scope_depths.push(self.scopes.len());
                self.class_direct_statements
                    .push(class.body.iter().map(Ranged::start).collect());
                self.visit_body(&class.body);
                self.class_direct_statements.pop();
                self.class_scope_depths.pop();
                self.conditional_class_definitions.pop();
                self.class_bindings.pop();
                self.scopes.pop();
                self.lexical_is_class.pop();
                self.lexical_scope.pop();
                self.classes.pop();
                self.bind_definition_in_class(class.name.as_str(), true, class.start());
                if !module_scope && !self.in_class_scope() {
                    if let Some(bindings) = self.bindings.last_mut() {
                        bindings.remove(class.name.as_str());
                    }
                }
                if module_scope
                    && self
                        .bindings
                        .first()
                        .is_some_and(|bindings| bindings.contains_key(class.name.as_str()))
                {
                    self.invalidated_bindings.insert(class.name.to_string());
                }
            }
            Stmt::FunctionDef(function) => {
                let module_scope = self.scopes.is_empty();
                // Decorators run while the function object is being created,
                // before names local to its body exist.
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
                // Type parameters, parameter defaults and annotations, and
                // the return annotation are evaluated outside the body too.
                if let Some(type_params) = &function.type_params {
                    self.visit_type_params(type_params);
                }
                self.visit_parameters(&function.parameters);
                if let Some(returns) = &function.returns {
                    self.visit_annotation(returns);
                }
                let function_scope = BoundNames::of_function(function);
                let direct_method = self.class_scope_depths.last() == Some(&self.scopes.len());
                let receiver_kind = if direct_method {
                    method_receiver(function, &self.aliases, &self.module_bindings)
                } else {
                    Receiver::None
                };
                let receiver_class = if direct_method {
                    self.classes.last().cloned()
                } else {
                    self.implicit_receiver_classes.last().cloned().flatten()
                };
                let receiver = if receiver_kind == Receiver::None {
                    self.implicit_receivers
                        .last()
                        .and_then(Clone::clone)
                        .filter(|receiver| !function_scope.names.contains(&receiver.name))
                } else {
                    function
                        .parameters
                        .posonlyargs
                        .first()
                        .or_else(|| function.parameters.args.first())
                        .map(|parameter| parameter.parameter.name.to_string())
                        .zip(self.classes.last().cloned())
                        .map(|(name, class)| ImplicitReceiver {
                            name,
                            class,
                            is_class: receiver_kind == Receiver::Class,
                        })
                };
                self.implicit_receivers.push(receiver);
                self.implicit_receiver_classes.push(receiver_class);
                self.bindings.push(BTreeMap::new());
                self.scopes.push(function_scope);
                self.binding_scope_depths.push(self.scopes.len() - 1);
                self.lexical_scope.push(function.name.to_string());
                self.lexical_is_class.push(false);
                self.visit_body(&function.body);
                self.lexical_is_class.pop();
                self.lexical_scope.pop();
                self.scopes.pop();
                self.bindings.pop();
                self.binding_scope_depths.pop();
                self.implicit_receiver_classes.pop();
                self.implicit_receivers.pop();
                if !module_scope && !self.in_class_scope() {
                    if let Some(bindings) = self.bindings.last_mut() {
                        bindings.remove(function.name.as_str());
                    }
                }
                self.bind_definition_in_class(function.name.as_str(), false, function.start());
                if module_scope {
                    self.rebound_classes.insert(function.name.to_string());
                }
                if module_scope
                    && self
                        .bindings
                        .first()
                        .is_some_and(|bindings| bindings.contains_key(function.name.as_str()))
                {
                    self.invalidated_bindings.insert(function.name.to_string());
                }
            }
            _ => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        let comprehension = match expression {
            Expr::ListComp(comprehension) => Some(comprehension.generators.as_slice()),
            Expr::SetComp(comprehension) => Some(comprehension.generators.as_slice()),
            Expr::DictComp(comprehension) => Some(comprehension.generators.as_slice()),
            Expr::Generator(comprehension) => Some(comprehension.generators.as_slice()),
            _ => None,
        };
        if let Some(generators) = comprehension {
            self.visit_comprehension(expression, generators);
            return;
        }
        match expression {
            Expr::Named(named) if self.in_class_scope() => {
                self.visit_expr(&named.value);
                self.visit_expr(&named.target);
                let mut bound = BoundNames::default();
                bound.bind(&named.target);
                // The class namespace resolves a name to an import bound in
                // the same body before anything else, so a target that took
                // the name over has to drop that import as an assignment does.
                self.invalidate_class_bindings(bound.names.iter().cloned());
                if let Some(scope) = self.scopes.last_mut() {
                    scope.names.extend(bound.names);
                }
                return;
            }
            Expr::Named(named) if self.scopes.is_empty() => {
                // A named expression evaluates its value before binding its
                // target. Calls in the value therefore still see an imported
                // callable, while calls reached afterwards see the replacement.
                self.visit_expr(&named.value);
                let mut rebound = BoundNames::default();
                rebound.bind(&named.target);
                self.rebind_module_name(rebound.names);
                return;
            }
            Expr::Named(named) => {
                self.visit_expr(&named.value);
                self.visit_expr(&named.target);
                let mut rebound = BoundNames::default();
                rebound.bind(&named.target);
                let function_depth = self
                    .binding_scope_depths
                    .last()
                    .copied()
                    .filter(|_| self.bindings.len() > 1);
                let lambda_owner = self
                    .lambda_scope_depths
                    .last()
                    .copied()
                    .filter(|lambda| function_depth.is_none_or(|function| *lambda > function));
                if let Some(lambda) = lambda_owner {
                    if let Some(scope) = self.scopes.get_mut(lambda) {
                        scope.names.extend(rebound.names);
                    }
                } else if let Some(bindings) = self
                    .bindings
                    .last_mut()
                    .filter(|_| function_depth.is_some())
                {
                    for name in rebound.names {
                        bindings.remove(&name);
                    }
                } else {
                    self.invalidated_bindings
                        .extend(rebound.names.iter().cloned());
                    self.rebound_classes.extend(rebound.names);
                }
                return;
            }
            Expr::Call(call) => {
                self.called.insert((call.func.start(), call.func.end()));
                self.check_call(call);
            }
            Expr::Name(_) | Expr::Attribute(_) => self.check_reference(expression),
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.visit_parameters(parameters);
                }
                let scope = BoundNames::of_lambda(lambda);
                let receiver = self
                    .implicit_receivers
                    .last()
                    .and_then(Clone::clone)
                    .filter(|receiver| !scope.names.contains(&receiver.name));
                // A lambda written straight in a class body belongs to that
                // class as much as a `def` there does, so Python hands it the
                // class's own `__class__` cell. Written anywhere else it keeps
                // the cell of whatever function holds it, which is why the
                // stack is pushed either way.
                let receiver_class = if self.class_scope_depths.last() == Some(&self.scopes.len()) {
                    self.classes.last().cloned()
                } else {
                    self.implicit_receiver_classes.last().cloned().flatten()
                };
                self.implicit_receivers.push(receiver);
                self.implicit_receiver_classes.push(receiver_class);
                self.scopes.push(scope);
                self.lambda_scope_depths.push(self.scopes.len() - 1);
                self.visit_expr(&lambda.body);
                self.lambda_scope_depths.pop();
                self.scopes.pop();
                self.implicit_receiver_classes.pop();
                self.implicit_receivers.pop();
                return;
            }
            _ => {}
        }
        walk_expr(self, expression);
    }
}

/// The offset of the first character of the line containing `offset`.
fn line_start(source: &str, offset: TextSize) -> TextSize {
    text_size(previous_line_start(source, offset.to_usize()))
}

/// The offset each line of a source starts at.
///
/// Built once per file, so converting an offset to a line is a binary search
/// rather than a scan from the top of the file. Scanning made producing a
/// file's diagnostics quadratic in how many it holds.
struct LineIndex(Vec<usize>);

impl LineIndex {
    fn new(source: &str) -> Self {
        Self(source_line_starts(source))
    }

    /// The one-based line and column of `offset`, with the column counted in
    /// characters.
    ///
    /// Ruff, whose concise format this imitates, reports character columns.
    /// Byte offsets would shift the reported column of anything that follows
    /// non-ASCII text on its line, and put the caret in `full` output that
    /// many cells too far right.
    fn locate(&self, source: &str, offset: TextSize) -> (usize, usize) {
        let offset = offset.to_usize();
        // Every source has a line starting at 0, so this is never zero.
        let line = self.0.partition_point(|start| *start <= offset);
        let start = self.0.get(line - 1).copied().unwrap_or(0);
        (line, source[start..offset].chars().count() + 1)
    }
}

/// The line and column of a single offset, for the callers that need only one.
fn line_column(source: &str, offset: TextSize) -> (usize, usize) {
    LineIndex::new(source).locate(source, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The field-carrying base classes a run with no configuration uses.
    fn default_bases() -> FieldBases {
        FieldBases::new(&default_field_base_classes())
    }

    fn positions(source: &str) -> Vec<(usize, usize)> {
        check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            false,
        )
        .diagnostics
        .into_iter()
        .map(|item| (item.line, item.column))
        .collect()
    }

    #[test]
    fn indexed_lines_match_walking_the_source() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        for text in [
            "one\ntwo\nthree\n",
            "one\ntwo\nthree",
            "one\r\ntwo\r\n",
            "\n\nthird\n",
            "single",
            "trailing\n\n",
            "",
        ] {
            std::fs::write(&path, text).map_err(|error| error.to_string())?;
            let source = SourceLines::read(&path).ok_or("expected to read the file")?;
            let walked: Vec<&str> = text.lines().collect();
            let indexed: Vec<&str> = (1..=walked.len())
                .map(|number| source.line(number).unwrap_or("<missing>"))
                .collect();
            assert_eq!(indexed, walked, "{text:?}");
            assert_eq!(source.line(walked.len() + 1), None, "{text:?}");
            assert_eq!(source.line(0), None, "{text:?}");
        }
        Ok(())
    }

    #[test]
    fn the_line_index_agrees_with_scanning_the_source() {
        for source in [
            "one\ntwo\nthree\n",
            "one\ntwo\nthree",
            "one\r\ntwo\r\n",
            "\n\nthird\n",
            "single",
            "trailing\n\n",
            "ä\nlonger ä line\n",
            "",
        ] {
            let index = LineIndex::new(source);
            for offset in 0..=source.len() {
                if !source.is_char_boundary(offset) {
                    continue;
                }
                let offset = TextSize::new(u32::try_from(offset).unwrap_or(u32::MAX));
                let before = &source[..offset.to_usize()];
                let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
                let start = before.rfind('\n').map_or(0, |position| position + 1);
                assert_eq!(
                    index.locate(source, offset),
                    (line, before[start..].chars().count() + 1),
                    "{source:?} at {offset:?}"
                );
            }
        }
    }

    #[test]
    fn columns_count_characters_rather_than_bytes() {
        // `ä` is two bytes, so a byte column would report 10 for the `1`.
        assert_eq!(positions("def f(ä=1):\n    pass\n"), [(1, 9)]);
        assert_eq!(positions("def f(x=1):\n    pass\n"), [(1, 9)]);
        // An emoji outside the basic multilingual plane is one character.
        assert_eq!(positions("def f(𝔞=1):\n    pass\n"), [(1, 9)]);
        // Lines after the first are measured from their own start.
        assert_eq!(positions("# ä\ndef f(ä=1): pass\n"), [(2, 9)]);
    }

    fn messages(source: &str, private_only: bool) -> Vec<String> {
        check_source(
            Path::new("fixture.py"),
            source,
            private_only,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            false,
        )
        .diagnostics
        .into_iter()
        .map(|item| item.message)
        .collect()
    }

    fn codes(source: &str) -> Vec<&'static str> {
        check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            false,
        )
        .diagnostics
        .into_iter()
        .map(|item| item.code)
        .collect()
    }

    /// Fix every file in `directory` the way `--fix` does, and report the calls
    /// that were left alone.
    fn fix_all(files: &[PathBuf]) -> Result<Vec<Skipped>, String> {
        let mut diagnostics = Vec::new();
        let mut signatures = Vec::new();
        let mut skipped = Vec::new();
        for path in files {
            let checked = check_file(
                path,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            diagnostics.extend(checked.diagnostics);
            signatures.extend(checked.signatures);
            skipped.extend(checked.skipped);
        }
        let mut call_sites = call_site_edits(files, signatures)?;
        call_sites.skipped.extend(skipped);
        for diagnostic in &diagnostics {
            let Some(range) = diagnostic.fix else {
                continue;
            };
            call_sites
                .edits
                .entry(diagnostic.path.clone())
                .or_default()
                .push(Edit::deletion(range));
        }
        let mut updated = 0;
        let mut unfixed = BTreeSet::new();
        write_fixes_atomically(fixed_sources(call_sites.edits, &mut updated, &mut unfixed)?)?;
        for path in files {
            assert!(
                check_file(
                    path,
                    false,
                    Path::new(""),
                    &Reexports::default(),
                    &default_bases(),
                    false,
                )
                .diagnostics
                .is_empty(),
                "{}",
                path.display()
            );
        }
        Ok(call_sites.skipped)
    }

    fn fixed(source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(&path, source).map_err(|error| error.to_string())?;
        fix_all(std::slice::from_ref(&path))?;
        std::fs::read_to_string(&path).map_err(|error| error.to_string())
    }

    /// Fix `source` and report why any call in it was left alone.
    fn skipped_reasons(source: &str) -> Result<Vec<String>, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(&path, source).map_err(|error| error.to_string())?;
        Ok(fix_all(std::slice::from_ref(&path))?
            .into_iter()
            .map(|skip| skip.reason)
            .collect())
    }

    #[test]
    fn an_annotation_does_not_replace_a_class() -> Result<(), String> {
        // `C: object` declares a type and binds nothing, so `C` is still the
        // class defined above it.
        let source = "class C:\n    def method(self, value=1): pass\n\nC: object\n\nC().method()\n";
        let updated = fixed(source)?;
        assert!(updated.ends_with("C().method(value=1)\n"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_dead_branch_import_does_not_replace_a_function_binding() -> Result<(), String> {
        // The `if False:` suite never runs, so `target` is still the one the
        // function imported above it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let other = directory.path().join("other.py");
        let user = directory.path().join("user.py");
        std::fs::write(&api, "def target(alpha=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&other, "def target(beta=2): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "def run():\n    from api import target\n    if False:\n        from other import target\n    target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, other, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert!(updated.ends_with("    target(alpha=1)\n"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_rebound_name_is_no_longer_the_class() -> Result<(), String> {
        // A loop, context-manager, or walrus target replaces the class, so a
        // later call through the name must not be rewritten against it.
        for rebinding in [
            "for C in items:\n    pass\n",
            "with open(path) as C:\n    pass\n",
            "if (C := make()):\n    pass\n",
        ] {
            let source = format!(
                "class C:\n    def method(self, value=1): pass\n\n{rebinding}\nC().method()\n"
            );
            let updated = fixed(&source)?;
            assert!(updated.ends_with("C().method()\n"), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_package_definition_outranks_a_sibling_submodule() -> Result<(), String> {
        // `from . import helper` reaches the package's own `helper`, since the
        // attribute is already set when the submodule would be imported.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("pkg");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let init = package.join("__init__.py");
        let submodule = package.join("helper.py");
        let user = package.join("user.py");
        std::fs::write(&init, "def helper(value=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&submodule, "OTHER = 1\n").map_err(|error| error.to_string())?;
        std::fs::write(&user, "from . import helper\n\nhelper()\n")
            .map_err(|error| error.to_string())?;
        fix_all(&[init, submodule, user.clone()])?;
        assert_eq!(
            std::fs::read_to_string(&user).map_err(|error| error.to_string())?,
            "from . import helper\n\nhelper(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_nested_dataclass_is_reached_from_a_deeper_scope() -> Result<(), String> {
        // `C` inside `inner` is the class `outer` holds, not the module-level
        // one that happens to share its name.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    other: int = 2\n\ndef outer():\n    @dataclass\n    class C:\n        value: int = 1\n    def inner():\n        return C()\n    return inner()\n";
        let updated = fixed(source)?;
        assert!(updated.contains("return C(value=1)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_wrapper_alias_under_the_method_name_keeps_its_signature() -> Result<(), String> {
        // `target = staticmethod(target)` renames nothing: the wrapper decides
        // how the one name is called, so the call still has to be rewritten.
        for (source, expected) in [
            (
                "class C:\n    def target(value=1): pass\n    target = staticmethod(target)\n\nC.target()\n",
                "class C:\n    def target(value): pass\n    target = staticmethod(target)\n\nC.target(value=1)\n",
            ),
            (
                "class C:\n    def target(cls, value=1): pass\n    target = classmethod(target)\n\nC.target()\n",
                "class C:\n    def target(cls, value): pass\n    target = classmethod(target)\n\nC.target(value=1)\n",
            ),
        ] {
            assert_eq!(fixed(source)?, expected);
        }
        Ok(())
    }

    #[test]
    fn a_wrapped_inherited_attribute_alias_is_indexed() -> Result<(), String> {
        // The wrapper says how the alias is called; `Base.target` inside it
        // still names the method the signature was collected for.
        for (source, expected) in [
            (
                "class Base:\n    def target(value=1): pass\n\nclass Child:\n    alias = staticmethod(Base.target)\n\nChild.alias()\n",
                "class Base:\n    def target(value): pass\n\nclass Child:\n    alias = staticmethod(Base.target)\n\nChild.alias(value=1)\n",
            ),
            (
                "class Base:\n    def target(cls, value=1): pass\n\nclass Child:\n    alias = classmethod(Base.target)\n\nChild.alias()\n",
                "class Base:\n    def target(cls, value): pass\n\nclass Child:\n    alias = classmethod(Base.target)\n\nChild.alias(value=1)\n",
            ),
        ] {
            assert_eq!(fixed(source)?, expected);
        }
        Ok(())
    }

    #[test]
    fn aliased_and_qualified_wrappers_index_class_aliases() -> Result<(), String> {
        // The decorator forms already accept these spellings, so a class-body
        // wrapper call has to recognise them too.
        for (source, expected) in [
            (
                "from builtins import staticmethod as sm\n\nclass Base:\n    def target(value=1): pass\n\nclass Child:\n    alias = sm(Base.target)\n\nChild.alias()\n",
                "from builtins import staticmethod as sm\n\nclass Base:\n    def target(value): pass\n\nclass Child:\n    alias = sm(Base.target)\n\nChild.alias(value=1)\n",
            ),
            (
                "import builtins\n\nclass Base:\n    def target(value=1): pass\n\nclass Child:\n    alias = builtins.staticmethod(Base.target)\n\nChild.alias()\n",
                "import builtins\n\nclass Base:\n    def target(value): pass\n\nclass Child:\n    alias = builtins.staticmethod(Base.target)\n\nChild.alias(value=1)\n",
            ),
        ] {
            assert_eq!(fixed(source)?, expected);
        }
        Ok(())
    }

    #[test]
    fn a_property_alias_in_another_class_retains_the_default() {
        // Reading `Child.alias` runs `Base.target`, so there is no call site
        // to carry the default to — wherever the alias is written.
        for source in [
            "class Base:\n    def target(self, value=1): pass\n    alias = property(target)\n",
            "class Base:\n    def target(self, value=1): pass\n\nclass Child:\n    alias = property(Base.target)\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1);
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
        }
    }

    #[test]
    fn property_aliases_are_found_however_they_are_written() {
        // The wrapper spellings the decorator path accepts, and a class body
        // suite, all still mean the getter runs on attribute access.
        for source in [
            "from builtins import property as prop\n\nclass Base:\n    def target(self, value=1): pass\n\nclass Child:\n    alias = prop(Base.target)\n",
            "import builtins\n\nclass Base:\n    def target(self, value=1): pass\n\nclass Child:\n    alias = builtins.property(Base.target)\n",
            "class Base:\n    def target(self, value=1): pass\n\nclass Child:\n    if cond:\n        alias = property(Base.target)\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1);
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
        }
    }

    #[test]
    fn a_nearer_binding_hides_an_outer_callable() -> Result<(), String> {
        // `inner` binds `C` itself, so the dataclass in `outer` is not what
        // the call reaches, even though nothing was recorded for the inner one.
        let source = "from dataclasses import dataclass\n\ndef outer():\n    @dataclass\n    class C:\n        value: int = 1\n    def inner():\n        class C:\n            pass\n        return C()\n    return inner()\n";
        let updated = fixed(source)?;
        assert!(updated.contains("return C()"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_method_does_not_see_a_class_nested_callable() -> Result<(), String> {
        // A class body is not a closure scope: `C` inside the method is the
        // module-level class, not the one nested beside the method.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int = 1\n\nclass Holder:\n    @dataclass\n    class C:\n        other: int = 2\n    def method(self):\n        return C()\n";
        let updated = fixed(source)?;
        assert!(updated.contains("return C(value=1)"), "{updated}");
        Ok(())
    }

    #[test]
    fn an_expression_scope_in_a_class_body_looks_past_the_class() -> Result<(), String> {
        // A lambda and a comprehension each have a scope the class namespace
        // is not part of, so `C` in them is the module-level class. Written
        // directly in the body it is the nested one.
        let holder = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int = 1\n\nclass Holder:\n    @dataclass\n    class C:\n        other: int = 2\n";
        for (written, expected) in [
            (
                "    factory = lambda: C()\n",
                "    factory = lambda: C(value=1)\n",
            ),
            (
                "    items = [C() for _ in range(1)]\n",
                "    items = [C(value=1) for _ in range(1)]\n",
            ),
            ("    made = C()\n", "    made = C(other=2)\n"),
        ] {
            let updated = fixed(&format!("{holder}{written}"))?;
            assert!(updated.ends_with(expected), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_package_reexport_outranks_a_sibling_submodule() -> Result<(), String> {
        // `__init__.py` puts a symbol under `helper`, so that is the package
        // attribute even though a `helper` submodule sits beside it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("pkg");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let init = package.join("__init__.py");
        let implementation = package.join("impl.py");
        let submodule = package.join("helper.py");
        let user = package.join("user.py");
        std::fs::write(&init, "from .impl import helper\n").map_err(|error| error.to_string())?;
        std::fs::write(&implementation, "def helper(value=1): pass\n")
            .map_err(|error| error.to_string())?;
        std::fs::write(&submodule, "OTHER = 1\n").map_err(|error| error.to_string())?;
        std::fs::write(&user, "from . import helper\n\nhelper()\n")
            .map_err(|error| error.to_string())?;
        fix_all(&[init, implementation, submodule, user.clone()])?;
        assert_eq!(
            std::fs::read_to_string(&user).map_err(|error| error.to_string())?,
            "from . import helper\n\nhelper(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn an_annotated_cross_file_alias_is_followed() -> Result<(), String> {
        // `target: object = api.target` names the same callable a plain
        // assignment would.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let reexport = directory.path().join("re_export.py");
        let user = directory.path().join("user.py");
        std::fs::write(&api, "def target(value=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&reexport, "import api\n\ntarget: object = api.target\n")
            .map_err(|error| error.to_string())?;
        std::fs::write(&user, "from re_export import target\n\ntarget()\n")
            .map_err(|error| error.to_string())?;
        fix_all(&[api, reexport, user.clone()])?;
        assert_eq!(
            std::fs::read_to_string(&user).map_err(|error| error.to_string())?,
            "from re_export import target\n\ntarget(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_loop_else_import_is_not_discarded() -> Result<(), String> {
        // `while False:` never iterates, which is exactly when its `else`
        // runs, so the import in there is the one that stands.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let other = directory.path().join("other.py");
        let user = directory.path().join("user.py");
        std::fs::write(&api, "def target(alpha=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&other, "def target(beta=2): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "def run():\n    from api import target\n    while False:\n        pass\n    else:\n        from other import target\n    target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, other, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert!(!updated.contains("target(alpha=1)"), "{updated}");
        Ok(())
    }

    #[test]
    fn detects_every_parameter_kind() {
        let found = messages("def f(a=1, /, b=2, *, c=3): pass\n", false);
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn detects_dataclass_defaults_but_not_class_vars() {
        let found = messages(
            "from typing import ClassVar\n\n@dataclass\nclass C:\n x: int = 1\n y: list = field(default_factory=list)\n z: ClassVar[int] = 2\n no_default: int = field()\n",
            false,
        );
        assert_eq!(found.len(), 2);
        assert!(found[1].contains("default factory"));
    }

    /// Check `source` with `bases` standing in for the configured base classes.
    fn messages_with_bases(source: &str, bases: &[&str]) -> Vec<String> {
        let names: Vec<String> = bases.iter().map(|name| (*name).to_owned()).collect();
        check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &FieldBases::new(&names),
            false,
        )
        .diagnostics
        .into_iter()
        .map(|item| item.message)
        .collect()
    }

    #[test]
    fn detects_defaults_on_a_pydantic_model() {
        let found = messages(
            "class Job(BaseModel):\n x: int = 1\n y: list = Field(default_factory=list)\n",
            false,
        );
        assert_eq!(
            found,
            [
                "class field `x` has a default",
                "class field `y` has a default factory",
            ]
        );
    }

    #[test]
    fn pydantic_private_attribute_defaults_are_not_model_fields() {
        assert!(messages(
            "from pydantic import BaseModel, PrivateAttr\n\nclass C(BaseModel):\n    _value: int = PrivateAttr(default=1)\n",
            false,
        )
        .is_empty());
    }

    #[test]
    fn underscore_model_attributes_are_not_constructor_fields() {
        assert!(messages(
            "from pydantic import BaseModel\n\nclass C(BaseModel):\n    _value: int = 1\n",
            false,
        )
        .is_empty());
    }

    #[test]
    fn detects_defaults_on_lambda_parameters() {
        assert_eq!(
            messages("lam = lambda z=4: z\n", false),
            ["parameter `z` of lambda has a default"]
        );
        assert_eq!(
            messages("key = lambda a, b=1, *, c=2: (a, b, c)\n", false).len(),
            2,
            "every parameter kind a lambda has counts, as for a `def`"
        );
        assert!(
            messages("plain = lambda q: q\n", false).is_empty(),
            "a lambda without defaults is not a violation"
        );
        assert_eq!(
            messages("def f(callback=lambda z=1: z): pass\n", false).len(),
            2,
            "a lambda inside a signature keeps its own violation"
        );
    }

    #[test]
    fn a_lambda_takes_the_privacy_of_the_scope_holding_it() {
        assert!(
            messages("lam = lambda z=4: z\n", true).is_empty(),
            "a lambda has no name of its own to call private"
        );
        assert_eq!(
            messages("def _helper():\n    return lambda z=4: z\n", true),
            ["parameter `z` of lambda has a default"]
        );
    }

    #[test]
    fn a_lambda_default_is_suppressible_and_fixable() -> Result<(), String> {
        assert!(codes("lam = lambda z=4: z  # noqa: NOD001\n").is_empty());
        assert_eq!(fixed("lam = lambda z=4: z\n")?, "lam = lambda z: z\n");
        Ok(())
    }

    #[test]
    fn a_parenthesized_parameter_default_is_removed_with_its_parentheses() -> Result<(), String> {
        assert_eq!(
            fixed("def f(x=(1), y=2):\n    pass\n")?,
            "def f(x, y):\n    pass\n"
        );
        Ok(())
    }

    #[test]
    fn a_parenthesized_lambda_default_is_removed_with_its_parentheses() -> Result<(), String> {
        assert_eq!(
            fixed("handler = lambda x=(1): x\n")?,
            "handler = lambda x: x\n"
        );
        Ok(())
    }

    #[test]
    fn a_parenthesized_dataclass_field_default_is_removed_with_its_parentheses(
    ) -> Result<(), String> {
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass A:\n    x: int = (1)\n    y: int = 2\n";
        assert_eq!(
            fixed(source)?,
            "from dataclasses import dataclass\n\n@dataclass\nclass A:\n    x: int\n    y: int\n"
        );
        Ok(())
    }

    #[test]
    fn lambda_defaults_keep_positional_order_in_stubs() -> Result<(), String> {
        let source = "handler = lambda first=..., second=2: second\n";
        assert_eq!(stub_fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn removing_a_lambda_default_warns_that_its_calls_were_left_alone() -> Result<(), String> {
        let reasons = skipped_reasons("lam = lambda z=4: z\n")?;
        assert_eq!(
            reasons,
            ["an anonymous function has no name to resolve a call through"]
        );
        assert!(
            skipped_reasons("plain = lambda q: q\n")?.is_empty(),
            "a lambda that was not changed has nothing to warn about"
        );
        Ok(())
    }

    #[test]
    fn an_aliased_dataclass_import_is_still_a_dataclass() {
        assert_eq!(
            messages(
                "from dataclasses import dataclass as dc\n\n@dc\nclass C:\n    x: int = 1\n",
                false
            ),
            ["dataclass field `x` has a default"]
        );
        assert_eq!(
            messages(
                "import dataclasses as dcs\n\n@dcs.dataclass\nclass C:\n    x: int = 1\n",
                false
            ),
            ["dataclass field `x` has a default"],
            "a module alias already worked, because an attribute keeps its last segment"
        );
        assert_eq!(
            messages(
                "from dataclasses import dataclass as dc\n\n@dc(frozen=True)\nclass C:\n    x: int = 1\n",
                false
            ),
            ["dataclass field `x` has a default"],
            "a called decorator resolves through its function"
        );
    }

    #[test]
    fn function_parameters_shadow_dataclass_decorator_imports() -> Result<(), String> {
        let source = "from dataclasses import dataclass\n\ndef identity(cls): return cls\ndef outer(dataclass):\n    @dataclass\n    class C:\n        x: int = 1\n    return C\n\nassert outer(identity).x == 1\n";
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn function_parameters_shadow_dataclass_field_imports() {
        let source = "from dataclasses import dataclass, field\n\ndef helper(*, default, repr): return default\ndef outer(field):\n    @dataclass\n    class C:\n        x: int = field(default=1, repr=False)\n    return C()\n\nassert outer(helper).x == 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn function_parameters_shadow_class_var_imports() {
        let source = "from dataclasses import dataclass\nfrom typing import ClassVar\n\nclass Marker:\n    def __getitem__(self, item): return item\n\ndef outer(ClassVar):\n    @dataclass\n    class C:\n        x: ClassVar[int] = 1\n    return C\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].message.contains("field `x`"));
    }

    #[test]
    fn function_parameters_shadow_dataclasses_missing() {
        let source = "from dataclasses import MISSING, dataclass\n\ndef outer(MISSING):\n    @dataclass\n    class C:\n        x: int = MISSING\n    return C\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].message.contains("field `x`"));
    }

    #[test]
    fn function_parameters_shadow_classmethod_imports() {
        let source = "from builtins import classmethod as cm\n\ndef identity(function): return function\ndef outer(cm):\n    class C:\n        @cm\n        def f(cls, value=1): return value\n    return C.f(C())\n\nassert outer(identity) == 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn function_parameters_shadow_structural_base_imports() {
        let source = "from dataclasses import dataclass\nfrom typing import Generic\n\n@dataclass\nclass Parent:\n    inherited: int = 1  # noqa: NOD001\n\ndef outer(Generic):\n    @dataclass\n    class Child(Generic):\n        own: int = 2\n    return Child\n\nassert outer(Parent)().own == 2\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn a_function_local_dataclass_alias_does_not_escape() {
        assert!(messages(
            "def dc(cls): return cls\n\ndef load():\n    from dataclasses import dataclass as dc\n\n@dc\nclass C:\n    value: int = 1\n",
            false,
        )
        .is_empty());
    }

    #[test]
    fn an_aliased_field_import_is_still_a_field() {
        assert_eq!(
            messages(
                "from dataclasses import dataclass, field as fld\n\n@dataclass\nclass C:\n    y: list = fld(default_factory=list)\n",
                false
            ),
            ["dataclass field `y` has a default factory"]
        );
        assert_eq!(
            messages(
                "from dataclasses import dataclass, field as fld\n\n@dataclass\nclass C:\n    y: list = fld()\n",
                false
            ),
            Vec::<String>::new(),
            "a `field()` with no default is not one under an alias either"
        );
        assert_eq!(
            messages(
                "from pydantic import BaseModel, Field as F\n\nclass M(BaseModel):\n    a: int = F(default=3)\n",
                false
            ),
            ["class field `a` has a default"]
        );
    }

    #[test]
    fn a_function_local_field_alias_does_not_escape() -> Result<(), String> {
        let source = "from dataclasses import dataclass\n\ndef helper(value): return value\ndef load():\n    from dataclasses import field as helper\n\n@dataclass\nclass C:\n    value: int = helper(1)\n";
        assert_eq!(
            fixed(source)?,
            "from dataclasses import dataclass\n\ndef helper(value): return value\ndef load():\n    from dataclasses import field as helper\n\n@dataclass\nclass C:\n    value: int\n"
        );
        Ok(())
    }

    #[test]
    fn an_aliased_kw_only_marker_still_declares_no_field() {
        assert_eq!(
            messages(
                "from dataclasses import dataclass, KW_ONLY as KO\n\n@dataclass\nclass C:\n    _: KO\n    z: int = 2\n",
                false
            ),
            ["dataclass field `z` has a default"],
            "the marker itself is not a field"
        );
    }

    #[test]
    fn a_quoted_kw_only_marker_still_declares_no_field() -> Result<(), String> {
        let source = "from dataclasses import dataclass, KW_ONLY\n\n@dataclass\nclass C:\n    _: \"KW_ONLY\"\n    first: int = ...\n    second: int = 2\n";
        assert_eq!(
            stub_fixed(source)?,
            "from dataclasses import dataclass, KW_ONLY\n\n@dataclass\nclass C:\n    _: \"KW_ONLY\"\n    first: int = ...\n    second: int\n"
        );
        Ok(())
    }

    #[test]
    fn an_alias_from_an_unrelated_module_is_not_resolved() {
        assert!(
            messages(
                "from elsewhere import thing as dataclass\n\n@dataclass\nclass C:\n    x: int = 1\n",
                false
            )
            .is_empty(),
            "the name is bound to an import of something else, so it is not \
             the decorator"
        );
        assert!(
            messages(
                "from elsewhere import helper as dc\n\n@dc\nclass C:\n    x: int = 1\n",
                false
            )
            .is_empty(),
            "only `dataclasses` and `pydantic` imports rename anything"
        );
    }

    #[test]
    fn an_alias_imported_under_type_checking_is_collected() {
        assert_eq!(
            messages(
                "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from dataclasses import dataclass as dc\n\n@dc\nclass C:\n    x: int = 1\n",
                false
            ),
            ["dataclass field `x` has a default"]
        );
    }

    #[test]
    fn a_base_class_is_recognised_however_it_is_named() {
        let found = messages(
            "class A(pydantic.BaseModel):\n x: int = 1\nclass B(BaseModel, Generic[T]):\n y: int = 2\n",
            false,
        );
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn a_configured_field_base_is_resolved_through_an_import_alias() {
        assert_eq!(
            messages(
                "from pydantic import BaseModel as Model\n\nclass C(Model):\n    value: int = 1\n",
                false,
            ),
            ["class field `value` has a default"]
        );
    }

    #[test]
    fn an_unconfigured_base_class_carries_no_fields() {
        let found = messages_with_bases("class Job(BaseModel):\n x: int = 1\n", &[]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_configured_base_class_carries_fields() {
        let found = messages_with_bases(
            "class Job(msgspec.Struct):\n x: int = 1\nclass Other(BaseModel):\n y: int = 2\n",
            &["msgspec.Struct"],
        );
        assert_eq!(found, ["class field `x` has a default"]);
    }

    #[test]
    fn an_ellipsis_marks_a_pydantic_field_required_rather_than_defaulted() {
        let found = messages(
            "class Job(BaseModel):\n x: int = Field(...)\n y: int = Field(..., description=\"d\")\n z: int = Field(default=...)\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// `dataclasses.field` has no such convention: its first argument is the
    /// default, whatever it is.
    #[test]
    fn an_ellipsis_is_still_a_dataclass_default() {
        let found = messages("@dataclass\nclass C:\n x: int = field(...)\n", false);
        assert_eq!(found, ["dataclass field `x` has a default"]);
    }

    #[test]
    fn dataclasses_missing_means_a_field_has_no_default() {
        let found = messages(
            "from dataclasses import MISSING, dataclass, field\nimport dataclasses as dc\n\n@dataclass\nclass C:\n    a: int = field(default=MISSING)\n    b: int = field(default_factory=dc.MISSING)\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn dataclasses_missing_assignment_means_a_required_field() {
        let found = messages(
            "from dataclasses import MISSING as required, dataclass\nimport dataclasses\n\n@dataclass\nclass C:\n    a: int = required\n    b: int = dataclasses.MISSING\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn unrelated_missing_assignments_are_still_defaults() {
        let found = messages(
            "from dataclasses import dataclass\nimport elsewhere\nMISSING = object()\n\n@dataclass\nclass C:\n    a: int = MISSING\n    b: int = elsewhere.MISSING\n",
            false,
        );
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn unrelated_missing_names_are_still_defaults() {
        let found = messages(
            "from dataclasses import dataclass, field\nimport elsewhere\nMISSING = object()\n\n@dataclass\nclass C:\n    a: int = field(default=MISSING)\n    b: int = field(default=elsewhere.MISSING)\n",
            false,
        );
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_pydantic_validation_alias_prevents_an_unsafe_call_rewrite() {
        let source = "from pydantic import BaseModel, Field\n\nclass C(BaseModel):\n    value: int = Field(1, alias=\"external\")\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_pydantic_field_keeps_its_metadata_when_fixed() -> Result<(), String> {
        let found = fixed(
            "class Job(BaseModel):\n x: list = Field(default_factory=list, description=\"d\")\n y: int = Field(2, gt=0)\n",
        )?;
        assert_eq!(
            found,
            "class Job(BaseModel):\n x: list = Field(description=\"d\")\n y: int = Field(gt=0)\n"
        );
        Ok(())
    }

    #[test]
    fn a_qualified_pydantic_field_keeps_its_metadata_when_fixed() -> Result<(), String> {
        let found = fixed(
            "import pydantic as p\n\nclass Job(p.BaseModel):\n value: int = p.Field(default=1, description=\"kept\")\n",
        )?;
        assert_eq!(
            found,
            "import pydantic as p\n\nclass Job(p.BaseModel):\n value: int = p.Field(description=\"kept\")\n"
        );
        Ok(())
    }

    #[test]
    fn a_pydantic_v1_field_keeps_its_metadata_when_fixed() -> Result<(), String> {
        let found = fixed(
            "from pydantic.v1 import BaseModel, Field\n\nclass Job(BaseModel):\n value: int = Field(default=1, description=\"kept\")\n",
        )?;
        assert_eq!(
            found,
            "from pydantic.v1 import BaseModel, Field\n\nclass Job(BaseModel):\n value: int = Field(description=\"kept\")\n"
        );
        Ok(())
    }

    #[test]
    fn class_level_assignments_that_declare_no_field_are_left_alone() {
        let found = messages(
            "from typing import ClassVar\n\nclass Job(BaseModel):\n model_config = ConfigDict(frozen=True)\n kind: ClassVar[str] = \"job\"\n def build(self) -> None:\n  seen: set[str] = set()\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// A model whose base is another model takes fields this file's class body
    /// does not list, so it is not treated as carrying fields at all.
    #[test]
    fn a_model_subclass_is_left_alone() {
        let found = messages("class Sub(Job):\n x: int = 1\n", false);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_known_local_model_subclass_carries_fields() {
        assert_eq!(
            messages(
                "from pydantic import BaseModel\n\nclass Parent(BaseModel):\n    first: int = 1\n\nclass Child(Parent):\n    second: int = 2\n",
                false,
            ),
            [
                "class field `first` has a default",
                "class field `second` has a default",
            ]
        );
    }

    #[test]
    fn a_model_call_site_gains_the_removed_default() -> Result<(), String> {
        let found = fixed(
            "class Job(BaseModel):\n name: str\n retries: int = 3\n\njob = Job(name=\"a\")\n",
        )?;
        assert!(found.contains("Job(name=\"a\", retries=3)"), "{found}");
        Ok(())
    }

    /// Any base beyond the one that made the class carry fields may declare
    /// fields of its own, so the constructor is not known from this file.
    #[test]
    fn a_model_with_another_base_has_its_calls_left_alone() -> Result<(), String> {
        let found =
            skipped_reasons("class Job(Mixin, BaseModel):\n retries: int = 3\n\njob = Job()\n")?;
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("inherits fields"), "{found:?}");
        Ok(())
    }

    #[test]
    fn a_directive_on_a_model_header_covers_every_field() {
        let found = codes("class Job(BaseModel):  # noqa: NOD001\n x: int = 1\n y: int = 2\n");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn quoted_class_var_annotations_are_not_fields() {
        let found = messages(
            "from typing import ClassVar\nimport typing\n\n@dataclass\nclass C:\n a: \"ClassVar[int]\" = 1\n b: \"typing.ClassVar[int]\" = 2\n c: 'ClassVar' = 3\n d: \"  ClassVar[int]  \" = 4\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn aliased_class_var_annotations_are_not_fields() {
        let found = messages(
            "from dataclasses import dataclass\nfrom typing import ClassVar as CV\nfrom typing_extensions import ClassVar as ExtendedCV\n\n@dataclass\nclass C:\n    a: CV[int] = 1\n    b: ExtendedCV[str] = 'b'\n    c: \"CV[float]\" = 3.0\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_user_defined_class_var_name_is_an_ordinary_field() {
        let found = messages(
            "from dataclasses import dataclass\n\nclass ClassVar:\n    def __class_getitem__(cls, item): return cls\n\n@dataclass\nclass C:\n    value: ClassVar[int] = 1\n",
            false,
        );
        assert_eq!(found, ["dataclass field `value` has a default"]);
    }

    #[test]
    fn a_user_defined_kw_only_name_is_an_ordinary_field() -> Result<(), String> {
        let source = "class KW_ONLY: pass\n\n@dataclass\nclass C:\n    marker: KW_ONLY\n    value: int = 1\n\nC(5, 6)\n";
        assert_eq!(
            fixed(source)?,
            "class KW_ONLY: pass\n\n@dataclass\nclass C:\n    marker: KW_ONLY\n    value: int\n\nC(5, 6)\n"
        );
        Ok(())
    }

    #[test]
    fn quoted_non_class_var_annotations_are_still_fields() {
        let found = messages(
            "@dataclass\nclass C:\n a: \"int\" = 1\n b: \"NotClassVar[int]\" = 2\n c: \"((\" = 3\n",
            false,
        );
        assert_eq!(found.len(), 3, "{found:?}");
    }

    #[test]
    fn ignores_annotated_locals_in_dataclass_methods() {
        let found = messages(
            "@dataclass\nclass C:\n x: int = 1\n def _validate(self) -> None:\n  seen: set[str] = set()\n  def inner() -> None:\n   nested: int = 0\n",
            false,
        );
        assert_eq!(found, ["dataclass field `x` has a default"]);
    }

    #[test]
    fn ignores_annotated_locals_in_async_dataclass_methods() {
        let found = messages(
            "@dataclass\nclass C:\n async def _fetch(self) -> None:\n  chunks: list[str] = []\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn detects_fields_of_dataclass_nested_in_a_method() {
        let found = messages(
            "@dataclass\nclass C:\n def build(self) -> None:\n  local: int = 0\n  @dataclass\n  class Inner:\n   y: int = 2\n",
            false,
        );
        assert_eq!(found, ["dataclass field `y` has a default"]);
    }

    #[test]
    fn resumes_field_detection_after_a_method() {
        let found = messages(
            "@dataclass\nclass C:\n def _validate(self) -> None:\n  local: int = 0\n after: int = 3\n",
            false,
        );
        assert_eq!(found, ["dataclass field `after` has a default"]);
    }

    #[test]
    fn private_only_checks_private_symbols() {
        let found = messages(
            "def public(x=1): pass\ndef _private(x=1): pass\n@dataclass\nclass C:\n public: int = 1\n _private: int = 2\n",
            true,
        );
        assert_eq!(found.len(), 2);
    }

    /// The violations found in a private module whose package re-exports the
    /// given names.
    fn private_module_messages(source: &str, reexported: &[&str]) -> Vec<String> {
        let reexports = Reexports {
            wildcard: false,
            module: false,
            names: reexported.iter().map(|name| (*name).to_owned()).collect(),
        };
        check_source(
            Path::new("package/_upload.py"),
            source,
            true,
            Path::new(""),
            &reexports,
            &default_bases(),
            false,
        )
        .diagnostics
        .into_iter()
        .map(|item| item.message)
        .collect()
    }

    #[test]
    fn a_reexported_name_is_public_however_private_its_module_is() {
        let source = "def upload(timeout=30): pass\ndef helper(x=1): pass\n";
        let found = private_module_messages(source, &["upload"]);
        assert_eq!(found, ["parameter `x` of function `helper` has a default"]);
    }

    #[test]
    fn a_reexported_class_carries_its_members_into_the_public_api() {
        let source =
            "class _Client:\n def fetch(self, retries=3): pass\n def _retry(self, x=1): pass\n";
        let found = private_module_messages(source, &["_Client"]);
        assert_eq!(
            found,
            ["parameter `x` of function `_retry` has a default"],
            "a private method of a public class is still private"
        );
        assert_eq!(private_module_messages(source, &[]).len(), 2);
    }

    #[test]
    fn a_reexported_dataclass_keeps_its_field_defaults() {
        let source = "@dataclass\nclass Job:\n retries: int = 3\n";
        assert!(private_module_messages(source, &["Job"]).is_empty());
        assert_eq!(private_module_messages(source, &[]).len(), 1);
    }

    #[test]
    fn a_module_reexported_under_its_own_name_is_public() {
        // `from . import _upload` makes `package._upload.upload` reachable.
        let source = "def upload(timeout=30): pass\ndef _helper(x=1): pass\n";
        let found = private_module_messages(source, &["_upload"]);
        assert_eq!(
            found,
            ["parameter `x` of function `_helper` has a default"],
            "a private name in a reachable module is still private"
        );
        assert_eq!(private_module_messages(source, &[]).len(), 2);
    }

    #[test]
    fn a_directive_on_a_reexported_signature_becomes_unused() {
        let reexports = Reexports {
            wildcard: false,
            module: false,
            names: BTreeSet::from(["upload".to_owned()]),
        };
        let found = check_source(
            Path::new("package/_upload.py"),
            "def upload(timeout=30): pass  # noqa: NOD001\n",
            true,
            Path::new(""),
            &reexports,
            &default_bases(),
            false,
        );
        let codes: Vec<&str> = found
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        assert_eq!(codes, ["NOD002"]);
    }

    #[test]
    fn a_wildcard_reexport_makes_every_name_public() {
        let reexports = Reexports {
            wildcard: true,
            module: false,
            names: BTreeSet::new(),
        };
        let found = check_source(
            Path::new("package/_upload.py"),
            "def _helper(x=1): pass\n",
            true,
            Path::new(""),
            &reexports,
            &default_bases(),
            false,
        );
        assert!(found.diagnostics.is_empty());
    }

    #[test]
    fn reexports_are_read_from_public_package_initializers() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("package");
        let internal = root.join("_internal");
        std::fs::create_dir_all(&internal).map_err(|error| error.to_string())?;
        std::fs::write(
            root.join("__init__.py"),
            "import os\nfrom . import _internal as internal\nfrom ._internal import upload as send\nJob = object()\n__all__ = [\"send\", \"Job\"]\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            internal.join("__init__.py"),
            "from ._upload import buried\n",
        )
        .map_err(|error| error.to_string())?;
        let target = internal.join("_upload.py");
        let reexports =
            package_reexports(&internal, &target, directory.path(), &mut BTreeMap::new())?.names;
        assert!(reexports.covers("buried"));
        assert!(reexports.module);
        Ok(())
    }

    #[test]
    fn a_public_alias_covers_the_source_symbol() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(&path, "from ._api import _hidden as public\n")
            .map_err(|error| error.to_string())?;
        let mut reexports = Reexports::default();
        collect_reexports(&path, &mut reexports)?;
        assert!(reexports.covers("public"));
        assert!(reexports.covers("_hidden"));
        let mut targeted = Reexports::default();
        collect_reexports_for_target(
            &path,
            &directory.path().join("_api.py"),
            directory.path(),
            &mut targeted,
        )?;
        assert!(targeted.covers("_hidden"));
        Ok(())
    }

    #[test]
    fn deleting_an_imported_name_removes_its_reexport() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(&path, "from ._api import public\ndel public\n")
            .map_err(|error| error.to_string())?;
        let mut reexports = Reexports::default();
        collect_reexports_for_target(
            &path,
            &directory.path().join("_api.py"),
            directory.path(),
            &mut reexports,
        )?;
        assert!(!reexports.covers("public"));
        Ok(())
    }

    #[test]
    fn a_public_module_alias_exposes_its_contents() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(&path, "from . import _api as public\n")
            .map_err(|error| error.to_string())?;
        let mut reexports = Reexports::default();
        collect_reexports(&path, &mut reexports)?;
        assert!(reexports.wildcard);
        assert!(reexports.covers("anything_in_the_module"));
        std::fs::create_dir(directory.path().join("_api")).map_err(|error| error.to_string())?;
        let mut targeted = Reexports::default();
        collect_reexports_for_target(
            &path,
            &directory.path().join("_api/member.py"),
            directory.path(),
            &mut targeted,
        )?;
        assert!(targeted.module);
        Ok(())
    }

    #[test]
    fn reexports_are_tied_to_their_source_module() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        let sub = package.join("sub");
        std::fs::create_dir_all(&sub).map_err(|error| error.to_string())?;
        std::fs::write(package.join("__init__.py"), "from .other import upload\n")
            .map_err(|error| error.to_string())?;
        let target = sub.join("_api.py");
        let reexports =
            package_reexports(&sub, &target, directory.path(), &mut BTreeMap::new())?.names;
        assert!(!reexports.covers("upload"));
        Ok(())
    }

    #[test]
    fn initializer_function_imports_are_not_reexports() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "def load():\n    from ._api import public\n    __all__ = ['also_local']\n",
        )
        .map_err(|error| error.to_string())?;
        let mut reexports = Reexports::default();
        collect_reexports(&path, &mut reexports)?;
        assert!(reexports.names.is_empty());
        assert!(!reexports.wildcard);
        Ok(())
    }

    #[test]
    fn initializer_class_imports_are_not_reexports() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "class Loader:\n    from ._api import public\n    __all__ = ['class_attribute']\n",
        )
        .map_err(|error| error.to_string())?;
        let mut reexports = Reexports::default();
        collect_reexports(&path, &mut reexports)?;
        assert!(reexports.names.is_empty());
        assert!(!reexports.wildcard);
        Ok(())
    }

    #[test]
    fn reassigned_dunder_all_replaces_stale_names() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "old = object()\nnew = object()\n__all__ = ['old']\n__all__ = ['new']\n__all__ += ['also_unbound']\n",
        )
        .map_err(|error| error.to_string())?;
        let mut reexports = Reexports::default();
        collect_reexports(&path, &mut reexports)?;
        assert!(!reexports.covers("old"));
        assert!(reexports.covers("new"));
        assert!(!reexports.covers("also_unbound"));
        Ok(())
    }

    #[test]
    fn annotation_only_dunder_all_does_not_abort_export_scanning() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "__all__ = ['old']\n__all__: list[str]\n__all__ = ['new']\n",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            explicit_all_names(&path),
            Some(BTreeSet::from(["new".to_owned()]))
        );
        Ok(())
    }

    #[test]
    fn explicit_dunder_all_follows_module_control_flow() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "if False:\n    __all__ = ['unreachable']\nelif True:\n    __all__ = ['if_name']\nfor _ in [0]:\n    __all__ += ['loop_name']\nwith manager():\n    __all__ += ['with_name']\n",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            explicit_all_names(&path),
            Some(BTreeSet::from([
                "if_name".to_owned(),
                "loop_name".to_owned(),
                "with_name".to_owned(),
            ]))
        );
        Ok(())
    }

    #[test]
    fn ambiguous_dunder_all_branches_are_not_assumed() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "if condition:\n    __all__ = ['left']\nelse:\n    __all__ = ['right']\n",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(explicit_all_names(&path), None);
        Ok(())
    }

    #[test]
    fn always_true_while_does_not_apply_its_else_exports() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "__all__ = ['before']\nwhile True:\n    __all__ = ['body']\n    break\nelse:\n    __all__ = ['else']\n",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            explicit_all_names(&path),
            Some(BTreeSet::from(["body".to_owned()]))
        );
        Ok(())
    }

    #[test]
    fn exhaustive_match_has_no_fallthrough_export_state() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("__init__.py");
        std::fs::write(
            &path,
            "__all__ = ['old']\nmatch value:\n    case 1:\n        __all__ = ['new']\n    case _:\n        __all__ = ['new']\n",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            explicit_all_names(&path),
            Some(BTreeSet::from(["new".to_owned()]))
        );
        Ok(())
    }

    #[test]
    fn a_star_import_in_an_initializer_sets_the_wildcard() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("package");
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        std::fs::write(root.join("__init__.py"), "from ._upload import *\n")
            .map_err(|error| error.to_string())?;
        assert!(
            package_reexports(
                &root,
                &root.join("_upload.py"),
                directory.path(),
                &mut BTreeMap::new(),
            )?
            .names
            .wildcard
        );
        Ok(())
    }

    #[test]
    fn a_reexported_private_package_is_read_after_all() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("package");
        let internal = root.join("_internal");
        let deeper = internal.join("deeper");
        std::fs::create_dir_all(&deeper).map_err(|error| error.to_string())?;
        std::fs::write(
            root.join("__init__.py"),
            "from . import _internal as internal\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            internal.join("__init__.py"),
            "from ._upload import reachable\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(deeper.join("__init__.py"), "from ._mod import deep\n")
            .map_err(|error| error.to_string())?;
        let mut cache = BTreeMap::new();
        let reexports = package_reexports(
            &deeper,
            &deeper.join("_mod.py"),
            directory.path(),
            &mut cache,
        )?
        .names;
        assert!(reexports.covers("deep"));
        // Each directory in the chain answered once, for the ancestors as well
        // as for the directory that asked: the three packages and the root the
        // walk stopped at.
        assert_eq!(cache.len(), 4);
        Ok(())
    }

    #[test]
    fn a_namespace_package_does_not_break_the_chain() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("package");
        // `data` has no `__init__.py`, but `package.data._mod` is imported
        // through it all the same.
        let data = root.join("data");
        std::fs::create_dir_all(&data).map_err(|error| error.to_string())?;
        std::fs::write(root.join("__init__.py"), "from .data._mod import upload\n")
            .map_err(|error| error.to_string())?;
        let reexports = package_reexports(
            &data,
            &data.join("_mod.py"),
            directory.path(),
            &mut BTreeMap::new(),
        )?
        .names;
        assert!(reexports.covers("upload"));
        Ok(())
    }

    #[test]
    fn the_walk_stops_at_the_project_root() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("project");
        let package = root.join("package");
        std::fs::create_dir_all(&package).map_err(|error| error.to_string())?;
        // An `__init__.py` above the root belongs to no package this run knows.
        std::fs::write(
            directory.path().join("__init__.py"),
            "from ._outer import far\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(package.join("__init__.py"), "from ._upload import near\n")
            .map_err(|error| error.to_string())?;
        let reexports = package_reexports(
            &package,
            &package.join("_upload.py"),
            &root,
            &mut BTreeMap::new(),
        )?
        .names;
        assert!(reexports.covers("near"));
        assert!(!reexports.covers("far"));
        Ok(())
    }

    #[test]
    fn a_private_package_seals_the_public_ones_inside_it() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("package");
        let internal = root.join("_internal");
        let deeper = internal.join("deeper");
        std::fs::create_dir_all(&deeper).map_err(|error| error.to_string())?;
        std::fs::write(root.join("__init__.py"), "from ._internal import shown\n")
            .map_err(|error| error.to_string())?;
        std::fs::write(internal.join("__init__.py"), "").map_err(|error| error.to_string())?;
        std::fs::write(deeper.join("__init__.py"), "from ._mod import buried\n")
            .map_err(|error| error.to_string())?;
        let package = package_reexports(
            &deeper,
            &deeper.join("_mod.py"),
            directory.path(),
            &mut BTreeMap::new(),
        )?;
        assert!(package.sealed);
        assert!(
            !package.names.covers("buried"),
            "a public package inside a private one is still out of reach"
        );
        Ok(())
    }

    #[test]
    fn a_stub_only_package_is_read_from_its_pyi_initializer() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("package");
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        std::fs::write(
            root.join("__init__.pyi"),
            "from . import _upload\nfrom ._upload import upload\n",
        )
        .map_err(|error| error.to_string())?;
        let reexports = package_reexports(
            &root,
            &root.join("_upload.pyi"),
            directory.path(),
            &mut BTreeMap::new(),
        )?
        .names;
        assert!(reexports.covers("upload"));
        assert!(
            !is_private_module(Path::new("package/_upload.pyi"), Path::new(""), &reexports),
            "the initializer imports the module itself as well as one symbol"
        );
        let other = package_reexports(
            &root,
            &root.join("_other.pyi"),
            directory.path(),
            &mut BTreeMap::new(),
        )?
        .names;
        assert!(is_private_module(
            Path::new("package/_other.pyi"),
            Path::new(""),
            &other
        ));
        Ok(())
    }

    #[test]
    fn a_file_outside_a_package_has_no_reexports() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let reexports = package_reexports(
            directory.path(),
            &directory.path().join("file.py"),
            directory.path(),
            &mut BTreeMap::new(),
        )?
        .names;
        assert!(reexports.names.is_empty());
        assert!(!reexports.wildcard);
        Ok(())
    }

    #[test]
    fn noqa_suppresses_blanket_and_selected_violations() {
        let found = messages(
            "def a(x=1): pass  # noqa\ndef b(x=1): pass  # noqa: E501, NOD001\ndef c(x=1): pass  # noqa: E501\n",
            false,
        );
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("function `c`"));
    }

    #[test]
    fn the_word_noqa_in_a_note_is_not_a_directive() {
        // The coded directive names another rule, so it does not apply here.
        // The word in the note after it must not blanket this one.
        let found = messages(
            "def a(x=1): pass  # noqa: E501  keep noqa
def b(x=1): pass  # type: ignore  # noqa
",
            false,
        );
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("function `a`"));
    }

    #[test]
    fn blanket_noqa_accepts_a_following_explanation() -> Result<(), String> {
        let source = "def target(value=1): pass  # noqa  # compatibility\n";
        assert!(messages(source, false).is_empty());
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn file_level_noqa_accepts_a_following_explanation() -> Result<(), String> {
        let source = "# ruff: noqa  # generated file\ndef target(value=1): pass\n";
        assert!(messages(source, false).is_empty());
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn recognizes_private_modules_and_packages() {
        let private =
            |path: &str| is_private_module(Path::new(path), Path::new(""), &Reexports::default());
        assert!(private("src/_module.py"));
        assert!(private("src/_package/module.py"));
        assert!(private("src/_package/__init__.py"));
        assert!(!private("src/package/__init__.py"));
        assert!(!private("src/package/module.py"));
    }

    #[test]
    fn directories_above_the_project_root_are_not_module_names() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        // A checkout under a directory whose name starts with an underscore.
        let root = directory.path().join("_work").join("proj");
        std::fs::create_dir_all(root.join("package")).map_err(|error| error.to_string())?;
        let root = root.canonicalize().map_err(|error| error.to_string())?;
        let public = root.join("mod.py");
        let private = root.join("package").join("_helper.py");
        for path in [&public, &private] {
            std::fs::write(path, "def f(x=1): pass\n").map_err(|error| error.to_string())?;
        }
        assert!(
            !is_private_module(&public, &root, &Reexports::default()),
            "`_work` is not a package, so it says nothing about `mod`"
        );
        assert!(
            is_private_module(&private, &root, &Reexports::default()),
            "a private module below the root is still private"
        );
        Ok(())
    }

    #[test]
    fn privacy_does_not_depend_on_how_the_path_was_written() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory.path().join("_work");
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let root = root.canonicalize().map_err(|error| error.to_string())?;
        std::fs::write(root.join("mod.py"), "def f(x=1): pass\n")
            .map_err(|error| error.to_string())?;
        let absolute = root.join("mod.py");
        let indirect = root.join(".").join("mod.py");
        assert_eq!(
            is_private_module(&absolute, &root, &Reexports::default()),
            is_private_module(&indirect, &root, &Reexports::default()),
        );
        assert!(!is_private_module(&absolute, &root, &Reexports::default()));
        Ok(())
    }

    fn loaded_config(
        private_only: bool,
        patterns: &[(&str, Enforcement)],
    ) -> Result<LoadedConfig, String> {
        let config = Config {
            private_only,
            per_file_enforcement: patterns
                .iter()
                .map(|(pattern, enforcement)| ((*pattern).to_owned(), *enforcement))
                .collect(),
            ..Config::default()
        };
        Ok(LoadedConfig {
            root: PathBuf::from("project"),
            field_bases: Arc::new(default_bases()),
            overrides: Arc::new(compile_overrides(&config.per_file_enforcement)?),
            config,
        })
    }

    #[test]
    fn per_file_enforcement_supports_tests_all_src_private() -> Result<(), String> {
        let loaded = loaded_config(
            false,
            &[
                ("src/**", Enforcement::Private),
                ("tests/**", Enforcement::All),
            ],
        )?;
        assert_eq!(
            private_only_for(Path::new("project/tests/test_api.py"), &loaded),
            Some(false)
        );
        assert_eq!(
            private_only_for(Path::new("project/src/package/api.py"), &loaded),
            Some(true)
        );
        assert_eq!(
            private_only_for(Path::new("project/scripts/release.py"), &loaded),
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn none_enforcement_exempts_matching_files() -> Result<(), String> {
        let loaded = loaded_config(true, &[("src/package/_compat.py", Enforcement::None)])?;
        assert_eq!(
            private_only_for(Path::new("project/src/package/_compat.py"), &loaded),
            None
        );
        // Files the pattern does not name keep the project-wide setting.
        assert_eq!(
            private_only_for(Path::new("project/src/package/api.py"), &loaded),
            Some(true)
        );
        Ok(())
    }

    #[test]
    fn none_enforcement_survives_the_private_only_flag() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.no_defaults]\nper_file_enforcement.\"exempt.py\" = \"none\"\n",
        )
        .map_err(|error| error.to_string())?;
        let exempt = root.join("exempt.py");
        std::fs::write(&exempt, "def f(x=1): pass\n").map_err(|error| error.to_string())?;
        let settings = settings_for_files(&[exempt], true, false)?;
        assert_eq!(settings[0].private_only, None);
        Ok(())
    }

    #[test]
    fn fixes_function_and_dataclass_defaults_safely() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(
            &path,
            "from dataclasses import dataclass, field\n\ndef f(a: int = 1, *, b=2): pass\n\n@dataclass\nclass C:\n    a: int = 1\n    b: int = field(default=2, repr=False)\n    c: int = field(kw_only=True, default_factory=int)\n    not_a_field = 3\n",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            check_file(
                &path,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                false,
            )
            .diagnostics
            .len(),
            5
        );
        fix_all(std::slice::from_ref(&path))?;
        let fixed = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        assert!(fixed.contains("def f(a: int, *, b): pass"));
        assert!(fixed.contains("a: int"));
        assert!(fixed.contains("b: int = field(repr=False)"));
        assert!(fixed.contains("c: int = field(kw_only=True)"));
        assert!(fixed.contains("not_a_field = 3"));
        Ok(())
    }

    #[test]
    fn multiline_fixes_are_idempotent_and_keep_comments() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(
            &path,
            "def f(\n    value: int = 1,  # useful comment\n    *,\n    flag=True,\n):\n    pass\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(std::slice::from_ref(&path))?;
        let fixed = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        assert!(fixed.contains("value: int,  # useful comment"));
        assert!(fixed.contains("flag,"));
        Ok(())
    }

    #[test]
    fn file_level_noqa_suppresses_rule() {
        assert!(codes("# ruff: noqa: NOD001\ndef f(x=1): pass\n").is_empty());
        assert_eq!(codes("# ruff: noqa: E501\ndef f(x=1): pass\n"), ["NOD001"]);
        assert!(codes("# ruff: noqa\ndef f(x=1): pass\n").is_empty());
        assert!(codes("# flake8: noqa\ndef f(x=1): pass\n").is_empty());
        assert!(codes("# flake8: noqa   \ndef f(x=1): pass\n").is_empty());
    }

    #[test]
    fn a_directive_may_follow_another_pragma_on_the_same_line() {
        assert!(
            codes("def a(x=1): pass  # type: ignore[misc]  # noqa: NOD001\n").is_empty(),
            "`# type: ignore` has to come first for some mypy versions"
        );
        assert!(codes("def a(x=1): pass  # pragma: no cover  # noqa\n").is_empty());
        assert!(
            codes("def a(x=1): pass  # explains why  # NOQA: NOD001\n").is_empty(),
            "the marker is matched case-insensitively wherever it sits"
        );
        assert_eq!(
            codes("def a(x=1): pass  # type: ignore  # noqa: E501\n"),
            ["NOD001"],
            "a code list that omits this rule still suppresses nothing"
        );
        assert!(
            codes("def a(x=1): pass  # noqa: E501  # noqa: NOD001\n").is_empty(),
            "a later noqa segment can select this rule"
        );
        assert_eq!(
            codes("def a(x=1): pass  # see noqattention\n"),
            ["NOD001"],
            "`noqa` must still be a word of its own wherever it sits"
        );
    }

    #[test]
    fn a_directive_inside_a_default_does_not_derail_the_fix() -> Result<(), String> {
        // Removing the default already removes the directive inside it, so the
        // two deletions overlap. Applying both in turn used to panic here.
        assert_eq!(
            fixed("def f(\n    x=[\n        1,  # noqa: NOD001\n    ],\n):\n    pass\n")?,
            "def f(\n    x,\n):\n    pass\n"
        );
        // With a shorter inner deletion the outer one stayed in bounds and
        // silently took the following parameter with it.
        assert_eq!(
            fixed(
                "def f(\n    x=[\n        1,  # noqa: NOD001, E501\n    ],\n    y=2,\n):\n    pass\n"
            )?,
            "def f(\n    x,\n    y,\n):\n    pass\n"
        );
        Ok(())
    }

    #[test]
    fn merging_deletions_covers_what_each_of_them_covered() {
        let range = |start: u32, end: u32| TextRange::new(TextSize::new(start), TextSize::new(end));
        assert_eq!(
            merge_deletions(vec![range(0, 10), range(3, 5)]),
            [range(0, 10)],
            "a contained deletion adds nothing"
        );
        assert_eq!(
            merge_deletions(vec![range(3, 5), range(0, 10)]),
            [range(0, 10)],
            "the order they arrive in does not matter"
        );
        assert_eq!(
            merge_deletions(vec![range(0, 5), range(3, 9)]),
            [range(0, 9)],
            "a partial overlap becomes the span both asked for"
        );
        assert_eq!(
            merge_deletions(vec![range(0, 5), range(5, 9)]),
            [range(0, 9)],
            "touching deletions are one contiguous span"
        );
        assert_eq!(
            merge_deletions(vec![range(0, 4), range(6, 9)]),
            [range(0, 4), range(6, 9)],
            "disjoint deletions stay apart"
        );
        assert_eq!(merge_deletions(vec![]), []);
    }

    #[test]
    fn removing_a_directive_keeps_a_pragma_that_follows_it() -> Result<(), String> {
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001  # type: ignore[misc]\n")?,
            "def b(y): pass  # type: ignore[misc]\n"
        );
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001  # pragma: no cover\n")?,
            "def b(y): pass  # pragma: no cover\n"
        );
        assert_eq!(
            fixed("# noqa: NOD001  # type: ignore\ndef b(y): pass\n")?,
            "# type: ignore\ndef b(y): pass\n",
            "the line survives because something on it is not the directive"
        );
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001, E501  # type: ignore\n")?,
            "def b(y): pass  # noqa: E501  # type: ignore\n",
            "dropping one code from a list never reached the rest of the line"
        );
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001\n")?,
            "def b(y): pass\n",
            "a directive with nothing after it still takes its whitespace"
        );
        assert_eq!(
            fixed("# noqa: NOD001\ndef b(y): pass\n")?,
            "def b(y): pass\n",
            "a directive alone on its line still takes the line"
        );
        Ok(())
    }

    #[test]
    fn removing_a_directive_that_follows_a_pragma_keeps_the_pragma() -> Result<(), String> {
        assert_eq!(
            fixed("def c(z): pass  # type: ignore[misc]  # noqa: NOD001\n")?,
            "def c(z): pass  # type: ignore[misc]\n"
        );
        assert_eq!(
            fixed("# type: ignore  # noqa: NOD001\ndef c(z): pass\n")?,
            "# type: ignore\ndef c(z): pass\n",
            "a comment-only line keeps the part that is not the directive"
        );
        Ok(())
    }

    #[test]
    fn a_file_level_directive_needs_no_space_after_the_colon() {
        assert!(codes("# ruff:noqa: NOD001\ndef f(x=1): pass\n").is_empty());
        assert!(codes("# ruff:noqa\ndef f(x=1): pass\n").is_empty());
        assert!(codes("# flake8:noqa\ndef f(x=1): pass\n").is_empty());
        assert!(codes("# ruff:\tnoqa\ndef f(x=1): pass\n").is_empty());
        assert!(codes("# RUFF:NOQA: NOD001\ndef f(x=1): pass\n").is_empty());
        assert_eq!(
            codes("# ruff:noqa: E501\ndef f(x=1): pass\n"),
            ["NOD001"],
            "a code list that omits this rule still suppresses nothing"
        );
        assert_eq!(
            codes("# flake8:noqa: NOD001\ndef f(x=1): pass\n"),
            ["NOD001"],
            "as with the spaced form, a `flake8: noqa` with codes appended is not a directive"
        );
        assert_eq!(
            codes("# ruffnoqa\ndef f(x=1): pass\n"),
            ["NOD001"],
            "the colon is what makes it a file-level directive"
        );
    }

    #[test]
    fn a_directive_on_the_def_line_covers_the_whole_signature() {
        assert!(
            codes("def f(  # noqa: NOD001\n    a=1,\n    b=2,\n):\n    pass\n").is_empty(),
            "one directive covers every parameter of the signature"
        );
        assert!(
            codes("@decorator\nasync def f(  # noqa: NOD001\n    a=1,\n) -> None:\n    pass\n")
                .is_empty(),
            "decorators do not move the `def` line"
        );
        assert!(
            codes("def f(  # noqa\n    a=1,\n):\n    pass\n").is_empty(),
            "a blanket directive on the `def` line covers the signature too"
        );
        assert_eq!(
            codes("def f(  # noqa: E501\n    a=1,\n):\n    pass\n"),
            ["NOD001"],
            "directives for other rules suppress nothing"
        );
    }

    #[test]
    fn a_directive_on_the_def_line_stops_at_the_signature() {
        assert_eq!(
            codes("def f(  # noqa: NOD001\n    a,\n):\n    def inner(b=1): pass\n"),
            ["NOD002", "NOD001"],
            "a nested function keeps its own violations and leaves the directive unused"
        );
        assert_eq!(
            codes("def f(\n    a=1,\n):  # noqa: NOD001\n    pass\n"),
            ["NOD001", "NOD002"],
            "only the `def` line carries a signature-wide directive"
        );
    }

    #[test]
    fn a_signature_directive_leaves_defaults_in_place() -> Result<(), String> {
        let source = "def f(  # noqa: NOD001\n    a=1,\n):\n    pass\n";
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn a_directive_on_the_class_line_covers_every_field() {
        assert!(
            codes(
                "@dataclass\nclass Job:  # noqa: NOD001\n    retries: int = 3\n    tags: list[str] = field(default_factory=list)\n"
            )
            .is_empty(),
            "one directive covers every field of the dataclass"
        );
        assert_eq!(
            codes("@dataclass\nclass Job:  # noqa: NOD001\n    name: str\n"),
            ["NOD002"],
            "a class directive that suppresses nothing is unused"
        );
    }

    #[test]
    fn a_directive_on_the_class_line_stops_at_the_fields() {
        assert_eq!(
            codes(
                "@dataclass\nclass Job:  # noqa: NOD001\n    retries: int = 3\n\n    def run(self, timeout=30): pass\n"
            ),
            ["NOD001"],
            "a method keeps the violations of its own signature"
        );
        assert_eq!(
            codes(
                "@dataclass\nclass Outer:  # noqa: NOD001\n    @dataclass\n    class Inner:\n        retries: int = 3\n"
            ),
            ["NOD002", "NOD001"],
            "a nested dataclass needs its own directive"
        );
    }

    #[test]
    fn a_class_directive_leaves_defaults_in_place() -> Result<(), String> {
        let source = "@dataclass\nclass Job:  # noqa: NOD001\n    retries: int = 3\n";
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn unused_directives_are_reported() {
        assert_eq!(
            codes("def f(x): pass  # noqa: NOD001\n"),
            ["NOD002"],
            "an inline directive that suppresses nothing is unused"
        );
        assert!(
            codes("def f(x): pass  # noqa\n").is_empty(),
            "a blanket directive may serve another linter"
        );
        assert!(
            codes("def f(x): pass  # noqa: E501\n").is_empty(),
            "directives for other rules are not this linter's business"
        );
        assert_eq!(
            codes("# ruff: noqa: NOD001\ndef f(x): pass\n"),
            ["NOD002"],
            "a file-level directive that suppresses nothing is unused"
        );
        assert!(
            codes("# ruff: noqa\ndef f(x): pass  # noqa: NOD001\n").is_empty(),
            "a blanket file-level directive silences every rule"
        );
    }

    #[test]
    fn a_file_level_directive_claims_the_inline_directives_it_covers() {
        assert!(
            codes("# ruff: noqa: NOD001\ndef f(x=1): pass  # noqa: NOD001\n").is_empty(),
            "both directives cover the same violation"
        );
        assert_eq!(
            codes("# ruff: noqa: NOD001\ndef f(x=1): pass\ndef g(x): pass  # noqa: NOD001\n"),
            ["NOD002"],
            "only the inline directive covering nothing is unused"
        );
    }

    #[test]
    fn private_only_makes_directives_for_public_symbols_unused() {
        let found = check_source(
            Path::new("fixture.py"),
            "def public(x=1): pass  # noqa: NOD001\n",
            true,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            false,
        );
        assert_eq!(
            found
                .diagnostics
                .iter()
                .map(|item| item.code)
                .collect::<Vec<_>>()
                .as_slice(),
            ["NOD002"]
        );
    }

    #[test]
    fn text_that_only_looks_like_a_directive_is_ignored() {
        assert!(
            codes("example = \"# noqa: NOD001\"\n").is_empty(),
            "a `#` inside a string does not start a comment"
        );
        assert!(
            codes("def f(x): pass  # noqattention: NOD001\n").is_empty(),
            "`noqa` must be a word of its own"
        );
    }

    #[test]
    fn directives_survive_carriage_returns() -> Result<(), String> {
        assert!(codes("def f(x=1): pass  # noqa: NOD001\r\n").is_empty());
        assert!(codes("def f(x=1): pass  # noqa: NOD001\r").is_empty());
        assert_eq!(
            fixed("def f(x): pass  # noqa: NOD001\r\n")?,
            "def f(x): pass\r\n"
        );
        assert_eq!(
            fixed("def f(x): pass  # noqa: NOD001\r")?,
            "def f(x): pass\r"
        );
        assert_eq!(
            positions("def first(value=1): pass\rdef second(value=2): pass\r"),
            [(1, 17), (2, 18)]
        );
        Ok(())
    }

    #[test]
    fn unused_directives_are_removed_by_fix() -> Result<(), String> {
        assert_eq!(
            fixed("def f(x): pass  # noqa: NOD001\n")?,
            "def f(x): pass\n"
        );
        assert_eq!(
            fixed("def f(x): pass  # noqa: NOD001, E501\n")?,
            "def f(x): pass  # noqa: E501\n"
        );
        assert_eq!(
            fixed("def f(x): pass  # noqa: E501, NOD001\n")?,
            "def f(x): pass  # noqa: E501\n"
        );
        assert_eq!(
            fixed("# ruff: noqa: NOD001\ndef f(x): pass\n")?,
            "def f(x): pass\n"
        );
        assert_eq!(
            fixed("# ruff: noqa: NOD001, E501\ndef f(x): pass\n")?,
            "# ruff: noqa: E501\ndef f(x): pass\n"
        );
        assert_eq!(
            fixed("def f(x=1): pass  # noqa: E501\ndef g(x): pass  # noqa: NOD001\n")?,
            "def f(x): pass  # noqa: E501\ndef g(x): pass\n"
        );
        Ok(())
    }

    #[test]
    fn positional_only_defaults_are_appended_positionally() -> Result<(), String> {
        assert_eq!(
            fixed("def f(a, b=2, /): pass\nf(1)\nf(1, 9)\n")?,
            "def f(a, b, /): pass\nf(1, 2)\nf(1, 9)\n"
        );
        Ok(())
    }

    #[test]
    fn a_positional_only_argument_is_inserted_before_a_keyword() -> Result<(), String> {
        assert_eq!(
            fixed("def f(a=1, /, *, b=2): pass\nf(b=3)\n")?,
            "def f(a, /, *, b): pass\nf(1, b=3)\n"
        );
        Ok(())
    }

    #[test]
    fn call_sites_keep_trailing_commas_and_comments() -> Result<(), String> {
        assert_eq!(
            fixed("def f(a, b=2): pass\nf(\n    1,  # first\n)\n")?,
            "def f(a, b): pass\nf(\n    1, b=2,  # first\n)\n"
        );
        Ok(())
    }

    #[test]
    fn a_default_factory_becomes_a_fresh_value_at_each_call() -> Result<(), String> {
        assert_eq!(
            fixed("@dataclass\nclass C:\n    a: list = field(default_factory=list)\n    b: dict = field(default_factory=dict)\nC()\n")?,
            "@dataclass\nclass C:\n    a: list = field()\n    b: dict = field()\nC(a=[], b={})\n"
        );
        Ok(())
    }

    #[test]
    fn an_unresolvable_factory_leaves_the_call_alone() -> Result<(), String> {
        assert_eq!(
            skipped_reasons(
                "@dataclass\nclass C:\n    a: int = field(default_factory=Thing)\nC()\n"
            )?
            .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn container_defaults_are_not_recreated_at_call_sites() -> Result<(), String> {
        let source = "def with_list(value=[]): pass\ndef with_dict(value={}): pass\ndef with_set(value={1}): pass\ndef with_tuple(value=(1, 2)): pass\n\nwith_list()\nwith_dict()\nwith_set()\nwith_tuple()\n";
        let reasons = skipped_reasons(source)?;
        assert_eq!(reasons.len(), 4);
        assert!(reasons
            .iter()
            .all(|reason| reason.contains("is not a literal")));
        Ok(())
    }

    #[test]
    fn a_shadowed_builtin_factory_is_not_synthesized() -> Result<(), String> {
        let source = "from dataclasses import dataclass, field\n\ndef list():\n    return ('custom',)\n\n@dataclass\nclass C:\n    value: object = field(default_factory=list)\n\nC()\n";
        let reasons = skipped_reasons(source)?;
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("is not a literal"));
        Ok(())
    }

    #[test]
    fn named_container_factories_are_not_looked_up_in_callers() -> Result<(), String> {
        for factory in ["set", "frozenset"] {
            let source = format!(
                "@dataclass\nclass C:\n    value: object = field(default_factory={factory})\n\nC()\n"
            );
            let reasons = skipped_reasons(&source)?;
            assert_eq!(reasons.len(), 1, "{factory}");
            assert!(reasons[0].contains("is not a literal"), "{factory}");
        }
        Ok(())
    }

    #[test]
    fn a_bare_decorator_gets_its_removed_default() -> Result<(), String> {
        assert_eq!(
            fixed(
                "def decorate(function, flag=1):\n    function.flag = flag\n    return function\n\n@decorate\ndef target():\n    pass\n"
            )?,
            "def decorate(function, flag):\n    function.flag = flag\n    return function\n\n@lambda __no_defaults_decorated: decorate(__no_defaults_decorated, flag=1)\ndef target():\n    pass\n"
        );
        Ok(())
    }

    #[test]
    fn a_bare_decorator_parameter_does_not_capture_the_decorator() -> Result<(), String> {
        assert_eq!(
            fixed(
                "def __no_defaults_decorated(function, flag=1):\n    function.flag = flag\n    return function\n\n@__no_defaults_decorated\ndef target():\n    pass\n"
            )?,
            "def __no_defaults_decorated(function, flag):\n    function.flag = flag\n    return function\n\n@lambda __no_defaults_decorated_: __no_defaults_decorated(__no_defaults_decorated_, flag=1)\ndef target():\n    pass\n"
        );
        Ok(())
    }

    #[test]
    fn a_callback_named_without_being_called_is_reported() -> Result<(), String> {
        assert_eq!(
            skipped_reasons("def cb(x=1): return x\nrun(cb)\n")?.len(),
            1,
            "a callback passed by name is called somewhere this cannot see"
        );
        Ok(())
    }

    #[test]
    fn a_dataclass_named_without_being_called_is_left_quiet() -> Result<(), String> {
        // Class names appear in annotations and `isinstance` checks constantly,
        // and none of those are calls.
        assert!(skipped_reasons(
            "@dataclass\nclass C:\n    x: int = 1\n\n\ndef f(c: C) -> C:\n    return c\n"
        )?
        .is_empty());
        Ok(())
    }

    #[test]
    fn a_header_directive_leaves_the_call_sites_alone_too() -> Result<(), String> {
        // Nothing was removed from the suppressed signature or dataclass, so
        // the calls to them still mean what they meant.
        let source = "def kept(  # noqa: NOD001\n    a=1,\n): pass\n\n\n@dataclass\nclass Kept:  # noqa: NOD001\n    x: int = 1\n\n\ndef f(b=2): pass\n\n\nkept()\nKept()\nf()\n";
        assert_eq!(
            fixed(source)?,
            source
                .replace("def f(b=2): pass", "def f(b): pass")
                .replace("\nf()\n", "\nf(b=2)\n")
        );
        Ok(())
    }

    #[test]
    fn each_kind_of_method_is_given_what_it_already_receives() -> Result<(), String> {
        // A `staticmethod` is given nothing, a `classmethod` is given the class
        // however it is reached, and an ordinary method is given the instance
        // only when it is reached through one.
        assert_eq!(
            fixed(
                "class C:\n    @staticmethod\n    def build(kind=1): pass\n\n    \
                 @classmethod\n    def make(cls, mode=2): pass\n\n    \
                 def fetch(self, url, verify=3): pass\n\n    \
                 def use(self):\n        self.build()\n        self.make()\n        \
                 self.fetch(\"u\")\n\n\nC.build()\nC.make()\nC.fetch(None, \"u\")\n"
            )?,
            "class C:\n    @staticmethod\n    def build(kind): pass\n\n    \
             @classmethod\n    def make(cls, mode): pass\n\n    \
             def fetch(self, url, verify): pass\n\n    \
             def use(self):\n        self.build(kind=1)\n        self.make(mode=2)\n        \
             self.fetch(\"u\", verify=3)\n\n\nC.build(kind=1)\nC.make(mode=2)\n\
             C.fetch(None, \"u\", verify=3)\n"
        );
        Ok(())
    }

    #[test]
    fn context_manager_enter_defaults_are_retained_for_implicit_calls() {
        let source = "class C:\n    def __enter__(self, value=1):\n        return value\n\n    def __exit__(self, kind, error, traceback):\n        pass\n\nwith C() as value:\n    assert value == 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn context_manager_exit_defaults_are_retained_for_implicit_calls() {
        let source = "class C:\n    def __enter__(self):\n        return self\n\n    def __exit__(self, kind, error, traceback, extra=None):\n        return False\n\nwith C():\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn async_context_manager_entry_defaults_are_retained_for_implicit_calls() {
        let source = "class C:\n    async def __aenter__(self, extra=None):\n        return self\n\n    async def __aexit__(self, kind, error, traceback):\n        return False\n\nasync def main():\n    async with C():\n        pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn async_context_manager_exit_defaults_are_retained_for_implicit_calls() {
        let source = "class C:\n    async def __aenter__(self):\n        return self\n\n    async def __aexit__(self, kind, error, traceback, extra=None):\n        return False\n\nasync def main():\n    async with C():\n        pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn iterator_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __iter__(self, extra=None):\n        return iter(())\n\niter(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn reflected_operator_defaults_are_retained_for_protocol_calls() {
        // Python reaches `__radd__` through syntax, so there is no written
        // call for the fixer to add the removed default back to.
        let source = "class C:\n    def __radd__(self, other, extra=None):\n        return self\n\n1 + C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn augmented_assignment_defaults_are_retained_for_protocol_calls() {
        // Python reaches `__iadd__` through syntax, so there is no written
        // call for the fixer to add the removed default back to.
        let source = "class C:\n    def __iadd__(self, other, extra=None):\n        return self\n\ntotal = C()\ntotal += 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn unary_operator_defaults_are_retained_for_protocol_calls() {
        // Python reaches `__neg__` through syntax, so there is no written
        // call for the fixer to add the removed default back to.
        let source = "class C:\n    def __neg__(self, extra=None):\n        return self\n\n-C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn next_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __next__(self, extra=None):\n        return 1\n\nnext(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn async_iterator_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __aiter__(self, extra=None):\n        return self\n\n    async def __anext__(self):\n        raise StopAsyncIteration\n\nasync def main():\n    async for _ in C():\n        pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn async_next_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __aiter__(self):\n        return self\n\n    async def __anext__(self, extra=None):\n        raise StopAsyncIteration\n\nasync def main():\n    async for _ in C():\n        pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn length_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __len__(self, extra=None):\n        return 0\n\nlen(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn length_hint_defaults_are_retained_for_protocol_calls() {
        let source = "import operator\n\nclass C:\n    def __length_hint__(self, extra=None):\n        return 7\n\nassert operator.length_hint(C()) == 7\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn getitem_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __getitem__(self, key, extra=None):\n        return key\n\nC()[0]\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn setitem_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __setitem__(self, key, value, extra=None):\n        pass\n\nC()[0] = 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn delitem_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __delitem__(self, key, extra=None):\n        pass\n\ndel C()[0]\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn missing_key_defaults_are_retained_for_protocol_calls() {
        let source = "class C(dict):\n    def __missing__(self, key, extra=None):\n        return key\n\nC()[0]\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn contains_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __contains__(self, item, extra=None):\n        return False\n\n0 in C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn reversed_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __reversed__(self, extra=None):\n        return iter(())\n\nreversed(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn bool_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __bool__(self, extra=None):\n        return True\n\nbool(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn str_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __str__(self, extra=None):\n        return 'C'\n\nstr(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn repr_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __repr__(self, extra=None):\n        return 'C'\n\nrepr(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn bytes_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __bytes__(self, extra=None):\n        return b'C'\n\nbytes(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn buffer_defaults_are_retained_for_implicit_memoryview_calls() {
        let source = "class C:\n    def __buffer__(self, flags, extra=1):\n        return memoryview(b'abc')\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn release_buffer_defaults_are_retained_for_implicit_memoryview_callbacks() {
        let source =
            "class C:\n    def __release_buffer__(self, view, extra=1):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn iterator_send_defaults_are_retained_for_yield_from_delegation() {
        let source = "class I:\n    def __iter__(self):\n        return self\n    def __next__(self):\n        return 0\n    def send(self, value, extra=1):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn dunder_aliases_retain_implicitly_called_defaults() {
        let source = "class Values:\n    def iterate(self, items=(1, 2)): return iter(items)\n    __iter__ = iterate\n\nassert list(Values()) == [1, 2]\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn constructor_aliases_retain_implementation_defaults() {
        for source in [
            "class C:\n    def setup(self, value=1): self.value = value\n    __init__ = setup\n\nassert C().value == 1\n",
            "class C:\n    def make(cls, value=1):\n        obj = object.__new__(cls)\n        obj.value = value\n        return obj\n    __new__ = staticmethod(make)\n\nassert C().value == 1\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
            assert!(checked.signatures.is_empty(), "{source}");
        }
    }

    #[test]
    fn nested_send_defaults_are_not_iterator_protocol_defaults() {
        let source = "class I:\n    def __iter__(self):\n        return self\n    def __next__(self):\n        return 0\n    def outer(self):\n        def send(value=1):\n            return value\n        return send()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
        assert_eq!(checked.signatures.len(), 1);
    }

    #[test]
    fn iterator_throw_defaults_are_retained_for_yield_from_delegation() {
        let source = "class I:\n    def __iter__(self):\n        return self\n    def __next__(self):\n        return 0\n    def throw(self, typ, value=None, traceback=None, extra=1):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 3);
        assert!(checked
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.fix.is_none()));
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn iterator_close_defaults_are_retained_for_yield_from_cleanup() {
        let source = "class I:\n    def __iter__(self):\n        return self\n    def __next__(self):\n        return 0\n    def close(self, extra=1):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn format_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __format__(self, spec, extra=None):\n        return spec\n\nformat(C(), '')\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn hash_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __hash__(self, extra=None):\n        return 1\n\nhash(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn index_defaults_are_retained_for_protocol_calls() {
        let source = "import operator\n\nclass C:\n    def __index__(self, extra=None):\n        return 1\n\noperator.index(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn int_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __int__(self, extra=None):\n        return 1\n\nint(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn float_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __float__(self, extra=None):\n        return 1.0\n\nfloat(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn complex_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __complex__(self, extra=None):\n        return 1j\n\ncomplex(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn round_defaults_are_retained_for_protocol_calls() {
        let source =
            "class C:\n    def __round__(self, ndigits=None):\n        return 1\n\nround(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn trunc_defaults_are_retained_for_protocol_calls() {
        let source = "import math\n\nclass C:\n    def __trunc__(self, extra=None):\n        return 1\n\nmath.trunc(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn floor_defaults_are_retained_for_protocol_calls() {
        let source = "import math\n\nclass C:\n    def __floor__(self, extra=None):\n        return 1\n\nmath.floor(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn ceil_defaults_are_retained_for_protocol_calls() {
        let source = "import math\n\nclass C:\n    def __ceil__(self, extra=None):\n        return 1\n\nmath.ceil(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn fspath_defaults_are_retained_for_protocol_calls() {
        let source = "import os\n\nclass C:\n    def __fspath__(self, extra=None):\n        return '/tmp'\n\nos.fspath(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn less_than_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __lt__(self, other, extra=None):\n        return False\n\nC() < C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn less_equal_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __le__(self, other, extra=None):\n        return False\n\nC() <= C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn equality_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __eq__(self, other, extra=None):\n        return False\n\nC() == C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn inequality_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __ne__(self, other, extra=None):\n        return True\n\nC() != C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn greater_than_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __gt__(self, other, extra=None):\n        return False\n\nC() > C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn greater_equal_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __ge__(self, other, extra=None):\n        return False\n\nC() >= C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn addition_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __add__(self, other, extra=None):\n        return self\n\nC() + C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn subtraction_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __sub__(self, other, extra=None):\n        return self\n\nC() - C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn multiplication_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __mul__(self, other, extra=None):\n        return self\n\nC() * C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn matrix_multiplication_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __matmul__(self, other, extra=None):\n        return self\n\nC() @ C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn true_division_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __truediv__(self, other, extra=None):\n        return self\n\nC() / C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn floor_division_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __floordiv__(self, other, extra=None):\n        return self\n\nC() // C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn modulo_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __mod__(self, other, extra=None):\n        return self\n\nC() % C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn divmod_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __divmod__(self, other, extra=None):\n        return self, other\n\ndivmod(C(), C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn power_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __pow__(self, exponent, modulus=None):\n        return self\n\npow(C(), 2)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn left_shift_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __lshift__(self, other, extra=None):\n        return self\n\nC() << C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn right_shift_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __rshift__(self, other, extra=None):\n        return self\n\nC() >> C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn bitwise_and_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __and__(self, other, extra=None):\n        return self\n\nC() & C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn bitwise_xor_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __xor__(self, other, extra=None):\n        return self\n\nC() ^ C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn bitwise_or_defaults_are_retained_for_protocol_calls() {
        let source = "class C:\n    def __or__(self, other, extra=None):\n        return self\n\nC() | C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_user_defined_staticmethod_name_is_not_a_descriptor() -> Result<(), String> {
        let source = "def staticmethod(function):\n    return function\n\nclass C:\n    @staticmethod\n    def parse(self, value=1): pass\n\n    def run(self):\n        return self.parse(5)\n";
        assert_eq!(
            fixed(source)?,
            "def staticmethod(function):\n    return function\n\nclass C:\n    @staticmethod\n    def parse(self, value): pass\n\n    def run(self):\n        return self.parse(5)\n"
        );
        Ok(())
    }

    #[test]
    fn a_user_defined_classmethod_name_is_not_a_descriptor() -> Result<(), String> {
        let source = "def classmethod(function):\n    return function\n\nclass C:\n    @classmethod\n    def parse(self, value=1): pass\n\nC.parse(C())\n";
        assert_eq!(
            fixed(source)?,
            "def classmethod(function):\n    return function\n\nclass C:\n    @classmethod\n    def parse(self, value): pass\n\nC.parse(C(), value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn inherited_methods_resolve_through_self() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): pass\n\nclass Child(Base):\n    def run(self):\n        return self.target()\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): pass\n\nclass Child(Base):\n    def run(self):\n        return self.target(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn nested_same_scope_classes_preserve_inherited_method_calls() -> Result<(), String> {
        let source = "def outer():\n    class Base:\n        def target(self, value=1): return value\n    class Child(Base):\n        def run(self): return self.target()\n    return Child().run()\n\nassert outer() == 1\n";
        assert_eq!(
            fixed(source)?,
            "def outer():\n    class Base:\n        def target(self, value): return value\n    class Child(Base):\n        def run(self): return self.target(value=1)\n    return Child().run()\n\nassert outer() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn nested_class_inheritance_is_lexically_scoped() -> Result<(), String> {
        let source = "def first():\n    class Base:\n        def target(self, value=1): return value\n    class Child(Base):\n        def run(self): return self.target()\n    return Child().run()\n\ndef second():\n    class Base:\n        def target(self, value=2): return value\n    class Child(Base):\n        def run(self): return self.target()\n    return Child().run()\n\nassert first() == 1\nassert second() == 2\n";
        assert_eq!(
            fixed(source)?,
            "def first():\n    class Base:\n        def target(self, value): return value\n    class Child(Base):\n        def run(self): return self.target(value=1)\n    return Child().run()\n\ndef second():\n    class Base:\n        def target(self, value): return value\n    class Child(Base):\n        def run(self): return self.target(value=2)\n    return Child().run()\n\nassert first() == 1\nassert second() == 2\n"
        );
        Ok(())
    }

    #[test]
    fn statically_resolvable_base_expressions_preserve_inherited_calls() -> Result<(), String> {
        for source in [
            "class Base:\n    def target(self, value=1): return value\nAlias = Base\nclass Child(Alias):\n    def run(self): return self.target()\n",
            "class Base:\n    @classmethod\n    def __class_getitem__(cls, item): return cls\n    def target(self, value=1): return value\nclass Child(Base[int]):\n    def run(self): return self.target()\n",
            "class Outer:\n    class Base:\n        def target(self, value=1): return value\nclass Child(Outer.Base):\n    def run(self): return self.target()\n",
        ] {
            let fixed = fixed(source)?;
            assert!(fixed.contains("def target(self, value):"), "{fixed}");
            assert!(fixed.contains("self.target(value=1)"), "{fixed}");
        }
        Ok(())
    }

    #[test]
    fn aliases_of_inherited_methods_are_indexed() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    alias = Base.target\n\nassert Child().alias() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    alias = Base.target\n\nassert Child().alias(value=1) == 1\n"
        );
        Ok(())
    }

    #[test]
    fn aliases_of_unknown_inherited_methods_are_left_unresolved() -> Result<(), String> {
        let source = "class Child(External):\n    alias = External.target\n\nChild().alias()\n";
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn super_skips_the_class_the_call_is_written_in() -> Result<(), String> {
        // ``super().target()`` calls ``Base.target``, so it takes that
        // method's removed default rather than the overriding one.
        let source = "class Base:\n    def target(self, value=1): pass\n\nclass Child(Base):\n    def target(self, value=2): pass\n\n    def run(self):\n        return super().target()\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): pass\n\nclass Child(Base):\n    def target(self, value): pass\n\n    def run(self):\n        return super().target(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn imported_super_aliases_support_zero_argument_lookup() -> Result<(), String> {
        let source = "from builtins import super as parent\n\nclass Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return parent().target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "from builtins import super as parent\n\nclass Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return parent().target(value=1)\n\nassert Child().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn unrenamed_imported_super_supports_zero_argument_lookup() -> Result<(), String> {
        let source = "from builtins import super\n\nclass Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return super().target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "from builtins import super\n\nclass Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return super().target(value=1)\n\nassert Child().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn inherited_methods_resolve_through_super() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): pass\n\nclass Child(Base):\n    def run(self):\n        return super().target()\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): pass\n\nclass Child(Base):\n    def run(self):\n        return super().target(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn classmethod_cls_calls_construct_known_instances() -> Result<(), String> {
        let source = "class C:\n    def target(self, value=1): return value\n\n    @classmethod\n    def run(cls): return cls().target()\n\nassert C.run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    def target(self, value): return value\n\n    @classmethod\n    def run(cls): return cls().target(value=1)\n\nassert C.run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn self_class_calls_resolve_to_the_enclosing_class() -> Result<(), String> {
        let source = "class C:\n    @staticmethod\n    def target(value=1): return value\n\n    def run(self): return self.__class__.target()\n\nassert C().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    @staticmethod\n    def target(value): return value\n\n    def run(self): return self.__class__.target(value=1)\n\nassert C().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn bare_class_cell_calls_resolve_to_the_enclosing_class() -> Result<(), String> {
        let source = "class C:\n    @staticmethod\n    def target(value=1): return value\n\n    def run(self): return __class__.target()\n\nassert C().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    @staticmethod\n    def target(value): return value\n\n    def run(self): return __class__.target(value=1)\n\nassert C().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn class_cells_keep_their_owner_inside_nested_class_bodies() -> Result<(), String> {
        let source = "class Outer:\n    @staticmethod\n    def target(value=1): return value\n\n    def run(self):\n        class Inner:\n            value = __class__.target()\n        return Inner.value\n\nassert Outer().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class Outer:\n    @staticmethod\n    def target(value): return value\n\n    def run(self):\n        class Inner:\n            value = __class__.target(value=1)\n        return Inner.value\n\nassert Outer().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn type_self_calls_resolve_to_the_enclosing_class() -> Result<(), String> {
        let source = "class C:\n    def target(self, value=1): return value\n\n    def run(self): return type(self).target(self)\n\nassert C().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    def target(self, value): return value\n\n    def run(self): return type(self).target(self, value=1)\n\nassert C().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn inherited_methods_follow_c3_mro_in_a_diamond() -> Result<(), String> {
        let source = "class A:\n    def target(self, value=1): return value\n\nclass B(A): pass\n\nclass C(A):\n    def target(self): return 9\n\nclass D(B, C):\n    def run(self): return self.target()\n\nassert D().run() == 9\n";
        let updated = fixed(source)?;
        assert!(updated.contains("def target(self, value): return value"));
        assert!(updated.contains("return self.target()"));
        assert!(!updated.contains("self.target(value="));
        Ok(())
    }

    #[test]
    fn a_shadowed_super_call_is_not_the_builtin() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): return value\n\nclass Other:\n    def target(self): return 9\n\nclass Child(Base):\n    def run(self):\n        def super(): return Other()\n        return super().target()\n\nassert Child().run() == 9\n";
        let updated = fixed(source)?;
        assert!(updated.contains("def target(self, value): return value"));
        assert!(updated.contains("return super().target()"));
        assert!(!updated.contains("super().target(value="));
        Ok(())
    }

    #[test]
    fn explicit_super_calls_resolve_in_the_enclosing_method() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self): return super(Child, self).target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self): return super(Child, self).target(value=1)\n\nassert Child().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn explicit_super_with_another_context_is_left_unresolved() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self, other): return super(Base, other).target()\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self, other): return super(Base, other).target()\n"
        );
        Ok(())
    }

    #[test]
    fn qualified_and_aliased_staticmethods_receive_no_implicit_argument() -> Result<(), String> {
        let source = "import builtins\nfrom builtins import staticmethod as static\n\nclass C:\n    @builtins.staticmethod\n    def parse(value=1): return value\n\n    @static\n    def load(value=2): return value\n\n    def run(self):\n        return self.parse(5), self.load(6)\n";
        assert_eq!(
            fixed(source)?,
            "import builtins\nfrom builtins import staticmethod as static\n\nclass C:\n    @builtins.staticmethod\n    def parse(value): return value\n\n    @static\n    def load(value): return value\n\n    def run(self):\n        return self.parse(5), self.load(6)\n"
        );
        Ok(())
    }

    #[test]
    fn qualified_and_aliased_classmethods_receive_the_class_argument() -> Result<(), String> {
        let source = "import builtins\nfrom builtins import classmethod as class_method\n\nclass C:\n    @builtins.classmethod\n    def parse(cls, value=1): return value\n\n    @class_method\n    def load(cls, value=2): return value\n\n\nC.parse(5)\nC.load(6)\n";
        assert_eq!(
            fixed(source)?,
            "import builtins\nfrom builtins import classmethod as class_method\n\nclass C:\n    @builtins.classmethod\n    def parse(cls, value): return value\n\n    @class_method\n    def load(cls, value): return value\n\n\nC.parse(5)\nC.load(6)\n"
        );
        Ok(())
    }

    #[test]
    fn classmethod_receivers_use_class_level_descriptor_binding() -> Result<(), String> {
        let source = "class C:\n    def target(self, value=1): return value\n\n    @classmethod\n    def run(cls): return cls.target(C())\n\nassert C.run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    def target(self, value): return value\n\n    @classmethod\n    def run(cls): return cls.target(C(), value=1)\n\nassert C.run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn a_method_on_an_unknown_receiver_is_left_alone() -> Result<(), String> {
        assert_eq!(
            skipped_reasons(
                "class C:\n    def fetch(self, url, verify=1): pass\n\n\nclient.fetch(\"u\")\n"
            )?
            .first()
            .map(String::as_str),
            Some("this call cannot be tied to the definition that was fixed"),
            "`client` could be anything, so its `fetch` is not known to be this one"
        );
        Ok(())
    }

    #[test]
    fn callable_instance_defaults_are_retained_without_instance_analysis() {
        let source = "class Callable:\n    def __call__(self, value=1):\n        return value\n\ntarget = Callable()\ntarget()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_class_body_method_alias_has_the_original_signature() -> Result<(), String> {
        let source = "class C:\n    def target(self, value=1):\n        return value\n    alias = target\n\nC().alias()\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    def target(self, value):\n        return value\n    alias = target\n\nC().alias(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_normal_class_call_uses_its_init_signature() -> Result<(), String> {
        let source =
            "class C:\n    def __init__(self, value=1):\n        self.value = value\n\nC()\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    def __init__(self, value):\n        self.value = value\n\nC(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_subclass_call_uses_its_inherited_init_signature() -> Result<(), String> {
        let source = "class Base:\n    def __init__(self, value=1): self.value = value\n\nclass Child(Base):\n    pass\n\nassert Child().value == 1\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def __init__(self, value): self.value = value\n\nclass Child(Base):\n    pass\n\nassert Child(value=1).value == 1\n"
        );
        Ok(())
    }

    #[test]
    fn property_getter_defaults_are_retained_for_descriptor_calls() {
        let source = "class C:\n    @property\n    def value(self, fallback=1):\n        return fallback\n\nC().value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn init_subclass_defaults_are_retained_for_implicit_calls() {
        let source = "class Base:\n    def __init_subclass__(cls, flag=1):\n        cls.flag = flag\n\nclass Child(Base):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn new_defaults_are_retained_for_implicit_object_construction() {
        let source = "class C:\n    def __new__(cls, extra=1):\n        return super().__new__(cls)\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn del_defaults_are_retained_for_implicit_finalization() {
        let source =
            "class C:\n    def __del__(self, extra=1):\n        print(extra)\n\nc = C()\ndel c\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn getattribute_defaults_are_retained_for_implicit_attribute_reads() {
        let source = "class C:\n    def __getattribute__(self, name, extra=1):\n        return extra\n\nC().value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn getattr_defaults_are_retained_for_implicit_fallback_attribute_reads() {
        let source =
            "class C:\n    def __getattr__(self, name, extra=1):\n        return extra\n\nC().value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn setattr_defaults_are_retained_for_implicit_attribute_assignment() {
        let source = "class C:\n    def __setattr__(self, name, value, extra=1):\n        object.__setattr__(self, name, value + extra)\n\nc = C()\nc.value = 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn delattr_defaults_are_retained_for_implicit_attribute_deletion() {
        let source = "class C:\n    def __delattr__(self, name, extra=1):\n        object.__delattr__(self, name)\n\nc = C()\nc.value = 1\ndel c.value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn dir_defaults_are_retained_for_implicit_builtin_calls() {
        let source =
            "class C:\n    def __dir__(self, extra=1):\n        return [str(extra)]\n\ndir(C())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn descriptor_get_defaults_are_retained_for_implicit_reads() {
        let source = "class D:\n    def __get__(self, instance, owner, extra=1):\n        return extra\n\nclass C:\n    value = D()\n\nC().value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn descriptor_set_defaults_are_retained_for_implicit_writes() {
        let source = "class D:\n    def __set__(self, instance, value, extra=1):\n        instance.saved = value + extra\n\nclass C:\n    value = D()\n\nc = C()\nc.value = 2\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn descriptor_delete_defaults_are_retained_for_implicit_deletions() {
        let source = "class D:\n    def __delete__(self, instance, extra=1):\n        instance.deleted = extra\n\nclass C:\n    value = D()\n\nc = C()\ndel c.value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn set_name_defaults_are_retained_for_implicit_class_creation_calls() {
        let source = "class D:\n    def __set_name__(self, owner, name, extra=1):\n        owner.bound_name = name + str(extra)\n\nclass C:\n    value = D()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn instancecheck_defaults_are_retained_for_implicit_isinstance_calls() {
        let source = "class M(type):\n    def __instancecheck__(cls, instance, extra=1):\n        return extra == 1\n\nclass C(metaclass=M):\n    pass\n\nisinstance(object(), C)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn subclasscheck_defaults_are_retained_for_implicit_issubclass_calls() {
        let source = "class M(type):\n    def __subclasscheck__(cls, subclass, extra=1):\n        return extra == 1\n\nclass C(metaclass=M):\n    pass\n\nissubclass(int, C)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn subclasshook_defaults_are_retained_for_implicit_abc_checks() {
        let source = "from abc import ABC\n\nclass C(ABC):\n    @classmethod\n    def __subclasshook__(cls, subclass, extra=1):\n        return extra == 1\n\nissubclass(int, C)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn class_getitem_defaults_are_retained_for_implicit_subscription() {
        let source = "class C:\n    def __class_getitem__(cls, item, extra=1):\n        return (item, extra)\n\nC[int]\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn mro_entries_defaults_are_retained_for_implicit_base_resolution() {
        let source = "class Proxy:\n    def __mro_entries__(self, bases, extra=1):\n        return ()\n\nproxy = Proxy()\n\nclass C(proxy):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn prepare_defaults_are_retained_for_implicit_metaclass_calls() {
        let source = "class M(type):\n    @classmethod\n    def __prepare__(mcls, name, bases, extra=1):\n        return {}\n\nclass C(metaclass=M):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn metaclass_new_defaults_are_retained_for_implicit_class_creation() {
        let source = "class M(type):\n    def __new__(mcls, name, bases, namespace, extra=1):\n        return super().__new__(mcls, name, bases, namespace)\n\nclass C(metaclass=M):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn metaclass_init_defaults_are_retained_for_implicit_class_creation() -> Result<(), String> {
        let source = "class M(type):\n    def __init__(cls, name, bases, namespace, extra=1):\n        super().__init__(name, bases, namespace)\n\nclass C(metaclass=M):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());

        assert_eq!(
            fixed("class Ordinary:\n    def __init__(self, value=1):\n        self.value = value\n\nOrdinary()\n")?,
            "class Ordinary:\n    def __init__(self, value):\n        self.value = value\n\nOrdinary(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn metaclass_mro_defaults_are_retained_for_implicit_class_creation() {
        let source = "class M(type):\n    def mro(cls, extra=1):\n        return super().mro()\n\nclass C(metaclass=M):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn await_defaults_are_retained_for_implicit_await_expressions() {
        let source = "class C:\n    def __await__(self, extra=1):\n        async def done():\n            return extra\n        return done().__await__()\n\nasync def main():\n    return await C()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn sizeof_defaults_are_retained_for_implicit_getsizeof_calls() {
        let source = "class C:\n    def __sizeof__(self, extra=1):\n        return 100 + extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn copy_defaults_are_retained_for_implicit_shallow_copy_calls() {
        let source = "class C:\n    def __copy__(self, extra=1):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn replace_defaults_are_retained_for_implicit_copy_replace_calls() {
        let source =
            "class C:\n    def __replace__(self, extra=1, **changes):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn deepcopy_defaults_are_retained_for_implicit_deep_copy_calls() {
        let source = "class C:\n    def __deepcopy__(self, memo, extra=1):\n        return extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn reduce_defaults_are_retained_for_implicit_pickle_calls() {
        let source = "class C:\n    def __reduce__(self, extra=1):\n        return (C, ())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn reduce_ex_defaults_are_retained_for_implicit_pickle_calls() {
        let source =
            "class C:\n    def __reduce_ex__(self, protocol, extra=1):\n        return (C, ())\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn getnewargs_defaults_are_retained_for_implicit_pickle_calls() {
        let source = "class C(tuple):\n    def __new__(cls):\n        return super().__new__(cls, ())\n\n    def __getnewargs__(self, extra=1):\n        return ()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn getnewargs_ex_defaults_are_retained_for_implicit_pickle_calls() {
        let source = "class C(tuple):\n    def __new__(cls):\n        return super().__new__(cls, ())\n\n    def __getnewargs_ex__(self, extra=1):\n        return (), {}\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn getstate_defaults_are_retained_for_implicit_pickle_calls() {
        let source =
            "class C:\n    def __getstate__(self, extra=1):\n        return {\"extra\": extra}\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn setstate_defaults_are_retained_for_implicit_unpickle_calls() {
        let source = "class C:\n    def __setstate__(self, state, extra=1):\n        self.__dict__.update(state)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn module_getattr_defaults_are_retained_for_implicit_attribute_fallback() {
        let source = "import sys\n\nmodule = sys.modules[__name__]\n\ndef __getattr__(name, extra=1):\n    return extra\n\nmodule.missing\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn module_dir_defaults_are_retained_for_implicit_builtin_calls() {
        let source = "import sys\n\nmodule = sys.modules[__name__]\n\ndef __dir__(extra=1):\n    return [str(extra)]\n\ndir(module)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn dataclass_post_init_defaults_are_retained_for_generated_calls() {
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int\n\n    def __post_init__(self, extra=1):\n        self.extra = extra\n\nC(5)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_staticmethod_parameter_named_self_is_not_an_implicit_receiver() -> Result<(), String> {
        let source = "class C:\n    def fetch(self, value=1): return value\n\n    @staticmethod\n    def run(self):\n        return self.fetch()\n";
        assert_eq!(
            skipped_reasons(source)?.first().map(String::as_str),
            Some("this call cannot be tied to the definition that was fixed")
        );
        Ok(())
    }

    #[test]
    fn a_shadowed_class_name_is_not_a_known_receiver() -> Result<(), String> {
        let source = "class Client:\n    @staticmethod\n    def build(value=1): return value\n\ndef run(Client):\n    return Client.build()\n";
        assert_eq!(
            skipped_reasons(source)?.first().map(String::as_str),
            Some("this call cannot be tied to the definition that was fixed")
        );
        Ok(())
    }

    #[test]
    fn a_nested_function_self_is_not_the_enclosing_instance() -> Result<(), String> {
        let source = "class C:\n    def fetch(self, value=1): return value\n\n    def run(self, other):\n        def inner(self):\n            return self.fetch()\n        return inner(other)\n";
        assert_eq!(
            skipped_reasons(source)?.first().map(String::as_str),
            Some("this call cannot be tied to the definition that was fixed")
        );
        Ok(())
    }

    #[test]
    fn a_nested_function_can_close_over_the_enclosing_receiver() -> Result<(), String> {
        let source = "class C:\n    def target(self, value=1): return value\n    def run(self):\n        def nested(): return self.target()\n        return nested()\nassert C().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class C:\n    def target(self, value): return value\n    def run(self):\n        def nested(): return self.target(value=1)\n        return nested()\nassert C().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn an_inheriting_dataclass_is_left_alone() -> Result<(), String> {
        // The parent's fields come first in the constructor and are not
        // visible from this class body, so the argument positions are unknown.
        let source = "@dataclass\nclass Child(Parent):\n    b: int = 2\n\n\nChild()\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass Child(Parent):\n    b: int\n\n\nChild()\n"
        );
        assert_eq!(
            skipped_reasons(source)?.first().map(String::as_str),
            Some(
                "the dataclass inherits fields, so its constructor is not known from the file \
                 that defines it"
            )
        );
        Ok(())
    }

    #[test]
    fn rebinding_a_shape_alias_drops_the_old_dataclass_fields() -> Result<(), String> {
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\nAlias = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        let updated = fixed(source)?;
        assert!(updated.ends_with("\nChild()\n"), "{updated}");
        assert!(!updated.contains("Child(inherited="), "{updated}");
        Ok(())
    }

    #[test]
    fn a_qualified_custom_generic_base_is_not_the_typing_construct() -> Result<(), String> {
        let source = "from dataclasses import dataclass\n\nclass helpers:\n    @dataclass\n    class Generic:\n        inherited: int = 1\n\n@dataclass\nclass C(helpers.Generic):\n    value: int = 2\n\nC()\n";
        let updated = fixed(source)?;
        assert!(updated.ends_with("\nC()\n"), "{updated}");
        assert_eq!(
            skipped_reasons(source)?.first().map(String::as_str),
            Some(
                "the dataclass inherits fields, so its constructor is not known from the file \
                 that defines it"
            )
        );
        Ok(())
    }

    #[test]
    fn a_later_generic_class_does_not_shadow_an_earlier_typing_base() -> Result<(), String> {
        let source = "from dataclasses import dataclass\nfrom typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n@dataclass\nclass C(Generic[T]):\n    value: int = 1\n\nC()\n\nclass Generic:\n    pass\n";
        assert_eq!(
            fixed(source)?,
            "from dataclasses import dataclass\nfrom typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n@dataclass\nclass C(Generic[T]):\n    value: int\n\nC(value=1)\n\nclass Generic:\n    pass\n"
        );
        Ok(())
    }

    #[test]
    fn a_rebound_bare_structural_name_carries_aliased_fields() -> Result<(), String> {
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nGeneric = Base\n\n@dataclass\nclass Child(Generic):\n    value: int = 2\n\nChild()\n";
        assert_eq!(
            fixed(source)?,
            "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int\n\nGeneric = Base\n\n@dataclass\nclass Child(Generic):\n    value: int\n\nChild(inherited=1, value=2)\n"
        );
        Ok(())
    }

    #[test]
    fn an_assignment_based_dataclass_init_suppresses_constructor_generation() {
        let source = "from dataclasses import dataclass\n\ndef initialize(self):\n    self.value = 5\n\n@dataclass\nclass C:\n    value: int = 1\n    __init__ = initialize\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_dataclass_with_a_metaclass_has_no_assumed_call_signature() {
        let source = "from dataclasses import dataclass\n\nclass Meta(type):\n    def __call__(cls):\n        return 5\n\n@dataclass\nclass C(metaclass=Meta):\n    value: int = 1\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_dataclass_inheriting_a_metaclass_has_no_assumed_call_signature() {
        // A metaclass is inherited, so `C` is built by `Meta` too even though
        // it names no `metaclass=` of its own.
        let source = "from dataclasses import dataclass\n\nclass Meta(type):\n    def __call__(cls):\n        return 5\n\nclass Base(metaclass=Meta):\n    pass\n\n@dataclass\nclass C(Base):\n    value: int = 1\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_dataclass_with_an_imported_base_has_no_assumed_metaclass() {
        let source = "from dataclasses import dataclass\nfrom base import Parent\n\n@dataclass\nclass Child(Parent):\n    value: int = 1\n\nassert Child() == 9\nassert Child.value == 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_redefined_base_without_a_metaclass_is_not_treated_as_metaclass_built() {
        // An earlier `Base` that named a metaclass must not stick after a
        // later plain `Base` takes its place.
        let source = "from dataclasses import dataclass\n\nclass Meta(type):\n    def __call__(cls):\n        return 5\n\nclass Base(metaclass=Meta):\n    pass\n\nclass Base:\n    pass\n\n@dataclass\nclass C(Base):\n    value: int = 1\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
        assert_eq!(checked.signatures.len(), 1);
    }

    #[test]
    fn a_metaclass_base_inside_a_function_does_not_leak() {
        let source = "from dataclasses import dataclass\n\nclass Meta(type):\n    def __call__(cls):\n        return 5\n\ndef build():\n    class Base(metaclass=Meta):\n        pass\n\n@dataclass\nclass Base:\n    value: int = 1\n\n@dataclass\nclass C(Base):\n    extra: int = 2\n\nC()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.fix.is_some()));
        assert_eq!(checked.signatures.len(), 2);
    }

    #[test]
    fn a_dataclass_only_type_checkers_see_is_not_rewritten() {
        // The block does not run, so the class has no constructor at runtime
        // and no call for the fixer to keep in step with.
        let source = "from typing import TYPE_CHECKING\nfrom dataclasses import dataclass\n\nif TYPE_CHECKING:\n    @dataclass\n    class C:\n        value: int = 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn a_name_bound_in_an_enclosing_scope_is_not_the_fixed_function() -> Result<(), String> {
        let source = "def connect(host, timeout=30):\n    pass\n\n\n\
                      def wrapper(connect):\n    connect(\"h\")\n\n\n\
                      def other():\n    connect = open\n    connect(\"h\")\n\n\n\
                      def fine():\n    connect(\"h\")\n";
        assert_eq!(
            fixed(source)?,
            "def connect(host, timeout):\n    pass\n\n\n\
             def wrapper(connect):\n    connect(\"h\")\n\n\n\
             def other():\n    connect = open\n    connect(\"h\")\n\n\n\
             def fine():\n    connect(\"h\", timeout=30)\n",
            "only the call that really reaches the fixed function is filled in"
        );
        assert_eq!(
            skipped_reasons(source)?,
            [
                "this call cannot be tied to the definition that was fixed",
                "this call cannot be tied to the definition that was fixed"
            ]
        );
        Ok(())
    }

    #[test]
    fn a_global_declaration_keeps_the_module_binding_visible() -> Result<(), String> {
        let source = "def target(value=1): return value\n\ndef run():\n    global target\n    result = target()\n    target = lambda: 2\n    return result\n";
        assert_eq!(
            fixed(source)?,
            "def target(value): return value\n\ndef run():\n    global target\n    result = target(value=1)\n    target = lambda: 2\n    return result\n"
        );
        Ok(())
    }

    #[test]
    fn a_nonlocal_declaration_keeps_the_enclosing_binding_visible() -> Result<(), String> {
        let source = "def outer():\n    def target(value=1): return value\n    def run():\n        nonlocal target\n        result = target()\n        target = lambda: 2\n        return result\n    return run()\nassert outer() == 1\n";
        assert_eq!(
            fixed(source)?,
            "def outer():\n    def target(value): return value\n    def run():\n        nonlocal target\n        result = target(value=1)\n        target = lambda: 2\n        return result\n    return run()\nassert outer() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn pattern_capture_names_shadow_fixed_functions() -> Result<(), String> {
        for pattern in [
            "target",
            "[first, *target]",
            "{'value': first, **target}",
            "[first] as target",
        ] {
            let source = format!(
                "def target(value=1): return value\n\ndef run(candidate):\n    match candidate:\n        case {pattern}:\n            pass\n    return target()\n"
            );
            assert_eq!(
                skipped_reasons(&source)?.first().map(String::as_str),
                Some("this call cannot be tied to the definition that was fixed"),
                "{pattern}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_lambda_default_in_a_condition_is_reported() {
        // The test of an `if`, `elif`, or `while` is ordinary code that runs
        // when the clause is reached.
        for source in [
            "if (lambda value=1: value)():\n    pass\n",
            "if False:\n    pass\nelif (lambda value=1: value)():\n    pass\n",
            "while (lambda value=1: value)():\n    break\n",
        ] {
            assert_eq!(messages(source, false).len(), 1, "{source}");
        }
    }

    #[test]
    fn a_loop_over_an_empty_literal_never_runs() {
        // `{}` is the empty mapping literal, and the only empty literal that
        // cannot be written as a set, so it belongs with the others.
        for empty in ["()", "[]", "set()", "{}"] {
            let source = format!("for _ in {empty}:\n    def target(value=1): pass\n");
            let found = messages(&source, false);
            let expected: usize = usize::from(empty == "set()");
            assert_eq!(found.len(), expected, "{empty}");
        }
    }

    #[test]
    fn every_shape_that_binds_a_name_shadows_it() -> Result<(), String> {
        for binding in [
            "connect = open",
            "for connect in []: pass",
            "with open(\"f\") as connect: pass",
            "import connect",
            "from os import path as connect",
            "if (connect := open): pass",
        ] {
            let source =
                format!("def connect(host, timeout=30):\n    pass\n\n\ndef f():\n    {binding}\n    connect(\"h\")\n");
            assert_eq!(
                fixed(&source)?,
                source.replace("host, timeout=30", "host, timeout"),
                "{binding}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_nested_scope_does_not_shadow_the_scope_holding_it() -> Result<(), String> {
        // `inner`'s parameter belongs to `inner`. The call in `outer` still
        // reaches the module-level definition and must still be filled in.
        let source = "def connect(host, timeout=30):\n    pass\n\n\n\
                      def outer():\n    def inner(connect):\n        return connect\n    \
                      connect(\"h\")\n";
        assert_eq!(
            fixed(source)?,
            "def connect(host, timeout):\n    pass\n\n\n\
             def outer():\n    def inner(connect):\n        return connect\n    \
             connect(\"h\", timeout=30)\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn calls_to_a_nested_function_receive_removed_defaults() -> Result<(), String> {
        let source =
            "def outer():\n    def inner(value=1):\n        return value\n    return inner()\n";
        assert_eq!(
            fixed(source)?,
            "def outer():\n    def inner(value):\n        return value\n    return inner(value=1)\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn nested_and_module_functions_have_distinct_identities() -> Result<(), String> {
        let nested_default = "def outer():\n    def target(value=1): return value\n    return target()\ndef target(): return 9\nassert target() == 9\n";
        assert_eq!(
            fixed(nested_default)?,
            "def outer():\n    def target(value): return value\n    return target(value=1)\ndef target(): return 9\nassert target() == 9\n"
        );

        let module_default = "def target(value=1): return value\ndef outer():\n    def target(): return 9\n    return target()\nassert outer() == 9\n";
        assert_eq!(
            fixed(module_default)?,
            "def target(value): return value\ndef outer():\n    def target(): return 9\n    return target()\nassert outer() == 9\n"
        );
        Ok(())
    }

    #[test]
    fn nested_function_calls_use_the_binding_owner_scope() -> Result<(), String> {
        let source = "def outer():\n    def target(value=1): return value\n    def run(): return target()\n    return run()\nassert outer() == 1\n";
        assert_eq!(
            fixed(source)?,
            "def outer():\n    def target(value): return value\n    def run(): return target(value=1)\n    return run()\nassert outer() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn same_named_nested_callables_do_not_collide() -> Result<(), String> {
        let functions = "def first():\n    def target(value=1): return value\n    return target()\ndef second():\n    def target(value=2): return value\n    return target()\n";
        let result = fixed(functions)?;
        assert!(result.contains("target(value=1)"), "{result}");
        assert!(result.contains("target(value=2)"), "{result}");

        let repeated = "def outer():\n    def target(value=1): return value\n    def target(value=2): return value\n    return target()\n";
        let checked = check_source(
            Path::new("example.py"),
            repeated,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.fix.is_none()));
        assert!(checked.signatures.is_empty());

        let dataclasses = "from dataclasses import dataclass\ndef first():\n    @dataclass\n    class C:\n        value: int = 1\n    return C()\ndef second():\n    @dataclass\n    class C:\n        value: int = 2\n    return C()\n";
        let result = fixed(dataclasses)?;
        assert!(result.contains("C(value=1)"), "{result}");
        assert!(result.contains("C(value=2)"), "{result}");
        Ok(())
    }

    #[test]
    fn definitions_competing_across_branches_keep_their_defaults() {
        // Which one survives is not knowable, so neither may lose a default
        // while the call is left as written.
        let source = "import os\n\nif os.environ:\n    def target(value=1): pass\nelse:\n    def target(value=2): pass\n\ntarget()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.fix.is_none()));
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn repeated_function_definitions_keep_their_defaults() {
        let source = "def target(value=1): pass\ntarget()\n\ndef target(value=2): pass\ntarget()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.fix.is_none()));
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn decorated_functions_keep_defaults_when_the_signature_may_change() {
        let source = "def replace(function):\n    return lambda: 5\n\n@replace\ndef target(value=1): pass\n\ntarget()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn function_decorators_use_the_enclosing_scope() -> Result<(), String> {
        let source = "def target(value=1):\n    return lambda function: function\n\n@target()\ndef decorated():\n    target = 5\n";
        assert_eq!(
            fixed(source)?,
            "def target(value):\n    return lambda function: function\n\n@target(value=1)\ndef decorated():\n    target = 5\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn function_annotations_use_the_enclosing_scope() -> Result<(), String> {
        let source = "def target(value=1):\n    return int\n\ndef decorated(item: target()):\n    target = 5\n";
        assert_eq!(
            fixed(source)?,
            "def target(value):\n    return int\n\ndef decorated(item: target(value=1)):\n    target = 5\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn future_annotations_are_not_rewritten_as_calls() -> Result<(), String> {
        let source = "from __future__ import annotations\n\ndef value(x=1): return x\ndef f(x: value()) -> value(): pass\ny: value()\nresult = value()\n";
        assert_eq!(
            fixed(source)?,
            "from __future__ import annotations\n\ndef value(x): return x\ndef f(x: value()) -> value(): pass\ny: value()\nresult = value(x=1)\n"
        );
        Ok(())
    }

    #[test]
    fn lambda_defaults_use_the_enclosing_scope() -> Result<(), String> {
        let source = "def target(value=1):\n    return 5\n\nhandler = lambda target=target(): target  # noqa: NOD001\n";
        assert_eq!(
            fixed(source)?,
            "def target(value):\n    return 5\n\nhandler = lambda target=target(value=1): target  # noqa: NOD001\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn class_headers_use_the_enclosing_scope() -> Result<(), String> {
        let source = "def target(value=1):\n    return lambda cls: cls\n\n@target()\nclass C:\n    target = 5\n";
        assert_eq!(
            fixed(source)?,
            "def target(value):\n    return lambda cls: cls\n\n@target(value=1)\nclass C:\n    target = 5\n"
        );
        // The body's `target` is the enclosing name until the assignment
        // binds it, exactly as at module scope, so naming it is reported.
        assert_eq!(
            skipped_reasons(source)?,
            ["it is named here without being called, so the removed default cannot be supplied"]
        );
        Ok(())
    }

    #[test]
    fn comprehension_targets_shadow_only_inside_the_comprehension() -> Result<(), String> {
        let source = "def target(value=1): return value\n\ndef run():\n    [target() for target in [lambda: 5]]\n    return target()\n";
        assert_eq!(
            fixed(source)?,
            "def target(value): return value\n\ndef run():\n    [target() for target in [lambda: 5]]\n    return target(value=1)\n"
        );
        assert_eq!(
            skipped_reasons(source)?,
            ["this call cannot be tied to the definition that was fixed"]
        );
        Ok(())
    }

    #[test]
    fn a_comprehensions_leftmost_iterable_is_outside_its_scope() -> Result<(), String> {
        // Python evaluates that iterable before the targets exist, so the
        // call in it is the outer `target` and has to be kept in step.
        let source = "def target(value=1): return value\n\nresult = [x for target in [target()] for x in [1]]\n";
        assert_eq!(
            fixed(source)?,
            "def target(value): return value\n\nresult = [x for target in [target(value=1)] for x in [1]]\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn calls_to_a_nested_dataclass_receive_removed_defaults() -> Result<(), String> {
        let source = "def outer():\n    @dataclass\n    class Inner:\n        value: int = 1\n    return Inner()\n";
        assert_eq!(
            fixed(source)?,
            "def outer():\n    @dataclass\n    class Inner:\n        value: int\n    return Inner(value=1)\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn decorator_replaced_dataclasses_keep_their_field_defaults() {
        let source = "def replace(cls):\n    return lambda: 5\n\n@replace\n@dataclass\nclass C:\n    value: int = 1\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_lambda_parameter_shadows_only_inside_the_lambda() -> Result<(), String> {
        let source = "def connect(host, timeout=30):\n    pass\n\n\n\
                      def f():\n    (lambda connect: connect(\"h\"))(open)\n    \
                      connect(\"h\")\n";
        assert_eq!(
            fixed(source)?,
            "def connect(host, timeout):\n    pass\n\n\n\
             def f():\n    (lambda connect: connect(\"h\"))(open)\n    \
             connect(\"h\", timeout=30)\n",
            "the call inside the lambda goes to its parameter; the one after it does not"
        );
        Ok(())
    }

    #[test]
    fn a_hash_in_comment_prose_does_not_bound_the_deletion() -> Result<(), String> {
        // `#35` is text, not a pragma. Stopping there left a mangled fragment.
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001  see #35  # type: ignore[misc]\n")?,
            "def b(y): pass  # type: ignore[misc]\n"
        );
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001  #type:ignore\n")?,
            "def b(y): pass  #type:ignore\n",
            "a pragma written without a space still opens a segment"
        );
        assert_eq!(
            fixed("def b(y): pass  # noqa: NOD001  refs #1 and #2\n")?,
            "def b(y): pass\n",
            "prose alone leaves nothing worth keeping"
        );
        Ok(())
    }

    #[test]
    fn an_alias_bound_to_more_than_one_member_resolves_to_neither() {
        // Whichever import comes last must not decide it.
        let source = "from dataclasses import dataclass, field as f\n\n\
                      def helper():\n    from pydantic import Field as f\n    return f\n\n\
                      @dataclass\nclass C:\n    x: int = f(...)\n";
        assert_eq!(
            messages(source, false),
            ["dataclass field `x` has a default"],
            "reading `f` as pydantic's `Field` would have taken the `...` for `required`"
        );
    }

    #[test]
    fn a_class_the_file_defines_is_not_the_typing_construct() -> Result<(), String> {
        // This `Protocol` is a dataclass of the file's own carrying a field.
        // Taking it for the typing construct would drop `base_field` from the
        // constructor; it is prepended instead.
        let source = "@dataclass\nclass Protocol:\n    base_field: int = 0\n\n\n\
                      @dataclass\nclass Box(Protocol):\n    value: int = 1\n\n\nBox()\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass Protocol:\n    base_field: int\n\n\n\
             @dataclass\nclass Box(Protocol):\n    value: int\n\n\n\
             Box(base_field=0, value=1)\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_base_that_is_a_dataclass_of_this_file_contributes_its_fields() -> Result<(), String> {
        let source = "@dataclass\nclass Parent:\n    a: int = 1\n\n\n\
                      @dataclass\nclass Child(Parent):\n    b: int = 2\n\n\n\
                      Child()\nChild(9)\nChild(9, 8)\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass Parent:\n    a: int\n\n\n\
             @dataclass\nclass Child(Parent):\n    b: int\n\n\n\
             Child(a=1, b=2)\nChild(9, b=2)\nChild(9, 8)\n",
            "the base's fields come first, so `Child(9)` already supplies `a`"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_subclass_needs_the_defaults_its_base_lost() -> Result<(), String> {
        // `Child` removes nothing of its own, but its constructor still lost
        // the default `a` had, so its call sites need it back.
        let source = "@dataclass\nclass Parent:\n    a: int = 1\n\n\n\
                      @dataclass\nclass Child(Parent):\n    b: int = 2\n\n\nChild()\n";
        assert!(fixed(source)?.contains("Child(a=1, b=2)"));
        Ok(())
    }

    #[test]
    fn a_subclass_keeps_defaults_after_a_retained_base_default() {
        let source = "@dataclass\nclass Base:\n    a: int = 1  # noqa: NOD001\n\n\n@dataclass\nclass Child(Base):\n    b: int = 2\n\n\nChild()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert_eq!(checked.diagnostics[0].line, 8);
        assert_eq!(checked.diagnostics[0].fix, None);
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn an_unknown_base_protects_every_default_from_inherited_metaclasses() {
        let source = "from dataclasses import dataclass, field\nfrom base import Parent\n\n@dataclass\nclass Child(Parent):\n    positional: int = 2\n    keyword: int = field(default=3, kw_only=True)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn imported_base_default_uncertainty_propagates_through_local_subclasses() {
        let source = "from dataclasses import dataclass\nfrom base import Parent\n\n@dataclass\nclass Middle(Parent):\n    middle: int = 2\n\n@dataclass\nclass Child(Middle):\n    child: int = 3\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics.iter().all(|item| item.fix.is_none()));
    }

    #[test]
    fn a_base_this_file_cannot_see_still_gives_up() -> Result<(), String> {
        for source in [
            // Imported from elsewhere.
            "@dataclass\nclass Child(Imported):\n    b: int = 2\n\n\nChild()\n",
            // Reached through a module, so not a bare name of this file.
            "@dataclass\nclass Child(other.Parent):\n    b: int = 2\n\n\nChild()\n",
            // Two bases: `dataclasses` walks the reverse MRO to order them.
            "@dataclass\nclass A:\n    a: int = 1\n\n\n@dataclass\nclass B:\n    b: int = 2\n\n\n@dataclass\nclass C(A, B):\n    c: int = 3\n\n\nC()\n",
            // A base whose own constructor is unknown.
            "@dataclass\nclass Middle(Imported):\n    a: int = 1\n\n\n@dataclass\nclass Child(Middle):\n    b: int = 2\n\n\nChild()\n",
            // A name two classes of this file share resolves to neither. They
            // have to be in one scope to share it: a class nested in a
            // function is not the module-level name of the same spelling.
            "@dataclass\nclass Parent:\n    a: int = 1\n\n\n@dataclass\nclass Parent:\n    z: int = 9\n\n\n@dataclass\nclass Child(Parent):\n    b: int = 2\n\n\nChild()\n",
        ] {
            assert_eq!(
                skipped_reasons(source)?.first().map(String::as_str),
                Some(
                    "the dataclass inherits fields, so its constructor is not known from the \
                     file that defines it"
                ),
                "{source}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_class_nested_out_of_scope_does_not_shadow_the_typing_construct() -> Result<(), String> {
        // The `Protocol` in scope where `Box` is written is the imported one.
        let source = "def factory():\n    class Protocol:\n        pass\n    \
                      return Protocol\n\n\n\
                      @dataclass\nclass Box(Protocol):\n    value: int = 1\n\n\nBox()\n";
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_generator_call_that_needs_nothing_added_is_not_warned_about() -> Result<(), String> {
        // The generator already supplies the only parameter whose default went,
        // so no argument would be appended and there is nothing to refuse.
        let source = "def f(items=1):\n    pass\n\n\nf(x for x in range(3))\n";
        assert_eq!(
            fixed(source)?,
            "def f(items):\n    pass\n\n\nf(x for x in range(3))\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_class_body_binding_does_not_shadow_a_method_global() -> Result<(), String> {
        let source = "def connect(host, timeout=30):\n    pass\n\n\n\
                      class Client:\n    connect = staticmethod(open)\n\n    \
                      def run(self):\n        connect(\"h\")\n";
        assert_eq!(
            fixed(source)?,
            source
                .replace("host, timeout=30", "host, timeout")
                .replace("connect(\"h\")", "connect(\"h\", timeout=30)")
        );
        Ok(())
    }

    #[test]
    fn class_namespaces_do_not_enclose_methods_or_comprehensions() -> Result<(), String> {
        let method = "def target(value=1): return value\nclass C:\n    target = lambda: 9\n    def run(self): return target()\nassert C().run() == 1\n";
        let result = fixed(method)?;
        assert!(result.contains("return target(value=1)"), "{result}");

        let comprehension = "def target(value=1): return value\nclass C:\n    target = lambda: 9\n    values = [target() for _ in [0]]\nassert C.values == [1]\n";
        let result = fixed(comprehension)?;
        assert!(
            result.contains("values = [target(value=1) for _ in [0]]"),
            "{result}"
        );
        Ok(())
    }

    /// Fix `source` as a `.pyi` stub, where `= ...` cannot be removed.
    ///
    /// Unlike `fixed`, this does not assert that nothing is left afterwards:
    /// a kept `= ...` is exactly what these cases are about.
    fn stub_fixed(source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.pyi");
        std::fs::write(&path, source).map_err(|error| error.to_string())?;
        let checked = check_file(
            &path,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        let mut edits: BTreeMap<PathBuf, Vec<Edit>> = BTreeMap::new();
        for diagnostic in &checked.diagnostics {
            if let Some(range) = diagnostic.fix {
                edits
                    .entry(diagnostic.path.clone())
                    .or_default()
                    .push(Edit::deletion(range));
            }
        }
        let mut updated = 0;
        let mut unfixed = BTreeSet::new();
        write_fixes_atomically(fixed_sources(edits, &mut updated, &mut unfixed)?)?;
        assert!(unfixed.is_empty(), "the result must parse");
        std::fs::read_to_string(&path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_late_preflight_failure_writes_none_of_the_project() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let first = directory.path().join("a.py");
        let missing = directory.path().join("z/missing.py");
        std::fs::write(&first, "def first(value=1): pass\n").map_err(|error| error.to_string())?;
        let changes = BTreeMap::from([
            (
                first.clone(),
                (
                    "def first(value=1): pass\n".to_owned(),
                    "def first(value): pass\n".to_owned(),
                ),
            ),
            (
                missing,
                (
                    "def last(value=2): pass\n".to_owned(),
                    "def last(value): pass\n".to_owned(),
                ),
            ),
        ]);
        assert!(write_fixes_atomically(changes).is_err());
        assert_eq!(
            std::fs::read_to_string(first).map_err(|error| error.to_string())?,
            "def first(value=1): pass\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn atomic_fixes_refuse_to_split_hard_links() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let source = directory.path().join("source.py");
        let alias = directory.path().join("alias.py");
        let original = "def example(value=1): pass\n";
        std::fs::write(&source, original).map_err(|error| error.to_string())?;
        std::fs::hard_link(&source, &alias).map_err(|error| error.to_string())?;
        let changes = BTreeMap::from([(
            source.clone(),
            (original.to_owned(), "def example(value): pass\n".to_owned()),
        )]);

        let error = match write_fixes_atomically(changes) {
            Ok(()) => return Err("hard links must be refused".to_owned()),
            Err(error) => error,
        };

        assert!(error.contains("hard-linked file"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&source).map_err(|error| error.to_string())?,
            original
        );
        assert_eq!(
            std::fs::read_to_string(&alias).map_err(|error| error.to_string())?,
            original
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn atomic_fixes_preserve_extended_attributes() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let source = directory.path().join("source.py");
        let original = "def example(value=1): pass\n";
        let attribute = "com.example.no-defaults-test";
        let value = b"kept metadata";
        std::fs::write(&source, original).map_err(|error| error.to_string())?;
        xattr::set(&source, attribute, value).map_err(|error| error.to_string())?;
        let changes = BTreeMap::from([(
            source.clone(),
            (original.to_owned(), "def example(value): pass\n".to_owned()),
        )]);

        write_fixes_atomically(changes)?;

        assert_eq!(
            xattr::get(&source, attribute).map_err(|error| error.to_string())?,
            Some(value.to_vec())
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn attributes_the_system_owns_do_not_abort_a_fix() {
        // macOS carries names such as `com.apple.macl` and
        // `com.apple.provenance` on ordinary files and refuses to let a user
        // process write them. Abandoning the run over one would leave the
        // whole project unfixed.
        assert!(system_owned_attribute(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!system_owned_attribute(&std::io::Error::from(
            std::io::ErrorKind::StorageFull
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn atomic_fixes_preserve_access_control_lists() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let source = directory.path().join("source.py");
        let original = "def example(value=1): pass\n";
        std::fs::write(&source, original).map_err(|error| error.to_string())?;
        let user = std::env::var("USER").map_err(|error| error.to_string())?;
        let entries = [exacl::AclEntry::allow_user(&user, exacl::Perm::READ, None)];
        exacl::setfacl(&[&source], &entries, None).map_err(|error| error.to_string())?;
        let before = exacl::getfacl(&source, None).map_err(|error| error.to_string())?;
        let changes = BTreeMap::from([(
            source.clone(),
            (original.to_owned(), "def example(value): pass\n".to_owned()),
        )]);

        write_fixes_atomically(changes)?;

        let after = exacl::getfacl(&source, None).map_err(|error| error.to_string())?;
        assert_eq!(after, before);
        Ok(())
    }

    #[test]
    fn a_default_after_one_that_is_kept_is_kept_too() -> Result<(), String> {
        // `= ...` in a stub cannot be removed, and Python does not allow a
        // parameter without a default to follow one with a default, so `y`
        // has to stay as well.
        assert_eq!(
            stub_fixed("def f(x: int = ..., y: int = 5) -> None: ...\n")?,
            "def f(x: int = ..., y: int = 5) -> None: ...\n"
        );
        assert_eq!(
            stub_fixed("def f(x: int = 1, y: int = ...) -> None: ...\n")?,
            "def f(x: int = 1, y: int = ...) -> None: ...\n",
            "stub defaults describe optional parameters and stay intact"
        );
        Ok(())
    }

    #[test]
    fn a_suppressed_positional_default_protects_later_defaults() {
        let source = "def f(\n    a=1,  # noqa: NOD001\n    b=2,\n):\n    pass\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert_eq!(checked.diagnostics[0].line, 3);
        assert_eq!(checked.diagnostics[0].fix, None);
    }

    #[test]
    fn a_keyword_only_default_is_not_constrained_by_order() -> Result<(), String> {
        // Everything after the `*` may take a default or not, in any order.
        assert_eq!(
            stub_fixed("def f(x: int = ..., *, y: int = 5) -> None: ...\n")?,
            "def f(x: int = ..., *, y: int = 5) -> None: ...\n"
        );
        Ok(())
    }

    #[test]
    fn a_positional_only_default_obeys_the_same_order() -> Result<(), String> {
        assert_eq!(
            stub_fixed("def f(x: int = ..., y: int = 5, /) -> None: ...\n")?,
            "def f(x: int = ..., y: int = 5, /) -> None: ...\n"
        );
        Ok(())
    }

    #[test]
    fn a_dataclass_field_after_a_kept_one_is_kept_too() -> Result<(), String> {
        // `dataclasses` rejects a field without a default following one with
        // it, and that is a `TypeError` at class creation rather than a
        // syntax error, so the post-fix parse guard would not have caught it.
        assert_eq!(
            stub_fixed("@dataclass\nclass C:\n    a: int = ...\n    b: int = 5\n")?,
            "@dataclass\nclass C:\n    a: int = ...\n    b: int = 5\n"
        );
        assert_eq!(
            stub_fixed("@dataclass\nclass C:\n    a: int = 1\n    b: int = ...\n")?,
            "@dataclass\nclass C:\n    a: int\n    b: int = ...\n",
            "a field before the kept one is still fixed"
        );
        Ok(())
    }

    #[test]
    fn a_suppressed_dataclass_field_protects_later_fields() {
        let source = "@dataclass\nclass C:\n    a: int = 1  # noqa: NOD001\n    b: int = 2\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert_eq!(checked.diagnostics[0].line, 4);
        assert_eq!(checked.diagnostics[0].fix, None);
    }

    #[test]
    fn a_keyword_only_field_is_not_constrained_by_order() -> Result<(), String> {
        assert_eq!(
            stub_fixed("from dataclasses import KW_ONLY\n\n@dataclass\nclass C:\n    a: int = ...\n    _: KW_ONLY\n    b: int = 5\n")?,
            "from dataclasses import KW_ONLY\n\n@dataclass\nclass C:\n    a: int = ...\n    _: KW_ONLY\n    b: int\n"
        );
        assert_eq!(
            stub_fixed("@dataclass(kw_only=True)\nclass C:\n    a: int = ...\n    b: int = 5\n")?,
            "@dataclass(kw_only=True)\nclass C:\n    a: int = ...\n    b: int\n"
        );
        Ok(())
    }

    #[test]
    fn each_class_body_starts_free_of_the_last_one() -> Result<(), String> {
        assert_eq!(
            stub_fixed(
                "@dataclass\nclass C:\n    a: int = ...\n\n\n@dataclass\nclass D:\n    b: int = 5\n"
            )?,
            "@dataclass\nclass C:\n    a: int = ...\n\n\n@dataclass\nclass D:\n    b: int\n",
            "what `C` kept says nothing about `D`"
        );
        Ok(())
    }

    #[test]
    fn a_default_in_an_unfixed_file_is_not_counted_as_removed() {
        let unfixed = PathBuf::from("left.py");
        let diagnostic = |path: &str| Diagnostic {
            path: PathBuf::from(path),
            line: 1,
            column: 1,
            code: "NOD001",
            message: String::new(),
            fix: Some(TextRange::default()),
        };
        let diagnostics = [diagnostic("left.py"), diagnostic("written.py")];
        assert_eq!(
            removed_defaults(&diagnostics, &BTreeSet::from([unfixed]), &BTreeSet::new()),
            1,
            "the file left on disk still has its default"
        );
        assert_eq!(
            removed_defaults(&diagnostics, &BTreeSet::new(), &BTreeSet::new()),
            2
        );
    }

    #[test]
    fn a_file_whose_fix_would_not_parse_is_left_alone_by_itself() -> Result<(), String> {
        // The guard is a "this linter has a bug" path, so it is driven here
        // with an edit that deliberately produces nonsense: no input shape is
        // supposed to reach it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let broken = directory.path().join("broken.py");
        let sound = directory.path().join("sound.py");
        std::fs::write(&broken, "def f(x=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&sound, "def g(y=2): pass\n").map_err(|error| error.to_string())?;
        let edits = BTreeMap::from([
            (
                broken.clone(),
                // Delete the `)`, which cannot parse.
                vec![Edit::deletion(TextRange::new(
                    TextSize::new(9),
                    TextSize::new(10),
                ))],
            ),
            (
                sound.clone(),
                vec![Edit::deletion(TextRange::new(
                    TextSize::new(7),
                    TextSize::new(9),
                ))],
            ),
        ]);
        let mut updated = 0;
        let mut unfixed = BTreeSet::new();
        let changes = fixed_sources(edits, &mut updated, &mut unfixed)?;
        assert_eq!(unfixed, BTreeSet::from([broken.clone()]));
        assert!(!changes.contains_key(&broken), "the bad file is untouched");
        assert_eq!(
            changes.get(&sound).map(|(_, fixed)| fixed.as_str()),
            Some("def g(y): pass\n"),
            "every other file is still fixed"
        );
        Ok(())
    }

    #[test]
    fn a_bare_generator_argument_is_left_alone() -> Result<(), String> {
        // Appending after `x for x in y` would not parse, and the post-fix
        // parse guard turned that into a failure for the whole run.
        let source = "def f(items, timeout=30):\n    pass\n\n\nf(x for x in range(3))\n";
        assert_eq!(
            fixed(source)?,
            "def f(items, timeout):\n    pass\n\n\nf(x for x in range(3))\n"
        );
        assert_eq!(
            skipped_reasons(source)?,
            [
                "the call's argument is a bare generator expression, which Python allows only \
              when it is the only one"
            ]
        );
        Ok(())
    }

    #[test]
    fn a_parenthesized_generator_argument_is_still_filled_in() -> Result<(), String> {
        let source = "def f(items, timeout=30):\n    pass\n\n\nf((x for x in range(3)))\n";
        assert_eq!(
            fixed(source)?,
            "def f(items, timeout):\n    pass\n\n\nf((x for x in range(3)), timeout=30)\n"
        );
        assert!(skipped_reasons(source)?.is_empty());
        Ok(())
    }

    #[test]
    fn one_unfixable_call_does_not_hold_back_the_rest() -> Result<(), String> {
        let source = "def f(items, timeout=30):\n    pass\n\n\nf(x for x in range(3))\nf([1])\n";
        assert_eq!(
            fixed(source)?,
            "def f(items, timeout):\n    pass\n\n\nf(x for x in range(3))\nf([1], timeout=30)\n"
        );
        Ok(())
    }

    #[test]
    fn a_keyword_only_field_holds_no_position_in_the_constructor() -> Result<(), String> {
        // The real constructor is `__init__(self, b=2, *, a=1)`, so `C(5)`
        // means `b=5`. Reading `a` as the first positional slot made the fix
        // drop the default `a` now needs and pass `b` twice.
        let source = "@dataclass\nclass C:\n    a: int = field(kw_only=True, default=1)\n    \
                      b: int = 2\n\n\nC(5)\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass C:\n    a: int = field(kw_only=True)\n    b: int\n\n\nC(5, a=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_truthy_non_boolean_kw_only_option_is_honoured() -> Result<(), String> {
        let source = "@dataclass\nclass C:\n    first: int = field(default=1, kw_only=1)\n    second: int = 2\n\n\nC(5)\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass C:\n    first: int = field(kw_only=1)\n    second: int\n\n\nC(5, first=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_decorator_that_says_kw_only_makes_every_field_keyword_only() -> Result<(), String> {
        let source =
            "@dataclass(kw_only=True)\nclass D:\n    a: int = 1\n    b: int = 2\n\n\nD()\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass(kw_only=True)\nclass D:\n    a: int\n    b: int\n\n\nD(a=1, b=2)\n"
        );
        Ok(())
    }

    #[test]
    fn a_falsey_dataclass_init_option_keeps_field_defaults() {
        let source = "@dataclass(init=0)\nclass C:\n    value: int = 1\n\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn a_decorator_reached_through_a_module_keeps_defaults() {
        let source = "import helpers\nfrom dataclasses import dataclass\n\n@helpers.marker\n@dataclass\nclass C:\n    first: int = 1\n    second: int = 2\n\n\nC(5)\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics.iter().all(|item| item.fix.is_none()));
    }

    #[test]
    fn a_kw_only_marker_makes_the_fields_after_it_keyword_only() -> Result<(), String> {
        let source = "from dataclasses import KW_ONLY\n\n@dataclass\nclass E:\n    a: int = 1\n    _: KW_ONLY\n    b: int = 2\n\n\nE()\n";
        assert_eq!(
            fixed(source)?,
            "from dataclasses import KW_ONLY\n\n@dataclass\nclass E:\n    a: int\n    _: KW_ONLY\n    b: int\n\n\nE(a=1, b=2)\n"
        );
        Ok(())
    }

    #[test]
    fn a_field_that_says_kw_only_false_overrides_the_class() -> Result<(), String> {
        // `__init__(self, a=1, *, b=2)`, so `F(9)` already supplies `a`.
        let source = "@dataclass(kw_only=True)\nclass F:\n    \
                      a: int = field(kw_only=False, default=1)\n    b: int = 2\n\n\nF(9)\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass(kw_only=True)\nclass F:\n    a: int = field(kw_only=False)\n    \
             b: int\n\n\nF(9, b=2)\n"
        );
        Ok(())
    }

    #[test]
    fn a_positional_field_after_a_keyword_only_one_keeps_its_slot() -> Result<(), String> {
        // `__init__(self, b=2, c=3, *, a=1)`: `G(5)` supplies `b`, so only `c`
        // and `a` are missing.
        let source = "@dataclass\nclass G:\n    a: int = field(kw_only=True, default=1)\n    \
                      b: int = 2\n    c: int = 3\n\n\nG(5)\n";
        // The keywords are appended in the order the fields are written, which
        // is what the rest of the fixer does; only which fields are missing is
        // at stake here.
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass G:\n    a: int = field(kw_only=True)\n    b: int\n    \
             c: int\n\n\nG(5, a=1, c=3)\n"
        );
        Ok(())
    }

    #[test]
    fn a_base_that_declares_no_fields_does_not_hide_the_constructor() -> Result<(), String> {
        // `Generic[T]`, `Protocol`, `ABC`, and `object` contribute no fields,
        // so the constructor is exactly what this class body says it is.
        for base in [
            "Generic[T]",
            "typing.Generic[T]",
            "Protocol",
            "Protocol[T]",
            "ABC",
            "object",
        ] {
            let source = format!("@dataclass\nclass Box({base}):\n    value: int = 1\n\n\nBox()\n");
            assert_eq!(
                fixed(&source)?,
                format!("@dataclass\nclass Box({base}):\n    value: int\n\n\nBox(value=1)\n"),
                "{base}"
            );
            assert!(skipped_reasons(&source)?.is_empty(), "{base}");
        }
        Ok(())
    }

    #[test]
    fn a_field_carrying_base_alongside_a_structural_one_still_hides_it() -> Result<(), String> {
        let source = "@dataclass\nclass Box(Parent, Generic[T]):\n    value: int = 1\n\n\nBox()\n";
        assert_eq!(
            skipped_reasons(source)?.first().map(String::as_str),
            Some(
                "the dataclass inherits fields, so its constructor is not known from the file \
                 that defines it"
            ),
            "one base that may carry fields is enough to give up"
        );
        Ok(())
    }

    #[test]
    fn a_field_kept_out_of_the_constructor_keeps_its_default() {
        // `init=False` keeps the field out of `__init__`, so naming it in a
        // call would raise `TypeError`, while deleting it loses the attribute.
        let source = "@dataclass\nclass C:\n    x: int = 1\n    y: int = field(default=5, init=False)\n\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_some());
        assert!(checked.diagnostics[1].fix.is_none());
    }

    #[test]
    fn a_field_with_a_falsey_init_option_keeps_its_default() {
        let source = "@dataclass\nclass C:\n    value: int = field(default=1, init=0)\n\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_kw_only_marker_takes_no_argument_position() -> Result<(), String> {
        // `_: KW_ONLY` declares no field, so it must not consume the slot that
        // tells whether `b` was already supplied positionally.
        assert_eq!(
            fixed("from dataclasses import KW_ONLY\n\n@dataclass\nclass C:\n    a: int\n    _: KW_ONLY\n    b: int = 2\n\n\nC(1)\n")?,
            "from dataclasses import KW_ONLY\n\n@dataclass\nclass C:\n    a: int\n    _: KW_ONLY\n    b: int\n\n\nC(1, b=2)\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_without_a_generated_constructor_keeps_defaults() {
        let source = "@dataclass(init=False)\nclass C:\n    x: int = 1\n\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_called_decorator_keeps_dataclass_field_defaults() {
        // The factory is applied to the class, so what it returns is what
        // `C()` constructs. That this one gives the class back unchanged is
        // not visible from the decorator itself.
        let source = "from dataclasses import dataclass\n\ndef marker(**options):\n    return lambda cls: cls\n\n@marker(init=False)\n@dataclass\nclass C:\n    value: int = 1\n\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn a_dataclass_with_an_explicit_initializer_keeps_field_defaults() {
        let source = "@dataclass\nclass C:\n    value: int = 1\n\n    def __init__(self):\n        self.value = 5\n\n\nC()\n";
        let checked = check_source(
            Path::new("example.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn a_call_inside_a_removed_default_is_not_rewritten() -> Result<(), String> {
        // The call is deleted along with the default, so inserting into it
        // would write into text that no longer exists.
        assert_eq!(
            fixed("def g(a=1): pass\ndef f(x=g()): pass\n")?,
            "def g(a): pass\ndef f(x): pass\n"
        );
        Ok(())
    }

    #[test]
    fn unknown_configuration_keys_are_rejected() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("pyproject.toml");
        std::fs::write(&path, "[tool.no_defaults]\nprivateonly = true\n")
            .map_err(|error| error.to_string())?;
        let error = load_config_path(&path).err().ok_or("expected an error")?;
        assert!(error.contains("invalid [tool.no_defaults]"), "{error}");
        assert!(error.contains("privateonly"), "{error}");
        Ok(())
    }

    #[test]
    fn hyphenated_configuration_keys_are_still_accepted() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("pyproject.toml");
        std::fs::write(
            &path,
            "[tool.no_defaults]\nprivate-only = true\nrespect-reexports = true\nfield-base-classes = [ \"msgspec.Struct\" ]\nper-file-enforcement.\"tests/**\" = \"all\"\n",
        )
        .map_err(|error| error.to_string())?;
        let loaded = load_config_path(&path)?;
        assert!(loaded.config.private_only);
        assert!(loaded.config.respect_reexports);
        assert_eq!(loaded.config.field_base_classes, ["msgspec.Struct"]);
        assert_eq!(loaded.config.per_file_enforcement.len(), 1);
        Ok(())
    }

    #[test]
    fn files_sharing_a_configuration_each_get_their_own_enforcement() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = directory
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.no_defaults]\nprivate_only = true\n\n[tool.no_defaults.per_file_enforcement]\n\"tests/**\" = \"all\"\n\"exempt.py\" = \"none\"\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::create_dir(root.join("tests")).map_err(|error| error.to_string())?;
        let paths = [
            root.join("tests").join("test_api.py"),
            root.join("exempt.py"),
            root.join("api.py"),
        ];
        for path in &paths {
            std::fs::write(path, "def f(x=1): pass\n").map_err(|error| error.to_string())?;
        }
        // One compiled set of overrides now serves every file, so it has to
        // stay applied per file rather than once.
        let settings = settings_for_files(&paths, false, false)?;
        assert_eq!(settings[0].private_only, Some(false));
        assert_eq!(settings[1].private_only, None);
        assert_eq!(settings[2].private_only, Some(true));
        Ok(())
    }

    #[test]
    fn closest_configuration_wins_for_each_file() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let nested = directory.path().join("package");
        std::fs::create_dir(&nested).map_err(|error| error.to_string())?;
        std::fs::write(
            directory.path().join("pyproject.toml"),
            "[tool.no_defaults]\nprivate_only = true\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            nested.join("pyproject.toml"),
            "[tool.no_defaults]\nprivate_only = false\n",
        )
        .map_err(|error| error.to_string())?;
        let root_file = directory.path().join("root.py");
        let nested_file = nested.join("nested.py");
        std::fs::write(&root_file, "def f(x=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&nested_file, "def f(x=1): pass\n").map_err(|error| error.to_string())?;
        let settings = settings_for_files(&[root_file, nested_file], false, false)?;
        assert_eq!(settings[0].private_only, Some(true));
        assert_eq!(settings[1].private_only, Some(false));
        Ok(())
    }

    #[test]
    fn a_global_declaration_reaches_the_module_binding() -> Result<(), String> {
        // `global target` names the module's binding, not the import an
        // enclosing function made under the same name. `nonlocal` does name
        // that enclosing one.
        for (body, expected) in [
            (
                "from api import target\n\ndef outer():\n    from other import target\n    def inner():\n        global target\n        target()\n    return inner\n",
                "target(alpha=1)",
            ),
            (
                "def outer():\n    from other import target\n    def inner():\n        nonlocal target\n        target()\n    return inner\n",
                "target(beta=2)",
            ),
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let api = directory.path().join("api.py");
            let other = directory.path().join("other.py");
            let user = directory.path().join("user.py");
            std::fs::write(&api, "def target(alpha=1): pass\n")
                .map_err(|error| error.to_string())?;
            std::fs::write(&other, "def target(beta=2): pass\n")
                .map_err(|error| error.to_string())?;
            std::fs::write(&user, body).map_err(|error| error.to_string())?;
            fix_all(&[api, other, user.clone()])?;
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert!(updated.contains(expected), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_subscripted_base_carries_its_metaclass() {
        // `Child(Parent[int])` builds on `Parent` and inherits the metaclass
        // intercepting attribute access, exactly as `Child(Parent)` does.
        for base in ["Parent", "Parent[int]"] {
            let source = format!(
                "from dataclasses import dataclass\n\nclass Meta(type):\n    def __getattribute__(self, name): pass\n\n@dataclass\nclass Parent(metaclass=Meta):\n    a: int = 1\n\n@dataclass\nclass Child({base}):\n    b: int = 2\n"
            );
            let checked = check_source(
                Path::new("fixture.py"),
                &source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert!(
                checked.diagnostics.iter().all(|d| d.fix.is_none()),
                "{base}"
            );
        }
    }

    #[test]
    fn a_rewrap_is_not_an_overwrite_but_what_follows_one_is() {
        // `target = staticmethod(target)` keeps the same function behind the
        // name, so the default is still what a call needs. Anything else
        // assigned to the name replaces it, including after a rewrap.
        for (source, fixable) in [
            (
                "class C:\n    def target(self, value=1): pass\n    target = staticmethod(target)\n",
                true,
            ),
            (
                "class C:\n    def target(self, value=1): pass\n    target = staticmethod(target)\n    target = lambda self: 9\n",
                false,
            ),
            (
                "class C:\n    def target(self, value=1): pass\n    target = lambda self: 9\n",
                false,
            ),
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1);
            assert_eq!(checked.diagnostics[0].fix.is_some(), fixable, "{source}");
        }
    }

    #[test]
    fn only_a_descriptor_wrapper_counts_as_a_rewrap() {
        // `staticmethod` leaves the function's own parameters behind the name.
        // An unknown call may return anything, so the default stays put.
        for (source, fixable) in [
            (
                "class C:\n    def target(self, value=1): pass\n    target = staticmethod(target)\n",
                true,
            ),
            (
                "class C:\n    def target(self, value=1): pass\n    target = memoize(target)\n",
                false,
            ),
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1);
            assert_eq!(checked.diagnostics[0].fix.is_some(), fixable, "{source}");
        }
    }

    #[test]
    fn an_unreachable_loop_else_import_does_not_take_over() -> Result<(), String> {
        // The `else` runs only where the loop was not broken out of, so an
        // import in there must not replace what the function was written
        // against.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let other = directory.path().join("other.py");
        let user = directory.path().join("user.py");
        std::fs::write(&api, "def target(alpha=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&other, "def target(beta=2): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "def run():\n    from api import target\n    for _ in [1]:\n        break\n    else:\n        from other import target\n    target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, other, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert!(!updated.contains("target(beta=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_class_built_in_a_function_sees_a_later_base() {
        // `build()` runs after the module is executed, so `Parent` exists by
        // then and its metaclass may replace what a call reads.
        for source in [
            "class Meta(type):\n    def __getattribute__(self, name): pass\n\nclass Parent(metaclass=Meta):\n    pass\n\ndef build():\n    class Child(Parent):\n        def run(self, value=1): pass\n    return Child\n",
            "def build():\n    class Child(Parent):\n        def run(self, value=1): pass\n    return Child\n\nclass Meta(type):\n    def __getattribute__(self, name): pass\n\nclass Parent(metaclass=Meta):\n    pass\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1);
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
        }
    }

    #[test]
    fn an_unknown_guard_does_not_bind_for_later_cases() -> Result<(), String> {
        // The second case runs only where the first guard failed, so the
        // import in that guarded body is not one it can be assumed to see.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let other = directory.path().join("other.py");
        let user = directory.path().join("user.py");
        std::fs::write(&api, "def target(alpha=1): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(&other, "def target(beta=2): pass\n").map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "from api import target\n\nmatch 1:\n    case 1 if unknown():\n        from other import target\n    case 2:\n        target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, other, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert!(!updated.contains("target(beta=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn an_alias_only_drops_what_it_really_rebinds() -> Result<(), String> {
        // `import pkg as pkg` names the same package, so the dotted imports
        // under it still stand. A different module taking the name does
        // replace them, and an import resolving to nothing rebinds nothing.
        for (caller, rewritten) in [
            ("import pkg.api\n\npkg.api.target()\n", true),
            (
                "import pkg.api\nimport pkg as pkg\n\npkg.api.target()\n",
                true,
            ),
            (
                "import pkg.api\nimport external as pkg\n\npkg.api.target()\n",
                false,
            ),
            (
                "import pkg.api\nimport external as pkg, pkg.missing\n\npkg.api.target()\n",
                false,
            ),
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let package = directory.path().join("pkg");
            std::fs::create_dir(&package).map_err(|error| error.to_string())?;
            std::fs::write(package.join("__init__.py"), "").map_err(|error| error.to_string())?;
            std::fs::write(package.join("api.py"), "def target(value=1): pass\n")
                .map_err(|error| error.to_string())?;
            let external = directory.path().join("external.py");
            std::fs::write(&external, "").map_err(|error| error.to_string())?;
            let user = directory.path().join("user.py");
            std::fs::write(&user, caller).map_err(|error| error.to_string())?;
            fix_all(&[
                package.join("__init__.py"),
                package.join("api.py"),
                external,
                user.clone(),
            ])?;
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert_eq!(updated.contains("target(value=1)"), rewritten, "{caller}");
        }
        Ok(())
    }

    #[test]
    fn an_annotation_without_a_value_keeps_a_dataclass_alias_shape() -> Result<(), String> {
        // `Alias: object` declares a type and binds nothing, so `Child` still
        // inherits `Base`'s field and its call needs that argument.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\nAlias: object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        assert_eq!(
            fixed(source)?,
            "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int\n\nAlias = Base\nAlias: object\n\n@dataclass\nclass Child(Alias):\n    value: int\n\nChild(inherited=1, value=2)\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_scope_walrus_replaces_an_earlier_import() -> Result<(), String> {
        // The class namespace resolves a name to an import bound in the same
        // body ahead of everything else, so a walrus that took the name over
        // has to drop it. Rewriting past one wrote `target(value=1)` against
        // a lambda that takes nothing.
        for body in [
            "class C:\n    from api import target\n\n    (target := staticmethod(lambda: 9))\n    result = target()\n",
            "class C:\n    from api import target\n\n    holder = [(target := staticmethod(lambda: 9))]\n    result = target()\n",
            "class C:\n    from api import target\n\n    flag = True\n    if flag:\n        (target := staticmethod(lambda: 9))\n    result = target()\n",
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let api = directory.path().join("api.py");
            let user = directory.path().join("user.py");
            std::fs::write(&api, "def target(value=1):\n    return value\n")
                .map_err(|error| error.to_string())?;
            std::fs::write(&user, body).map_err(|error| error.to_string())?;
            fix_all(&[api, user.clone()])?;
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert!(!updated.contains("target(value=1)"), "{updated}");
        }
        // The same class body without the walrus is rewritten, so the calls
        // above are left alone by the rebinding rather than by nothing.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let user = directory.path().join("user.py");
        std::fs::write(&api, "def target(value=1):\n    return value\n")
            .map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "class C:\n    from api import target\n\n    result = target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert!(updated.contains("target(value=1)"), "{updated}");
        Ok(())
    }

    #[test]
    fn an_annotated_assignment_drops_a_dataclass_alias_shape() -> Result<(), String> {
        // `Alias: object = object` does rebind, so nothing is known about what
        // `Child` inherits and its call is left alone.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\nAlias: object = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        let updated = fixed(source)?;
        assert!(updated.ends_with("\nChild()\n"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_rebound_base_name_loses_its_dataclass_shape() -> Result<(), String> {
        // A loop, context-manager, walrus, or match target replaces the class
        // the name stood for just as an assignment does. Keeping the shape
        // wrote the old base's fields into a subclass's constructor.
        let head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n";
        let tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for rebind in [
            "for Alias in [object]:\n    pass\n",
            "class Ctx:\n    def __enter__(self):\n        return object\n\n    def __exit__(self, *args):\n        return False\n\nwith Ctx() as Alias:\n    pass\n",
            "print(Alias := object)\n",
            "match object:\n    case Alias:\n        pass\n",
        ] {
            let updated = fixed(&format!("{head}{rebind}{tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        // Without a rebinding the alias still carries the fields through.
        let updated = fixed(&format!("{head}{tail}"))?;
        assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_rebound_flag_is_no_longer_known_to_be_true() -> Result<(), String> {
        // The same rebindings decide a branch the recorded truthiness would
        // otherwise settle, so a stale value picked the wrong base entirely.
        let head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nFLAG = True\n";
        let tail = "\nif FLAG:\n    Alias = Base\nelse:\n    Alias = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for rebind in ["for FLAG in [False]:\n    pass\n", "print(FLAG := False)\n"] {
            let updated = fixed(&format!("{head}{rebind}{tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        let updated = fixed(&format!("{head}{tail}"))?;
        assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_nested_rebinding_leaves_module_truthiness_alone() -> Result<(), String> {
        // A loop or walrus target in a function or class body binds that
        // scope's own name, leaving the module-level flag it happens to share
        // a name with untouched. Dropping the recorded truth made the branch
        // below uncertain, so the base was unknown and `Child()` kept its
        // arguments while the fields it needed lost their defaults.
        let head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nFLAG = True\n";
        let tail = "\nif FLAG:\n    Alias = Base\nelse:\n    Alias = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for nested in [
            "def helper():\n    for FLAG in [False]:\n        pass\n",
            "def helper():\n    print(FLAG := False)\n",
            "class Holder:\n    for FLAG in [False]:\n        pass\n",
        ] {
            let updated = fixed(&format!("{head}{nested}{tail}"))?;
            assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        }
        // `global` sends the body's binding to the module namespace, so the
        // flag there does stop being known, as it does when the rebinding is
        // written at module level.
        for rebind in [
            "def helper():\n    global FLAG\n    for FLAG in [False]:\n        pass\n\nhelper()\n",
            "def helper():\n    global FLAG\n    FLAG = False\n\nhelper()\n",
            "for FLAG in [False]:\n    pass\n",
        ] {
            let updated = fixed(&format!("{head}{rebind}{tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_class_match_capture_shadows_an_import_before_a_case_is_chosen() -> Result<(), String> {
        // A pattern that cannot be selected statically still binds its
        // captures for its own body, so a capture named after an earlier class
        // import takes that name over there. Only a case capturing some other
        // name leaves the import standing.
        for (case, rewritten) in [
            ("case [other]:", true),
            ("case [target]:", false),
            ("case [_] as target:", false),
            ("case Wrapper(inner=target):", false),
            ("case target if flag():", false),
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let api = directory.path().join("api.py");
            let user = directory.path().join("user.py");
            std::fs::write(&api, "def target(value=1): return value\n")
                .map_err(|error| error.to_string())?;
            let caller = format!(
                "class C:\n    from api import target\n    match subject:\n        {case}\n            result = target()\n"
            );
            std::fs::write(&user, &caller).map_err(|error| error.to_string())?;
            fix_all(&[api, user.clone()])?;
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert_eq!(updated.contains("target(value=1)"), rewritten, "{caller}");
        }
        Ok(())
    }

    #[test]
    fn a_class_match_capture_ends_with_its_own_case() -> Result<(), String> {
        // A case after one whose pattern cannot be selected statically runs
        // only where that pattern did not match, so a capture that one made is
        // not in force there and the class import it displaced still stands.
        // The class body after the whole match is reached whichever case ran,
        // so there a captured name does stay uncertain.
        for (cases, rewritten) in [
            (
                "        case [target]:\n            result = target()\n",
                false,
            ),
            (
                "        case [target]:\n            pass\n        case [other]:\n            result = target()\n",
                true,
            ),
            (
                "        case [other]:\n            pass\n        case [another]:\n            result = target()\n",
                true,
            ),
            (
                "        case [target]:\n            pass\n    result = target()\n",
                false,
            ),
            (
                "        case [other]:\n            pass\n    result = target()\n",
                true,
            ),
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let api = directory.path().join("api.py");
            let user = directory.path().join("user.py");
            std::fs::write(&api, "def target(value=1): return value\n")
                .map_err(|error| error.to_string())?;
            let caller =
                format!("class C:\n    from api import target\n    match subject:\n{cases}");
            std::fs::write(&user, &caller).map_err(|error| error.to_string())?;
            fix_all(&[api, user.clone()])?;
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert_eq!(
                updated.contains("result = target(value=1)"),
                rewritten,
                "{caller}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_class_match_capture_undoes_a_reimport_in_its_case() -> Result<(), String> {
        // Undoing a capture has to undo an import the case body wrote for that
        // same name, because a later case runs only where the pattern did not
        // match and so reaches neither the capture nor the import. The name
        // there is whatever the case found, an earlier class import or a
        // module-level one, and never the re-import. An import of a name the
        // pattern did not capture is left alone.
        for (caller, expected) in [
            (
                "class C:\n    from api import target\n    match subject:\n        case [target]:\n            from other import target\n        case [x]:\n            result = target()\n",
                "result = target(value=1)",
            ),
            (
                "from api import target\nclass C:\n    match subject:\n        case [target]:\n            from other import target\n        case [x]:\n            result = target()\n",
                "result = target(value=1)",
            ),
            (
                "class C:\n    from api import target\n    match subject:\n        case [x]:\n            from other import target\n        case [y]:\n            result = target()\n",
                "result = target(value=2)",
            ),
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let api = directory.path().join("api.py");
            let other = directory.path().join("other.py");
            let user = directory.path().join("user.py");
            std::fs::write(&api, "def target(value=1): return value\n")
                .map_err(|error| error.to_string())?;
            std::fs::write(&other, "def target(value=2): return value\n")
                .map_err(|error| error.to_string())?;
            std::fs::write(&user, caller).map_err(|error| error.to_string())?;
            fix_all(&[api, other, user.clone()])?;
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert!(updated.contains(expected), "{caller}\n{updated}");
        }
        Ok(())
    }

    #[test]
    fn stub_annotations_are_not_rewritten_as_calls() -> Result<(), String> {
        // A stub postpones its annotations without the future import, so a
        // call in one is a type expression rather than a call site. The two
        // `.py` cases pin the surrounding behaviour: without the import the
        // annotation runs and must be rewritten, with it it must not.
        for (name, caller, expected) in [
            (
                "stub.pyi",
                "from api import helper\n\ndef f(x: helper()) -> None: ...\n",
                "def f(x: helper()) -> None: ...\n",
            ),
            (
                "plain.py",
                "from api import helper\n\ndef f(x: helper()) -> None: pass\n",
                "def f(x: helper(value=1)) -> None: pass\n",
            ),
            (
                "postponed.py",
                "from __future__ import annotations\n\nfrom api import helper\n\ndef f(x: helper()) -> None: pass\n",
                "def f(x: helper()) -> None: pass\n",
            ),
        ] {
            let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
            let api = directory.path().join("api.py");
            let user = directory.path().join(name);
            std::fs::write(&api, "def helper(value=1): return value\n")
                .map_err(|error| error.to_string())?;
            std::fs::write(&user, caller).map_err(|error| error.to_string())?;
            fix_all(&[api.clone(), user.clone()])?;
            assert_eq!(
                std::fs::read_to_string(&api).map_err(|error| error.to_string())?,
                "def helper(value): return value\n"
            );
            let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
            assert!(updated.ends_with(expected), "{name}\n{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_class_body_global_rebinding_drops_module_truthiness() -> Result<(), String> {
        // A class body runs when its statement does, so a `global` binding
        // there lands in the module namespace straight away and the flag a
        // later module-level test reads is the one the body wrote. Keeping the
        // recorded truth took the branch the class body had already ruled out,
        // writing the wrong base's fields into `Child()`.
        let head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nFLAG = True\n";
        let tail = "\nif FLAG:\n    Alias = Base\nelse:\n    Alias = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for rebind in [
            "class Holder:\n    global FLAG\n    FLAG = False\n",
            "class Holder:\n    global FLAG\n    for FLAG in [False]:\n        pass\n",
            "class Holder:\n    global FLAG\n    print(FLAG := False)\n",
        ] {
            let updated = fixed(&format!("{head}{rebind}{tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        // Without `global` the body binds a class attribute and the module
        // flag stands, so the calls above are left alone by the rebinding
        // rather than by the class body alone.
        let updated = fixed(&format!("{head}class Holder:\n    FLAG = False\n{tail}"))?;
        assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_global_rebinding_drops_the_module_dataclass_shape() -> Result<(), String> {
        // `global` sends a body's binding to the module namespace, so the
        // class the module name stood for is gone by the time a later
        // subclass names it. Shapes are keyed by the enclosing scope, which
        // left the module entry standing, and `Child()` was rewritten with an
        // `inherited` argument the rebound base no longer accepts.
        let head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n";
        let tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for rebind in [
            "class Holder:\n    global Alias\n    Alias = object\n",
            "class Holder:\n    global Alias\n    for Alias in [object]:\n        pass\n",
            "def helper():\n    global Alias\n    Alias = object\n\nhelper()\n",
        ] {
            let updated = fixed(&format!("{head}{rebind}{tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        // Without `global` the body binds its own name, the module alias still
        // stands for the dataclass, and the fields come through as they do
        // when no body intervenes at all.
        for kept in [
            "class Holder:\n    Alias = object\n",
            "def helper():\n    Alias = object\n\nhelper()\n",
            "",
        ] {
            let updated = fixed(&format!("{head}{kept}{tail}"))?;
            assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_global_rebinding_drops_a_structural_base_alias() -> Result<(), String> {
        // A structural base declares no fields, so a dataclass built on one
        // has exactly the fields its own body writes and its constructor is
        // known from the file that defines it. `global` sends a body's binding
        // to the module namespace, so the imported name stands for the
        // rebinding by the time a later subclass names it, and `Child()` was
        // rewritten without the fields that binding brought with it.
        for (imported, base) in [
            ("from typing import Protocol", "Protocol"),
            ("from abc import ABC", "ABC"),
        ] {
            let head = format!(
                "from dataclasses import dataclass\n{imported}\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\n"
            );
            let tail =
                format!("\n@dataclass\nclass Child({base}):\n    value: int = 2\n\nChild()\n");
            for rebind in [
                format!("class Holder:\n    global {base}\n    {base} = Base\n"),
                format!("def helper():\n    global {base}\n    {base} = Base\n\nhelper()\n"),
            ] {
                let updated = fixed(&format!("{head}{rebind}{tail}"))?;
                assert!(!updated.contains("Child(value=2)"), "{updated}");
            }
            // Without `global` the body binds a name only it can see, the
            // import still stands, and the structural base contributes nothing
            // beyond what the subclass writes itself.
            for kept in [format!("class Holder:\n    {base} = Base\n"), String::new()] {
                let updated = fixed(&format!("{head}{kept}{tail}"))?;
                assert!(updated.contains("Child(value=2)"), "{updated}");
            }
        }
        Ok(())
    }

    #[test]
    fn a_twice_nested_global_rebinding_drops_a_structural_base_alias() -> Result<(), String> {
        // `global` sends a binding to the module namespace from however deep
        // in the file it is written, so the import the name arrived under is
        // gone once the body runs. A body one scope down is reached after its
        // enclosing body has saved the alias table it puts back on the way
        // out, so the import used to reappear and `Child()` was rewritten as a
        // subclass of a structural base, without the fields the rebinding
        // brought with it.
        for (imported, base) in [
            ("from typing import Protocol", "Protocol"),
            ("from abc import ABC", "ABC"),
        ] {
            let head = format!(
                "from dataclasses import dataclass\n{imported}\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\n"
            );
            let tail =
                format!("\n@dataclass\nclass Child({base}):\n    value: int = 2\n\nChild()\n");
            for rebind in [
                format!(
                    "def outer():\n    class Holder:\n        global {base}\n        {base} = Base\n\nouter()\n"
                ),
                format!(
                    "def outer():\n    def inner():\n        global {base}\n        {base} = Base\n\n    inner()\n\nouter()\n"
                ),
                format!(
                    "class Holder:\n    def inner():\n        global {base}\n        {base} = Base\n\nHolder.inner()\n"
                ),
                format!(
                    "class Holder:\n    class Inner:\n        global {base}\n        {base} = Base\n"
                ),
            ] {
                let updated = fixed(&format!("{head}{rebind}{tail}"))?;
                assert!(!updated.contains("Child(value=2)"), "{updated}");
            }
            // The same nesting without `global` binds a name only the inner
            // body can see, so the import stands and the structural base still
            // contributes nothing of its own.
            for kept in [
                format!("def outer():\n    class Holder:\n        {base} = Base\n\nouter()\n"),
                String::new(),
            ] {
                let updated = fixed(&format!("{head}{kept}{tail}"))?;
                assert!(updated.contains("Child(value=2)"), "{updated}");
            }
        }
        Ok(())
    }

    #[test]
    fn an_annotation_does_not_lose_a_live_shape_alias() -> Result<(), String> {
        // A bare annotation binds nothing, and an annotated assignment names
        // its value just as a plain one does, so both leave `Alias` describing
        // `Base` and its field reaches the subclass constructor. Only a real
        // rebinding takes those fields away.
        for (middle, inherits) in [
            ("", true),
            ("Alias: object\n", true),
            ("Alias: object = Base\n", true),
            ("Alias = object\n", false),
            ("Alias: object = object\n", false),
        ] {
            let source = format!(
                "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n{middle}\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n"
            );
            let updated = fixed(&source)?;
            assert_eq!(
                updated.contains("Child(inherited=1, value=2)"),
                inherits,
                "{middle}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_annotation_only_global_leaves_the_module_name_standing() -> Result<(), String> {
        // `global Alias` followed by `Alias: int` declares a type and assigns
        // nothing, so the module name still holds what it was imported or
        // assigned as. Reading the declaration as a rebinding dropped the
        // alias, the shape and the flag, leaving the subclass below with a
        // base the file could no longer describe.
        let structural_head = "from dataclasses import dataclass\nfrom typing import Protocol\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\n";
        let structural_tail =
            "\n@dataclass\nclass Child(Protocol):\n    value: int = 2\n\nChild()\n";
        let shape_head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n";
        let shape_tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        let flag_head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nFLAG = True\n";
        let flag_tail = "\nif FLAG:\n    Alias = Base\nelse:\n    Alias = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for declaration in [
            "def helper():\n    global Protocol\n    Protocol: int\n",
            "class Holder:\n    global Protocol\n    Protocol: int\n",
        ] {
            let updated = fixed(&format!("{structural_head}{declaration}{structural_tail}"))?;
            assert!(updated.contains("Child(value=2)"), "{updated}");
        }
        for declaration in [
            "def helper():\n    global Alias\n    Alias: int\n",
            "class Holder:\n    global Alias\n    Alias: int\n",
        ] {
            let updated = fixed(&format!("{shape_head}{declaration}{shape_tail}"))?;
            assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        }
        for declaration in [
            "def helper():\n    global FLAG\n    FLAG: int\n",
            "class Holder:\n    global FLAG\n    FLAG: int\n",
        ] {
            let updated = fixed(&format!("{flag_head}{declaration}{flag_tail}"))?;
            assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        }
        // An annotated assignment in the same place does put something behind
        // the name, so the calls above are left standing by the missing value
        // rather than by the declaration being nested or `global` doing
        // nothing at all.
        for rebind in [
            "def helper():\n    global Protocol\n    Protocol: object = Base\n\nhelper()\n",
            "class Holder:\n    global Protocol\n    Protocol: object = Base\n",
        ] {
            let updated = fixed(&format!("{structural_head}{rebind}{structural_tail}"))?;
            assert!(!updated.contains("Child(value=2)"), "{updated}");
        }
        for rebind in [
            "def helper():\n    global Alias\n    Alias: object = object\n\nhelper()\n",
            "class Holder:\n    global Alias\n    Alias: object = object\n",
        ] {
            let updated = fixed(&format!("{shape_head}{rebind}{shape_tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_lambda_walrus_leaves_the_enclosing_name_standing() -> Result<(), String> {
        // A walrus in a lambda body binds in the lambda's own scope, which
        // nothing outside the lambda reads. Treating it as a module rebinding
        // threw away the shape and the flag the module name really holds, and
        // the subclass below lost the base it was written against.
        let shape_head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n";
        let shape_tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        let flag_head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nFLAG = True\n";
        let flag_tail = "\nif FLAG:\n    Alias = Base\nelse:\n    Alias = object\n\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        let updated = fixed(&format!(
            "{shape_head}handler = lambda: (Alias := object)\n{shape_tail}"
        ))?;
        assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        let updated = fixed(&format!(
            "{flag_head}handler = lambda: (FLAG := False)\n{flag_tail}"
        ))?;
        assert!(updated.contains("Child(inherited=1, value=2)"), "{updated}");
        // A lambda's parameter defaults are evaluated where the lambda is
        // written, not in its scope, so a walrus among them does rebind. The
        // same walrus written as a statement does too, so the lambda body is
        // what spares the name above.
        for rebind in [
            "handler = lambda value=(Alias := object): value\n",
            "print(Alias := object)\n",
        ] {
            let updated = fixed(&format!("{shape_head}{rebind}{shape_tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{updated}");
        }
        Ok(())
    }

    #[test]
    fn a_definition_header_walrus_rebinds_the_enclosing_name() -> Result<(), String> {
        // Decorators, parameter defaults, annotations and bases are evaluated
        // where the `def` or `class` is written, before the scope it opens
        // exists. Recording a walrus among them against that scope left the
        // module shape standing, and the subclass below was constructed with
        // fields the rebound base no longer has.
        let shape_head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n\ndef deco(value):\n    return lambda target: target\n";
        let shape_tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for header in [
            "def helper(value=(Alias := object)):\n    pass\n",
            "def helper(value: (Alias := object) = 1):\n    pass\n",
            "def helper() -> (Alias := object):\n    pass\n",
            "@deco(Alias := object)\ndef helper():\n    pass\n",
            "@deco(Alias := object)\nclass Holder:\n    pass\n",
            "class Holder((Alias := object)):\n    pass\n",
        ] {
            let updated = fixed(&format!("{shape_head}{header}{shape_tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{header}\n{updated}");
        }
        // The same walrus in the body binds a name only that body can see, so
        // the module alias still stands and the fields come through.
        for body in [
            "def helper():\n    print(Alias := object)\n",
            "class Holder:\n    print(Alias := object)\n",
            "",
        ] {
            let updated = fixed(&format!("{shape_head}{body}{shape_tail}"))?;
            assert!(
                updated.contains("Child(inherited=1, value=2)"),
                "{body}\n{updated}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_header_lambda_default_walrus_rebinds_the_enclosing_name() -> Result<(), String> {
        // A lambda written in a `def` or `class` header keeps its body to
        // itself, but the defaults of its parameters are evaluated where the
        // lambda is, which is the enclosing namespace. Stopping at the lambda
        // while collecting what a header rebinds left the alias table saved on
        // the way into the definition holding the old name, and the table put
        // back on the way out brought it with it, so the subclass below was
        // built on a base the rebinding had taken away.
        let shape_head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n\ndef deco(value):\n    return lambda target: target\n\ndef pick(value):\n    return object\n";
        let shape_tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for header in [
            "def helper(value=(lambda inner=(Alias := object): inner)):\n    pass\n",
            "def helper() -> (lambda inner=(Alias := object): inner):\n    pass\n",
            "@deco(lambda inner=(Alias := object): inner)\ndef helper():\n    pass\n",
            "@deco(lambda inner=(Alias := object): inner)\nclass Holder:\n    pass\n",
            "class Holder(pick(lambda inner=(Alias := object): inner)):\n    pass\n",
        ] {
            let updated = fixed(&format!("{shape_head}{header}{shape_tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{header}\n{updated}");
        }
        // A structural base is imported rather than assigned, so what the
        // header takes away is the import itself.
        let structural_head = "from dataclasses import dataclass\nfrom typing import Protocol\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\ndef deco(value):\n    return lambda target: target\n";
        let structural_tail =
            "\n@dataclass\nclass Child(Protocol):\n    value: int = 2\n\nChild()\n";
        for header in [
            "def helper(value=(lambda inner=(Protocol := Base): inner)):\n    pass\n",
            "@deco(lambda inner=(Protocol := Base): inner)\ndef helper():\n    pass\n",
            "@deco(lambda inner=(Protocol := Base): inner)\nclass Holder:\n    pass\n",
        ] {
            let updated = fixed(&format!("{structural_head}{header}{structural_tail}"))?;
            assert!(!updated.contains("Child(value=2)"), "{header}\n{updated}");
        }
        // The lambda's body is another scope, so a walrus there leaves the
        // enclosing name standing, as does a header with nothing rebound in
        // it at all.
        for header in [
            "@deco(lambda: (Alias := object))\ndef helper():\n    pass\n",
            "def helper(value=(lambda: (Alias := object))):\n    pass\n",
            "",
        ] {
            let updated = fixed(&format!("{shape_head}{header}{shape_tail}"))?;
            assert!(
                updated.contains("Child(inherited=1, value=2)"),
                "{header}\n{updated}"
            );
        }
        let updated = fixed(&format!("{structural_head}{structural_tail}"))?;
        assert!(updated.contains("Child(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn constructor_aliases_shadow_the_inherited_constructor() -> Result<(), String> {
        // ``__init__ = setup`` makes ``setup`` the constructor, so ``Child()``
        // takes that method's parameters rather than the ones the base's
        // ``__init__`` was left with. Rewriting it against the base would call
        // ``setup`` with a keyword it does not accept.
        let aliased = "class Base:\n    def __init__(self, parent=1):\n        self.value = parent\n\nclass Child(Base):\n    def setup(self):\n        self.value = 2\n    __init__ = setup\n\nassert Child().value == 2\n";
        assert_eq!(
            fixed(aliased)?,
            "class Base:\n    def __init__(self, parent):\n        self.value = parent\n\nclass Child(Base):\n    def setup(self):\n        self.value = 2\n    __init__ = setup\n\nassert Child().value == 2\n"
        );
        // A subclass that binds no constructor of its own still inherits the
        // base's, so its calls are rewritten against it.
        let inherited = "class Base:\n    def __init__(self, parent=1):\n        self.value = parent\n\nclass Child(Base):\n    pass\n\nassert Child().value == 1\n";
        assert_eq!(
            fixed(inherited)?,
            "class Base:\n    def __init__(self, parent):\n        self.value = parent\n\nclass Child(Base):\n    pass\n\nassert Child(parent=1).value == 1\n"
        );
        Ok(())
    }

    /// Fix `source` the way `--fix` does, without insisting that nothing is
    /// left to report afterwards.
    ///
    /// A constructor alias keeps the defaults its implementation declares, so
    /// the diagnostic for them outlives the fix that `fixed` requires to be
    /// cleared.
    fn fixed_with_retained_defaults(source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(&path, source).map_err(|error| error.to_string())?;
        let files = [path.clone()];
        let checked = check_file(
            &path,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        let mut edits = call_site_edits(&files, checked.signatures)?.edits;
        for diagnostic in &checked.diagnostics {
            if let Some(range) = diagnostic.fix {
                edits
                    .entry(diagnostic.path.clone())
                    .or_default()
                    .push(Edit::deletion(range));
            }
        }
        let mut updated = 0;
        let mut unfixed = BTreeSet::new();
        write_fixes_atomically(fixed_sources(edits, &mut updated, &mut unfixed)?)?;
        std::fs::read_to_string(&path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_retained_alias_default_leaves_the_constructor_call_alone() -> Result<(), String> {
        // The implementation behind `__init__ = setup` keeps its default,
        // which records no signature for the call site to be rewritten
        // against. The base's `__init__` must not stand in for the one that
        // is missing: `setup` never takes `parent`.
        let source = "class Base:\n    def __init__(self, parent=1):\n        self.value = parent\n\nclass Child(Base):\n    def setup(self, own=2):\n        self.value = own\n    __init__ = setup\n\nassert Child().value == 2\n";
        assert_eq!(
            fixed_with_retained_defaults(source)?,
            source.replace("def __init__(self, parent=1)", "def __init__(self, parent)")
        );
        Ok(())
    }

    #[test]
    fn a_bare_module_annotation_leaves_super_the_builtin() -> Result<(), String> {
        // Nothing stands behind ``super`` here, so the call in ``run`` still
        // reaches the builtin and the inherited default has to travel with it.
        // Removing ``value`` while leaving the call alone would have left a
        // live call short of an argument.
        let annotated = "super: object\n\nclass Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self): return super().target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(annotated)?,
            "super: object\n\nclass Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self): return super().target(value=1)\n\nassert Child().run() == 1\n"
        );
        // An assignment does put something else behind the name, and then the
        // call reaches that instead of the inherited method.
        let assigned = "class Other:\n    def target(self): return 9\n\nclass Base:\n    def target(self, value=1): return value\n\nsuper = Other\n\nclass Child(Base):\n    def run(self): return super().target()\n\nassert Child().run() == 9\n";
        assert_eq!(
            fixed(assigned)?,
            "class Other:\n    def target(self): return 9\n\nclass Base:\n    def target(self, value): return value\n\nsuper = Other\n\nclass Child(Base):\n    def run(self): return super().target()\n\nassert Child().run() == 9\n"
        );
        Ok(())
    }

    #[test]
    fn an_aliased_post_init_keeps_its_defaults() {
        // `@dataclass` finds the hook under the name it is bound to, so
        // aliasing an ordinary method to `__post_init__` makes the generated
        // `__init__` call that method with no arguments. There is no call site
        // to carry the default to, so it has to stay.
        let aliased = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int\n\n    def setup(self, extra=1):\n        self.extra = extra\n\n    __post_init__ = setup\n\nC(5)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            aliased,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
        // Without the alias nothing calls the method implicitly, so the same
        // default is removed as usual.
        let unaliased = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int\n\n    def setup(self, extra=1):\n        self.extra = extra\n\nC(5)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            unaliased,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn a_class_nested_in_a_method_does_not_claim_its_receiver() -> Result<(), String> {
        // A class defined in a method body is pushed onto the class stack
        // while the method's `self` or `cls` is still in view, so a receiver
        // used from inside that class stands for the outer class rather than
        // the one being defined.
        for receiver in ["type(self)()", "self.__class__()"] {
            let source = format!(
                "class Outer:\n    def target(self, value=1):\n        return value\n\n    def run(self):\n        class Inner:\n            def target(self, other=2):\n                return other\n\n            result = {receiver}.target()\n        return Inner.result\n\nassert Outer().run() == 1\n"
            );
            assert_eq!(
                fixed(&source)?,
                format!(
                    "class Outer:\n    def target(self, value):\n        return value\n\n    def run(self):\n        class Inner:\n            def target(self, other):\n                return other\n\n            result = {receiver}.target(value=1)\n        return Inner.result\n\nassert Outer().run() == 1\n"
                )
            );
        }
        let classmethod = "class Outer:\n    def target(self, value=1):\n        return value\n\n    @classmethod\n    def run(cls):\n        class Inner:\n            def target(self, other=2):\n                return other\n\n            result = cls().target()\n        return Inner.result\n\nassert Outer.run() == 1\n";
        assert_eq!(
            fixed(classmethod)?,
            "class Outer:\n    def target(self, value):\n        return value\n\n    @classmethod\n    def run(cls):\n        class Inner:\n            def target(self, other):\n                return other\n\n            result = cls().target(value=1)\n        return Inner.result\n\nassert Outer.run() == 1\n"
        );
        // A method of the nested class brings its own receiver, which does
        // stand for the nested class.
        let own_receiver = "class Outer:\n    def target(self, value=1):\n        return value\n\n    def run(self):\n        class Inner:\n            def target(self, other=2):\n                return other\n\n            @classmethod\n            def build(cls):\n                return cls().target()\n        return Inner.build()\n\nassert Outer().run() == 2\n";
        assert_eq!(
            fixed(own_receiver)?,
            "class Outer:\n    def target(self, value):\n        return value\n\n    def run(self):\n        class Inner:\n            def target(self, other):\n                return other\n\n            @classmethod\n            def build(cls):\n                return cls().target(other=2)\n        return Inner.build()\n\nassert Outer().run() == 2\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_body_shadows_only_with_what_it_leaves_behind() -> Result<(), String> {
        // A `TYPE_CHECKING` block does not run, so `Child` has no `method` of
        // its own and the call still reaches the base's. Treating the block as
        // a shadow left the call as written while the default it needs was
        // removed from under it.
        let guarded = "from typing import TYPE_CHECKING\n\nclass Base:\n    def method(self, value=1): return value\n\nclass Child(Base):\n    if TYPE_CHECKING:\n        def method(self): ...\n\nassert Child().method() == 1\n";
        assert_eq!(
            fixed(guarded)?,
            "from typing import TYPE_CHECKING\n\nclass Base:\n    def method(self, value): return value\n\nclass Child(Base):\n    if TYPE_CHECKING:\n        def method(self): ...\n\nassert Child().method(value=1) == 1\n"
        );
        // An `except ... as` target is deleted when its handler ends, so it
        // is never on the class either.
        let caught = "class Base:\n    def method(self, value=1): return value\n\nclass Child(Base):\n    try:\n        pass\n    except Exception as method:\n        pass\n\nassert Child().method() == 1\n";
        assert_eq!(
            fixed(caught)?,
            "class Base:\n    def method(self, value): return value\n\nclass Child(Base):\n    try:\n        pass\n    except Exception as method:\n        pass\n\nassert Child().method(value=1) == 1\n"
        );
        // An assignment under `global` is made outside the class, and a `del`
        // takes back what the body put there.
        let elsewhere = "class Base:\n    def method(self, value=1): return value\n\ndef other(self): return 9\n\nclass Child(Base):\n    global method\n    method = other\n\nassert Child().method() == 1\n";
        assert_eq!(
            fixed(elsewhere)?,
            "class Base:\n    def method(self, value): return value\n\ndef other(self): return 9\n\nclass Child(Base):\n    global method\n    method = other\n\nassert Child().method(value=1) == 1\n"
        );
        let deleted = "class Base:\n    def method(self, value=1): return value\n\ndef other(self): return 9\n\nclass Child(Base):\n    method = other\n    del method\n\nassert Child().method() == 1\n";
        assert_eq!(
            fixed(deleted)?,
            "class Base:\n    def method(self, value): return value\n\ndef other(self): return 9\n\nclass Child(Base):\n    method = other\n    del method\n\nassert Child().method(value=1) == 1\n"
        );
        // A binding the body does leave behind still shadows, so the call is
        // left alone.
        let bound = "class Base:\n    def method(self, value=1): return value\n\ndef other(self): return 9\n\nclass Child(Base):\n    method = other\n\nassert Child().method() == 9\n";
        assert_eq!(
            fixed(bound)?,
            "class Base:\n    def method(self, value): return value\n\ndef other(self): return 9\n\nclass Child(Base):\n    method = other\n\nassert Child().method() == 9\n"
        );
        Ok(())
    }

    #[test]
    fn a_lambda_owns_the_class_cell_of_the_body_it_is_written_in() -> Result<(), String> {
        // Python hands a lambda written straight in a class body that class's
        // own `__class__` cell, exactly as it does a `def` there, so the
        // nested class the lambda sits in is the one `__class__` names.
        let nested = "class Outer:\n    @staticmethod\n    def target(value=1): return value\n\n    def run(self):\n        class Inner:\n            @staticmethod\n            def target(value=2): return value\n            make = lambda: __class__.target()\n        return Inner.make()\n\nassert Outer().run() == 2\n";
        assert_eq!(
            fixed(nested)?,
            "class Outer:\n    @staticmethod\n    def target(value): return value\n\n    def run(self):\n        class Inner:\n            @staticmethod\n            def target(value): return value\n            make = lambda: __class__.target(value=2)\n        return Inner.make()\n\nassert Outer().run() == 2\n"
        );
        let module_level = "class Top:\n    @staticmethod\n    def target(value=3): return value\n    make = lambda: __class__.target()\n\nassert Top.make() == 3\n";
        assert_eq!(
            fixed(module_level)?,
            "class Top:\n    @staticmethod\n    def target(value): return value\n    make = lambda: __class__.target(value=3)\n\nassert Top.make() == 3\n"
        );
        // A lambda in a method body owns no class of its own, so it still sees
        // the cell of the method holding it.
        let in_method = "class Outer:\n    @staticmethod\n    def target(value=1): return value\n\n    def run(self):\n        make = lambda: __class__.target()\n        return make()\n\nassert Outer().run() == 1\n";
        assert_eq!(
            fixed(in_method)?,
            "class Outer:\n    @staticmethod\n    def target(value): return value\n\n    def run(self):\n        make = lambda: __class__.target(value=1)\n        return make()\n\nassert Outer().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn an_unaliased_imported_super_is_still_the_builtin() -> Result<(), String> {
        // `from builtins import super` binds `super` to the very builtin the
        // bare name reaches, so the call it stands in front of resolves as it
        // would without the import.
        let source = "from builtins import super\n\nclass Base:\n    def method(self, value=1): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return super().method()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "from builtins import super\n\nclass Base:\n    def method(self, value): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return super().method(value=1)\n\nassert Child().run() == 1\n"
        );
        // A name the file binds to something of its own is a real shadow, and
        // the call through it names nothing this pass can follow.
        let shadowed = "def super(): raise SystemExit\n\nclass Base:\n    def method(self, value=1): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return super().method()\n";
        assert_eq!(
            fixed(shadowed)?,
            "def super(): raise SystemExit\n\nclass Base:\n    def method(self, value): return value\n\nclass Child(Base):\n    def run(self):\n        __class__\n        return super().method()\n"
        );
        Ok(())
    }

    #[test]
    fn parameterized_local_bases_carry_default_uncertainty() {
        // `Middle` is built on an import, so whether its fields end in a
        // default is unknown, and a subclass must keep its own defaults to
        // stay constructible. Writing the base as `Middle[int]` names the same
        // class, so it must reach the same conclusion as a bare `Middle`.
        for base in ["Middle[int]", "Middle"] {
            let source = format!(
                "from dataclasses import dataclass\nfrom typing import Generic, TypeVar\nfrom mixins import Mixin\n\nT = TypeVar(\"T\")\n\n@dataclass\nclass Middle(Mixin, Generic[T]):\n    first: int = 1\n\n@dataclass\nclass Child({base}):\n    second: int = 2\n"
            );
            let checked = check_source(
                Path::new("fixture.py"),
                &source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 2, "{base}");
            assert!(
                checked.diagnostics.iter().all(|item| item.fix.is_none()),
                "{base}"
            );
        }
    }

    #[test]
    fn a_same_scope_import_names_a_nested_class_base() -> Result<(), String> {
        // The import and the subclass sit in one function body, where a scope
        // of its own holds the name. The same pair at module level resolves,
        // and so must this one.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let other = directory.path().join("other.py");
        let user = directory.path().join("user.py");
        std::fs::write(
            &other,
            "class Base:\n    def method(self, value=1): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "def outer():\n    from other import Base\n\n    class Child(Base):\n        def run(self): return self.method()\n\n    return Child().run()\n\n\nassert outer() == 1\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[other, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert_eq!(
            updated,
            "def outer():\n    from other import Base\n\n    class Child(Base):\n        def run(self): return self.method(value=1)\n\n    return Child().run()\n\n\nassert outer() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn a_parameter_shadowing_an_imported_base_keeps_child_defaults() {
        // The parameter hides the import rather than revealing what it named,
        // so the base is still one whose fields this file cannot see and may
        // end in a positional default. Removing the child's would leave a
        // field without one behind it, which `dataclasses` rejects outright,
        // and no call could be rewritten to make up for it because the
        // inherited fields are unknown. Field order spares a keyword-only
        // field, which `dataclasses` moves past the `*`, but a metaclass the
        // base brings does not: one that builds the class without arguments
        // reaches `__init__` with none to give, and the default is what stood
        // in for them. That hazard is the same behind the shadow as before it.
        let source = "from dataclasses import dataclass, field\nfrom base import Parent\n\n\ndef build(Parent):\n    @dataclass\n    class Child(Parent):\n        positional: int = 2\n        keyword: int = field(default=3, kw_only=True)\n\n    return Child()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics.iter().all(|item| item.fix.is_none()));
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_nested_class_body_calls_through_the_enclosing_receiver() -> Result<(), String> {
        // A class suite is not a closure scope for the names it binds, but a
        // name it only reads still comes from the function around it, so
        // `self` and `cls` written straight in a nested class body are the
        // enclosing method's receiver rather than anything the nested class
        // holds. Indexing nested classes puts a same-named method of the
        // nested class within reach, and taking that one would supply the
        // wrong default.
        let instance = "class Outer:\n    def target(self, value=1):\n        return value\n\n    def run(self):\n        class Inner:\n            def target(self, value=2):\n                return value\n\n            probe = self.target()\n\n        return Inner.probe\n\nassert Outer().run() == 1\n";
        assert_eq!(
            fixed(instance)?,
            "class Outer:\n    def target(self, value):\n        return value\n\n    def run(self):\n        class Inner:\n            def target(self, value):\n                return value\n\n            probe = self.target(value=1)\n\n        return Inner.probe\n\nassert Outer().run() == 1\n"
        );
        // The class a receiver stands for reaches the nested body the same way
        // however the call spells it out.
        for receiver in ["type(self)", "self.__class__"] {
            let source = format!(
                "class Outer:\n    @staticmethod\n    def target(value=1):\n        return value\n\n    def run(self):\n        class Inner:\n            @staticmethod\n            def target(value=2):\n                return value\n\n            probe = {receiver}.target()\n\n        return Inner.probe\n\nassert Outer().run() == 1\n"
            );
            assert_eq!(
                fixed(&source)?,
                format!(
                    "class Outer:\n    @staticmethod\n    def target(value):\n        return value\n\n    def run(self):\n        class Inner:\n            @staticmethod\n            def target(value):\n                return value\n\n            probe = {receiver}.target(value=1)\n\n        return Inner.probe\n\nassert Outer().run() == 1\n"
                ),
                "{receiver}"
            );
        }
        let class_method = "class Outer:\n    @staticmethod\n    def target(value=1):\n        return value\n\n    @classmethod\n    def run(cls):\n        class Inner:\n            @staticmethod\n            def target(value=2):\n                return value\n\n            probe = cls.target()\n\n        return Inner.probe\n\nassert Outer.run() == 1\n";
        assert_eq!(
            fixed(class_method)?,
            "class Outer:\n    @staticmethod\n    def target(value):\n        return value\n\n    @classmethod\n    def run(cls):\n        class Inner:\n            @staticmethod\n            def target(value):\n                return value\n\n            probe = cls.target(value=1)\n\n        return Inner.probe\n\nassert Outer.run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn a_function_local_class_holds_a_base_name_against_an_import() -> Result<(), String> {
        // The class written in the function body takes `Helper` over from the
        // import for the rest of that body, so `Child` is built on the local
        // class and `super().target()` reaches the parameter it declares. The
        // imported class of the same name is never the base here, and passing
        // the parameter it declares would raise `TypeError`.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let other = directory.path().join("other.py");
        let user = directory.path().join("user.py");
        std::fs::write(
            &other,
            "class Helper:\n    def target(self, imported=1): return imported\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &user,
            "from other import Helper\n\n\ndef outer():\n    class Helper:\n        def target(self, local=2): return local\n\n    class Child(Helper):\n        def run(self):\n            return super().target()\n\n    return Child().run()\n\n\nassert outer() == 2\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[other, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert!(updated.contains("super().target(local=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_parameter_shadowing_an_import_leaves_a_nested_class_base_unknown() -> Result<(), String> {
        // The parameter holds the name for the whole call, so the class the
        // subclass is built on is whatever the caller passed rather than the
        // import above. Reading the import as the base would rewrite the
        // inherited call with a default the runtime base never had, silently
        // changing what the program returns.
        // `a_same_scope_import_names_a_nested_class_base` covers the same
        // shape without the parameter, where the import is the base and the
        // call is rewritten.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let other = directory.path().join("other.py");
        let runtime = directory.path().join("runtime.py");
        let user = directory.path().join("user.py");
        std::fs::write(
            &other,
            "class Base:\n    def method(self, value=1): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &runtime,
            "class Runtime:\n    def method(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        let source = "from other import Base\nfrom runtime import Runtime\n\n\ndef outer(Base):\n    class Child(Base):\n        def run(self): return self.method()\n\n    return Child().run()\n\n\nassert outer(Runtime) == 2\n";
        std::fs::write(&user, source).map_err(|error| error.to_string())?;
        fix_all(&[other, runtime, user.clone()])?;
        let updated = std::fs::read_to_string(&user).map_err(|error| error.to_string())?;
        assert_eq!(updated, source);
        Ok(())
    }

    /// Fix a function body written against a `Runtime` class in another file,
    /// and report what the body became.
    fn fixed_against_runtime_class(user_source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let runtime = directory.path().join("runtime.py");
        let user = directory.path().join("user.py");
        std::fs::write(
            &runtime,
            "class Runtime:\n    def method(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(&user, user_source).map_err(|error| error.to_string())?;
        fix_all(&[runtime, user.clone()])?;
        std::fs::read_to_string(&user).map_err(|error| error.to_string())
    }

    #[test]
    fn a_parameter_holds_its_name_against_a_later_class_of_the_same_spelling() -> Result<(), String>
    {
        // The class comes after the subclass, so the name the subclass was
        // built on was still the parameter's. Dropping the parameter's name
        // rather than holding it would leave the later class free to answer
        // to it, and the inherited call would be rewritten with a default
        // that belongs to a class the subclass never had.
        let source = "from runtime import Runtime\n\n\ndef outer(Helper):\n    class Child(Helper):\n        def run(self): return self.method()\n\n    class Helper:\n        def method(self, value=3): return value\n\n    return Child().run()\n\n\nassert outer(Runtime) == 2\n";
        assert_eq!(
            fixed_against_runtime_class(source)?,
            "from runtime import Runtime\n\n\ndef outer(Helper):\n    class Child(Helper):\n        def run(self): return self.method()\n\n    class Helper:\n        def method(self, value): return value\n\n    return Child().run()\n\n\nassert outer(Runtime) == 2\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_written_before_a_subclass_takes_the_name_from_a_parameter() -> Result<(), String> {
        // The other order, where the class has replaced the parameter by the
        // time the subclass is written and is what it inherits from. Holding
        // the parameter's name must not reach this far, or the base would go
        // unresolved and the call would be left alone.
        let source = "from runtime import Runtime\n\n\ndef outer(Helper):\n    class Helper:\n        def method(self, value=3): return value\n\n    class Child(Helper):\n        def run(self): return self.method()\n\n    return Child().run()\n\n\nassert outer(Runtime) == 3\n";
        assert_eq!(
            fixed_against_runtime_class(source)?,
            "from runtime import Runtime\n\n\ndef outer(Helper):\n    class Helper:\n        def method(self, value): return value\n\n    class Child(Helper):\n        def run(self): return self.method(value=3)\n\n    return Child().run()\n\n\nassert outer(Runtime) == 3\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_in_a_function_leaves_a_module_level_namesake_its_bases() -> Result<(), String> {
        // Nothing that names a class puts the function holding it in the name,
        // so a class written in a function body is spelled exactly as one
        // written outside one. Recording the inner class's bases under that
        // name would replace the outer class's ancestry with an empty one, and
        // the inherited call in its body would then be left as written while
        // the default behind it was removed.
        let source = "class Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self): return self.target()\n\ndef outer():\n    class Child:\n        def target(self): return 0\n    return Child().target()\n\nassert Child().run() == 1\nassert outer() == 0\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self): return self.target(value=1)\n\ndef outer():\n    class Child:\n        def target(self): return 0\n    return Child().target()\n\nassert Child().run() == 1\nassert outer() == 0\n"
        );
        Ok(())
    }

    #[test]
    fn a_module_level_rebinding_takes_an_imported_super_back() -> Result<(), String> {
        // The module body runs in full before any method it defines is called,
        // so a `def`, a `class`, or an assignment that takes the name over
        // leaves the import behind. A call through the name reaches whatever
        // replaced it, and the class the file is written in says nothing about
        // what it returns.
        for rebinding in [
            "def super():\n    return Other()\n",
            "class super:\n    def __new__(cls, *args):\n        return Other()\n",
        ] {
            let source = format!(
                "from builtins import super\n\nclass Base:\n    def target(self, value=1): return value\n\nclass Other:\n    def target(self): return 2\n\n{rebinding}\nclass Child(Base):\n    def run(self):\n        return super().target()\n\nassert Child().run() == 2\n"
            );
            let expected = format!(
                "from builtins import super\n\nclass Base:\n    def target(self, value): return value\n\nclass Other:\n    def target(self): return 2\n\n{rebinding}\nclass Child(Base):\n    def run(self):\n        return super().target()\n\nassert Child().run() == 2\n"
            );
            assert_eq!(fixed(&source)?, expected, "{rebinding}");
        }
        // The import alone still names the builtin, so a zero-argument call
        // through it resolves as one written as `super`.
        let untouched = "from builtins import super\n\nclass Base:\n    def target(self, value=1): return value\n\nclass Child(Base):\n    def run(self):\n        return super().target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(untouched)?,
            "from builtins import super\n\nclass Base:\n    def target(self, value): return value\n\nclass Child(Base):\n    def run(self):\n        return super().target(value=1)\n\nassert Child().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_in_a_control_flow_suite_holds_no_name_it_is_not_indexed_under() -> Result<(), String>
    {
        // The walk over a file descends into class bodies and function bodies
        // alone, so a class written in an `if`, `try`, `for`, or `with` suite
        // is never recorded. Holding its name against a class in a function
        // body would leave both of them unindexed, and the inherited call in
        // the one that is written would be left as written while the default
        // behind it was removed.
        let body = "def outer():\n    class Base:\n        def target(self, value=2): return value\n\n    class Helper(Base):\n        def run(self): return self.target()\n\n    return Helper().run()\n\n\nassert outer() == 2\n";
        let fixed_body = "def outer():\n    class Base:\n        def target(self, value): return value\n\n    class Helper(Base):\n        def run(self): return self.target(value=2)\n\n    return Helper().run()\n\n\nassert outer() == 2\n";
        for suite in [
            "if True:\n    class Helper:\n        def unrelated(self): return 0\n",
            "try:\n    class Helper:\n        def unrelated(self): return 0\nexcept Exception:\n    pass\n",
            "for _ in range(1):\n    class Helper:\n        def unrelated(self): return 0\n",
        ] {
            assert_eq!(
                fixed(&format!("{suite}\n\n{body}"))?,
                format!("{suite}\n\n{fixed_body}"),
                "{suite}"
            );
        }
        Ok(())
    }

    #[test]
    fn inherited_aliases_reach_a_class_named_in_the_scope_around_them() -> Result<(), String> {
        // `alias = Base.target` names the class beside the one being
        // collected, which a function body identifies by the scopes holding
        // it. Naming it as though it sat at module level finds nothing, so the
        // default comes off `target` with the alias call left as written, or
        // finds a namesake of another scope and passes that one's default.
        let local = "def outer():\n    class Base:\n        def target(self, value=1): return value\n\n    class Child(Base):\n        alias = Base.target\n\n        def run(self): return self.alias()\n\n    return Child().run()\n\nassert outer() == 1\n";
        assert_eq!(
            fixed(local)?,
            "def outer():\n    class Base:\n        def target(self, value): return value\n\n    class Child(Base):\n        alias = Base.target\n\n        def run(self): return self.alias(value=1)\n\n    return Child().run()\n\nassert outer() == 1\n"
        );
        // The module-level namesake is a different class, and taking its
        // default would return `2` where Python returns `1`.
        let shadowed = "class Base:\n    def target(self, value=2): return value\n\ndef outer():\n    class Base:\n        def target(self, value=1): return value\n\n    class Child(Base):\n        alias = Base.target\n\n        def run(self): return self.alias()\n\n    return Child().run()\n\nassert outer() == 1\n";
        assert_eq!(
            fixed(shadowed)?,
            "class Base:\n    def target(self, value): return value\n\ndef outer():\n    class Base:\n        def target(self, value): return value\n\n    class Child(Base):\n        alias = Base.target\n\n        def run(self): return self.alias(value=1)\n\n    return Child().run()\n\nassert outer() == 1\n"
        );
        // A method body is such a scope too, and the class it holds is told
        // apart from one written straight in the class around it.
        let in_method = "class Outer:\n    class Base:\n        def target(self, value=2): return value\n\n    def build(self):\n        class Base:\n            def target(self, value=1): return value\n\n        class Child(Base):\n            alias = Base.target\n\n            def run(self): return self.alias()\n\n        return Child().run()\n\nassert Outer().build() == 1\n";
        assert_eq!(
            fixed(in_method)?,
            "class Outer:\n    class Base:\n        def target(self, value): return value\n\n    def build(self):\n        class Base:\n            def target(self, value): return value\n\n        class Child(Base):\n            alias = Base.target\n\n            def run(self): return self.alias(value=1)\n\n        return Child().run()\n\nassert Outer().build() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn same_scope_local_bases_shadow_imported_classes() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &api,
            "class Base:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "from api import Base\n\ndef outer():\n    class Base:\n        def target(self, value=1): return value\n    class Child(Base):\n        def run(self): return self.target()\n    return Child().run()\n\nassert outer() == 1\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, case.clone()])?;
        assert_eq!(
            std::fs::read_to_string(case).map_err(|error| error.to_string())?,
            "from api import Base\n\ndef outer():\n    class Base:\n        def target(self, value): return value\n    class Child(Base):\n        def run(self): return self.target(value=1)\n    return Child().run()\n\nassert outer() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn imported_base_default_uncertainty_propagates_through_subscripted_bases() {
        let source = "from dataclasses import dataclass\nfrom typing import Generic, TypeVar\nfrom base import Parent\n\nT = TypeVar('T')\n\n@dataclass\nclass Middle(Parent, Generic[T]):\n    middle: int = 2\n\n@dataclass\nclass Child(Middle[T]):\n    child: int = 3\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics.iter().all(|item| item.fix.is_none()));
    }

    #[test]
    fn an_import_stays_the_base_of_a_subclass_written_above_a_local_class() -> Result<(), String> {
        // `Child` is written while `Helper` still names the import, so that is
        // the class it inherits from however thoroughly a class further down
        // takes the name over. Reading the whole suite for local classes
        // without regard for where each is written would rewrite the
        // inherited call with a default the base never had.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &api,
            "class Helper:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "from api import Helper\n\n\nclass Child(Helper):\n    def run(self): return self.target()\n\n\nclass Helper:\n    def target(self, value=2): return value\n\n\nassert Child().run() == 9\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, case.clone()])?;
        assert_eq!(
            std::fs::read_to_string(case).map_err(|error| error.to_string())?,
            "from api import Helper\n\n\nclass Child(Helper):\n    def run(self): return self.target(value=9)\n\n\nclass Helper:\n    def target(self, value): return value\n\n\nassert Child().run() == 9\n"
        );
        Ok(())
    }

    #[test]
    fn control_flow_classes_preserve_inherited_method_calls() -> Result<(), String> {
        let source = "if True:\n    class Base:\n        def target(self, value=1): return value\n    class Child(Base):\n        def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "if True:\n    class Base:\n        def target(self, value): return value\n    class Child(Base):\n        def run(self): return self.target(value=1)\n\nassert Child().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn lexical_attribute_bases_preserve_inherited_method_calls() -> Result<(), String> {
        let source = "def outer():\n    class Namespace:\n        class Base:\n            def target(self, value=1): return value\n    class Child(Namespace.Base):\n        def run(self): return self.target()\n    return Child().run()\n\nassert outer() == 1\n";
        assert_eq!(
            fixed(source)?,
            "def outer():\n    class Namespace:\n        class Base:\n            def target(self, value): return value\n    class Child(Namespace.Base):\n        def run(self): return self.target(value=1)\n    return Child().run()\n\nassert outer() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_does_not_shadow_its_own_imported_base() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &api,
            "class Base:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "from api import Base\n\nclass Base(Base):\n    def run(self): return self.target()\n\nassert Base().run() == 9\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, case.clone()])?;
        assert_eq!(
            std::fs::read_to_string(case).map_err(|error| error.to_string())?,
            "from api import Base\n\nclass Base(Base):\n    def run(self): return self.target(value=9)\n\nassert Base().run() == 9\n"
        );
        Ok(())
    }

    #[test]
    fn aliased_namespace_attribute_bases_preserve_inherited_method_calls() -> Result<(), String> {
        // The dotted spelling of a nested class is not the only way to reach
        // it: a name bound to the class around it names the same member, and
        // the spelling `NS.Base` matches no class this file writes.
        for source in [
            "class Namespace:\n    class Base:\n        def target(self, value=1): return value\n\nNS = Namespace\n\nclass Child(NS.Base):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n",
            "class Namespace:\n    class Inner:\n        class Base:\n            def target(self, value=1): return value\n\nNS = Namespace.Inner\n\nclass Child(NS.Base):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n",
        ] {
            assert_eq!(
                fixed(source)?,
                source
                    .replace("value=1): return value", "value): return value")
                    .replace("self.target()", "self.target(value=1)")
            );
        }
        Ok(())
    }

    #[test]
    fn an_unseen_imported_ancestor_protects_a_local_subclass() {
        // The metaclass an imported base may carry builds every class beneath
        // it, not only the one that names it, so a local class standing
        // between the two hides nothing. Removing the subclass's default would
        // leave that metaclass reaching `__init__` with no argument to stand
        // in for it, and no call could be rewritten to make up for it because
        // the inherited fields are unknown.
        for source in [
            "from dataclasses import dataclass, field\nfrom other import Base\n\nclass Middle(Base):\n    pass\n\n@dataclass\nclass Child(Middle):\n    keyword: int = field(default=3, kw_only=True)\n",
            "from dataclasses import dataclass, field\nfrom other import Base\n\nclass Middle(Base):\n    pass\n\nclass Inner(Middle):\n    pass\n\n@dataclass\nclass Child(Inner):\n    keyword: int = field(default=3, kw_only=True)\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1);
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
            assert!(checked.signatures.is_empty(), "{source}");
        }
    }

    #[test]
    fn a_class_body_does_not_rewrite_its_own_header_bases() {
        // Whether a base is an unseen import is settled where the header is
        // written, not where the body ends. A member binding the base's name
        // rebinds it for readers of the class, never for the header above it,
        // and a base spelled with the class's own name still reaches whatever
        // that name held before the class did. Reading either after the body
        // would drop the mark that protects a later subclass, or invent one
        // over a base that carries no fields at all.
        for (source, removable) in [
            ("from dataclasses import dataclass, field\nfrom other import Base\n\nclass Middle(Base):\n    Base = 1\n\n@dataclass\nclass Child(Middle):\n    keyword: int = field(default=3, kw_only=True)\n", false),
            ("from dataclasses import dataclass, field\nfrom other import Base\n\nclass Middle(Base):\n    class Base:\n        pass\n\n@dataclass\nclass Child(Middle):\n    keyword: int = field(default=3, kw_only=True)\n", false),
            ("from dataclasses import dataclass, field\nfrom typing import Protocol\n\nclass Protocol(Protocol):\n    pass\n\n@dataclass\nclass Child(Protocol):\n    keyword: int = field(default=3, kw_only=True)\n", true),
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert_eq!(checked.diagnostics[0].fix.is_some(), removable, "{source}");
            assert_eq!(checked.signatures.is_empty(), !removable, "{source}");
        }
    }
    #[test]
    fn control_flow_classes_see_enclosing_suite_bases() -> Result<(), String> {
        let source = "class Base:\n    def target(self, value=1): return value\n\nif True:\n    class Child(Base):\n        def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            "class Base:\n    def target(self, value): return value\n\nif True:\n    class Child(Base):\n        def run(self): return self.target(value=1)\n\nassert Child().run() == 1\n"
        );
        Ok(())
    }

    #[test]
    fn suites_that_disagree_on_a_class_leave_its_inherited_calls_alone() -> Result<(), String> {
        // Both suites define `Child`, and only one of them runs. Rewriting the
        // call against either suite's base would hand the class the module
        // actually built the other one's default.
        let source = "import os\n\nclass BaseA:\n    def target(self, value=1): return value\n\nclass BaseB:\n    def target(self, value=2): return value\n\nif os.environ.get('PICK'):\n    class Child(BaseA):\n        def run(self): return self.target()\nelse:\n    class Child(BaseB):\n        def run(self): return self.target()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("self.target(value="), "{updated}");
        Ok(())
    }

    #[test]
    fn a_suite_that_cannot_run_leaves_the_base_of_the_one_that_can() -> Result<(), String> {
        let source = "class BaseA:\n    def target(self, value=1): return value\n\nclass BaseB:\n    def target(self, value=2): return value\n\nif True:\n    class Child(BaseA):\n        def run(self): return self.target()\nelse:\n    class Child(BaseB):\n        def run(self): return self.target()\n\nassert Child().run() == 1\n";
        let updated = fixed(source)?;
        assert!(updated.contains("self.target(value=1)"), "{updated}");
        assert!(!updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn suites_that_hold_both_a_class_and_its_base_still_disagree() -> Result<(), String> {
        let source = "import os\n\nif os.environ.get('PICK'):\n    class BaseA:\n        def target(self, value=1): return value\n    class Child(BaseA):\n        def run(self): return self.target()\nelse:\n    class BaseB:\n        def target(self, value=2): return value\n    class Child(BaseB):\n        def run(self): return self.target()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("self.target(value="), "{updated}");
        Ok(())
    }

    #[test]
    fn match_cases_that_disagree_on_a_class_leave_its_inherited_calls_alone() -> Result<(), String>
    {
        let source = "import os\n\nclass BaseA:\n    def target(self, value=1): return value\n\nclass BaseB:\n    def target(self, value=2): return value\n\nmatch os.environ.get('PICK'):\n    case 'a':\n        class Child(BaseA):\n            def run(self): return self.target()\n    case _:\n        class Child(BaseB):\n            def run(self): return self.target()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("self.target(value="), "{updated}");
        Ok(())
    }
    #[test]
    fn suites_that_disagree_on_a_class_report_its_construction() -> Result<(), String> {
        // `Child()` is spelled with the class's name, not the `__init__` it
        // inherits, so the gate that reports an unresolved call never saw it
        // and the default behind the constructor was dropped with nothing
        // written into the call to stand in for it. A subclass of the
        // contested class is no better placed than the class itself.
        for source in [
            "import os\n\nclass BaseA:\n    def __init__(self, value=1): self.value = value\n\nclass BaseB:\n    def __init__(self, value=2): self.value = value\n\nif os.environ.get('PICK'):\n    class Child(BaseA):\n        pass\nelse:\n    class Child(BaseB):\n        pass\n\nprint(Child().value)\n",
            "import os\n\nclass BaseA:\n    def __init__(self, value=1): self.value = value\n\nclass BaseB:\n    def __init__(self, value=2): self.value = value\n\nif os.environ.get('PICK'):\n    class Child(BaseA):\n        pass\nelse:\n    class Child(BaseB):\n        pass\n\nclass Grandchild(Child):\n    pass\n\nprint(Grandchild().value)\n",
        ] {
            assert_eq!(
                skipped_reasons(source)?,
                ["this call cannot be tied to the definition that was fixed"],
                "{source}"
            );
        }
        Ok(())
    }
    #[test]
    fn suites_that_disagree_on_a_class_report_a_qualified_construction() -> Result<(), String> {
        // `api.Child()` is spelled exactly as a method call, but the callee
        // names a class of the module rather than a method of a receiver. It
        // resolves and is rewritten when the ancestry is settled, so it has to
        // be held back when it is not.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let user = directory.path().join("user.py");
        let api_source = "import os\n\nclass BaseA:\n    def __init__(self, value=1): self.value = value\n\nclass BaseB:\n    def __init__(self, value=2): self.value = value\n\nif os.environ.get('PICK'):\n    class Child(BaseA):\n        pass\nelse:\n    class Child(BaseB):\n        pass\n";
        let user_source = "import api\n\nprint(api.Child().value)\n";
        std::fs::write(&api, api_source).map_err(|error| error.to_string())?;
        std::fs::write(&user, user_source).map_err(|error| error.to_string())?;
        let reasons: Vec<String> = fix_all(&[api, user])?
            .into_iter()
            .map(|skip| skip.reason)
            .collect();
        assert_eq!(
            reasons,
            ["this call cannot be tied to the definition that was fixed"]
        );
        Ok(())
    }

    #[test]
    fn branches_that_disagree_on_a_base_leave_its_inherited_calls_alone() -> Result<(), String> {
        // The bases are settled at module level and only the subclass is
        // written twice, so the disagreement is over `Child` alone. Which
        // `target` it inherits depends on a test the tool cannot read, so the
        // calls that would have named it are reported rather than rewritten.
        // That the defaults behind them survive the fix is decided further on,
        // where `retained` is honoured, so it is covered end to end by
        // `branches_that_disagree_on_a_base_keep_the_inherited_default` in
        // `tests/cli.rs` rather than here.
        let source = "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, value=2): return value\n\nif os.environ.get('PICK'):\n    class Child(First):\n        def run(self): return self.target()\nelse:\n    class Child(Second):\n        def run(self): return self.target()\n\nChild().run()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("self.target(value="), "{updated}");
        assert_eq!(
            skipped_reasons(source)?,
            [
                "this call cannot be tied to the definition that was fixed",
                "this call cannot be tied to the definition that was fixed",
                "this call cannot be tied to the definition that was fixed"
            ]
        );
        Ok(())
    }

    #[test]
    fn a_class_shape_alias_uses_the_nearest_enclosing_scope() -> Result<(), String> {
        // The alias is written in a class body, and the name it reads is a
        // free one there, so Python answers it from the enclosing function
        // before the module. Reading the module first would build `Child` on
        // the outermost `Base` and give it a field the class never has.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    module: int = 1\n\ndef outer():\n    @dataclass\n    class Base:\n        local: int = 2\n\n    class Container:\n        Alias = Base\n\n        @dataclass\n        class Child(Alias):\n            child: int = 3\n\n    return Container.Child()\n\nouter()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        let child = checked
            .signatures
            .iter()
            .find(|signature| signature.positional.iter().any(|field| field == "child"))
            .ok_or("expected the nested child signature")?;
        assert_eq!(child.positional, ["local", "child"]);
        Ok(())
    }

    #[test]
    fn a_nested_class_shape_alias_skips_outer_class_scopes() -> Result<(), String> {
        // `Nested` is written inside `Container`, but a class namespace is not
        // a closure scope: the alias reads straight past the `Base` of the
        // outer class body to the one the enclosing function holds.
        let source = "from dataclasses import dataclass\n\ndef outer():\n    @dataclass\n    class Base:\n        local: int = 1\n\n    class Container:\n        @dataclass\n        class Base:\n            class_body: int = 2\n\n        class Nested:\n            Alias = Base\n\n            @dataclass\n            class Child(Alias):\n                child: int = 3\n\n    return Container.Nested.Child\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        let child = checked
            .signatures
            .iter()
            .find(|signature| signature.positional.iter().any(|field| field == "child"))
            .ok_or("expected the nested child signature")?;
        assert_eq!(child.positional, ["local", "child"]);
        Ok(())
    }

    #[test]
    fn a_parameter_hides_the_enclosing_class_of_the_same_name() -> Result<(), String> {
        // `inner` takes its base as a parameter, so the `Base` the alias reads
        // is that argument and not the class `outer` holds. Walking on to the
        // enclosing class would give `Child` a `local` field the class it is
        // really built on has no room for.
        let source = "from dataclasses import dataclass\n\nclass Fallback:\n    pass\n\ndef outer():\n    @dataclass\n    class Base:\n        local: int = 2\n\n    def inner(Base):\n        Alias = Base\n\n        @dataclass\n        class Child(Alias):\n            child: int = 3\n\n        return Child()\n\n    return inner(Fallback)\n\nouter()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("Child(local="), "{updated}");
        Ok(())
    }

    #[test]
    fn a_rebinding_hides_the_enclosing_class_of_the_same_name() -> Result<(), String> {
        // The rebinding is a call, so what it produces is unknown, but that it
        // happened is not: `Base` in `inner` is whatever the call returned and
        // the enclosing class is out of reach behind it.
        let source = "from dataclasses import dataclass\n\ndef outer():\n    @dataclass\n    class Base:\n        local: int = 2\n\n    def inner():\n        Base = type('Base', (), {})\n        Alias = Base\n\n        @dataclass\n        class Child(Alias):\n            child: int = 3\n\n        return Child()\n\n    return inner()\n\nouter()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("Child(local="), "{updated}");
        Ok(())
    }

    #[test]
    fn a_loop_target_hides_the_enclosing_class_of_the_same_name() -> Result<(), String> {
        // A loop target binds its name for the rest of the body, so the same
        // reasoning holds for it as for a plain assignment.
        let source = "from dataclasses import dataclass\n\nclass Fallback:\n    pass\n\ndef outer():\n    @dataclass\n    class Base:\n        local: int = 2\n\n    def inner():\n        for Base in [Fallback]:\n            pass\n        Alias = Base\n\n        @dataclass\n        class Child(Alias):\n            child: int = 3\n\n        return Child()\n\n    return inner()\n\nouter()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("Child(local="), "{updated}");
        Ok(())
    }

    fn fixed_keeping_unreachable_defaults(source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(&path, source).map_err(|error| error.to_string())?;
        let checked = check_file(
            &path,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        let files = [path.clone()];
        let mut call_sites = call_site_edits(&files, checked.signatures)?;
        for diagnostic in &checked.diagnostics {
            let Some(range) = diagnostic.fix else {
                continue;
            };
            if call_sites
                .retained
                .contains(&fix_key(&diagnostic.path, range))
            {
                continue;
            }
            call_sites
                .edits
                .entry(diagnostic.path.clone())
                .or_default()
                .push(Edit::deletion(range));
        }
        let mut updated = 0;
        let mut unfixed = BTreeSet::new();
        write_fixes_atomically(fixed_sources(call_sites.edits, &mut updated, &mut unfixed)?)?;
        std::fs::read_to_string(&path).map_err(|error| error.to_string())
    }

    #[test]
    fn competing_conditional_class_bases_retain_inherited_defaults() -> Result<(), String> {
        // Leaving the call alone is only half of it: the defaults behind it
        // have to stay too, or the call it could not rewrite reaches a method
        // that no longer has one.
        let source = "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, value=2): return value\n\nif os.environ.get(\"PICK\"):\n    class Child(First):\n        def run(self): return self.target()\nelse:\n    class Child(Second):\n        def run(self): return self.target()\n\nChild().run()\n";
        assert_eq!(fixed_keeping_unreachable_defaults(source)?, source);
        Ok(())
    }

    #[test]
    fn an_ambiguous_ancestor_leaves_a_subclass_method_resolvable() -> Result<(), String> {
        // The subclass's own method does not depend on which base the competing
        // suites gave `Amb`, so its default is removed and the call to it
        // rewritten. Leaving the call bare would have raised `TypeError`.
        let source = "import sys\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, value=2): return value\n\nif sys.argv:\n    class Amb(First):\n        pass\nelse:\n    class Amb(Second):\n        pass\n\nclass Sub(Amb):\n    def own(self, value=5): return value\n    def run(self): return self.own()\n\nassert Sub().run() == 5\n";
        assert_eq!(
            fixed_keeping_unreachable_defaults(source)?,
            source
                .replace("def own(self, value=5)", "def own(self, value)")
                .replace("self.own()", "self.own(value=5)")
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("def target(self, value=2)", "def target(self, value)")
        );
        Ok(())
    }

    #[test]
    fn a_default_an_ambiguous_ancestry_hides_is_kept() -> Result<(), String> {
        // `Leaf` has two bases, so which of `Sub`'s ancestors come before the
        // other is not known once `Amb` is in doubt, and `self.own()` is left
        // alone. The default behind it has to stay for that call to keep
        // working.
        let source = "import sys\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, value=2): return value\n\nclass Mixin:\n    pass\n\nif sys.argv:\n    class Amb(First):\n        pass\nelse:\n    class Amb(Second):\n        pass\n\nclass Sub(Amb):\n    def own(self, value=5): return value\n\nclass Leaf(Sub, Mixin):\n    def run(self): return self.own()\n\nassert Leaf().run() == 5\n";
        assert_eq!(
            fixed_keeping_unreachable_defaults(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("def target(self, value=2)", "def target(self, value)")
        );
        Ok(())
    }

    #[test]
    fn competing_aliases_leave_a_subclass_ancestry_unsettled() -> Result<(), String> {
        // Which suite ran decides which class `Alias` stands for, so `Child`
        // has an ancestry for each and the file settles neither. Resolving the
        // inherited call against either candidate strips the other's default
        // too: with `Second` live, a rewritten `self.target(value=1)` raises
        // `TypeError: Second.target() got an unexpected keyword argument`.
        for source in [
            "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, other=2): return other\n\nif os.environ.get(\"PICK\"):\n    Alias = Second\nelse:\n    Alias = First\n\nclass Child(Alias):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n",
            "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, other=2): return other\n\nif os.environ.get(\"PICK\"):\n    Alias = Second\nelse:\n    Alias = First\n\nif os.environ.get(\"OTHER\"):\n    class Child(Alias):\n        def run(self): return self.target()\n\nassert Child().run() == 1\n",
        ] {
            assert_eq!(fixed_keeping_unreachable_defaults(source)?, source);
        }
        Ok(())
    }

    #[test]
    fn an_alias_a_later_assignment_settles_resolves_a_subclass_ancestry() -> Result<(), String> {
        // The assignment after the suites runs whichever way they went, so the
        // name stands for one class again and the ancestry is known.
        let source = "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, value=2): return value\n\nif os.environ.get(\"PICK\"):\n    Alias = Second\nelse:\n    Alias = First\n\nAlias = First\n\nclass Child(Alias):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed_keeping_unreachable_defaults(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("def target(self, value=2)", "def target(self, value)")
                .replace("self.target()", "self.target(value=1)")
        );
        Ok(())
    }

    #[test]
    fn a_later_unguarded_class_definition_settles_a_contested_ancestry() -> Result<(), String> {
        // The definition after the suites is the one standing when the module
        // is done, whichever suite ran, so its bases are the class's and the
        // inherited call resolves against them.
        let source = "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, value=2): return value\n\nif os.environ.get(\"PICK\"):\n    class Child(First):\n        pass\nelse:\n    class Child(Second):\n        pass\n\nclass Child(First):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed_keeping_unreachable_defaults(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("def target(self, value=2)", "def target(self, value)")
                .replace("self.target()", "self.target(value=1)")
        );
        Ok(())
    }

    #[test]
    fn a_subscripted_settled_alias_resolves_its_inherited_call() -> Result<(), String> {
        // The parameter on the base changes nothing about which class it
        // names, so a settled alias still resolves through it. That the
        // contested spelling of the same shape keeps its defaults is decided
        // where `retained` is honoured, so it is covered end to end by
        // `a_subscripted_contested_base_keeps_the_inherited_default` in
        // `tests/cli.rs` rather than here.
        let source = "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\nclass First(Generic[T]):\n    def target(self, value=1): return value\n\nAlias = First\n\nclass Child(Alias[int]):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("self.target()", "self.target(value=1)")
        );
        Ok(())
    }

    #[test]
    fn a_suite_that_certainly_runs_settles_the_name_it_binds() -> Result<(), String> {
        // The body of `if True:` runs, so `Alias` names `First` afterwards
        // however it was bound above. Reading the earlier binding instead
        // rewrote the call with `Second`'s parameter, which `First` does not
        // take.
        let source = "class First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, other=2): return other\n\nAlias = Second\nif True:\n    Alias = First\n\nclass Child(Alias):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("def target(self, other=2)", "def target(self, other)")
                .replace("self.target()", "self.target(value=1)")
        );
        Ok(())
    }

    #[test]
    fn a_class_shape_alias_does_not_reach_an_enclosing_class_scope() -> Result<(), String> {
        // A class body is not a closure scope, so `Base` beside `Inner` is the
        // module's, not the `Base` written in `Outer` around it.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    module: int = 1\n\nclass Outer:\n    @dataclass\n    class Base:\n        outer: int = 2\n\n    class Inner:\n        Alias = Base\n\n        @dataclass\n        class Child(Alias):\n            child: int = 3\n\nOuter.Inner.Child()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        let child = checked
            .signatures
            .iter()
            .find(|signature| signature.positional.iter().any(|field| field == "child"))
            .ok_or("expected the nested child signature")?;
        assert_eq!(child.positional, ["module", "child"]);
        Ok(())
    }

    #[test]
    fn a_copy_of_a_settled_name_resolves_its_inherited_call() -> Result<(), String> {
        // Nothing is in doubt here, so reading one name from another resolves
        // straight through. That the same shape keeps its defaults once the
        // source is contested is decided where `retained` is honoured, so it
        // is covered end to end by
        // `a_copy_of_a_contested_name_keeps_the_inherited_default` in
        // `tests/cli.rs` rather than here.
        let source = "class First:\n    def target(self, value=1): return value\n\nAlias = First\n\nOther = Alias\n\nclass Child(Other):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("self.target()", "self.target(value=1)")
        );
        Ok(())
    }

    #[test]
    fn a_certain_suite_settles_a_name_by_rebinding_what_the_scope_held() -> Result<(), String> {
        // The `if True:` body runs, so `Alias` names `First` afterwards
        // whichever way the branch above it went. Binding what the scope
        // already held is still a binding: passing over it left the earlier
        // branch's candidate standing and the call declined for nothing.
        let source = "import os\n\nclass First:\n    def target(self, value=1): return value\n\nclass Second:\n    def target(self, other=2): return other\n\nAlias = First\nif os.environ.get(\"PICK\"):\n    Alias = Second\n\nif True:\n    Alias = First\n\nclass Child(Alias):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n";
        assert_eq!(
            fixed(source)?,
            source
                .replace("def target(self, value=1)", "def target(self, value)")
                .replace("def target(self, other=2)", "def target(self, other)")
                .replace("self.target()", "self.target(value=1)")
        );
        Ok(())
    }

    #[test]
    fn imported_metaclass_uncertainty_propagates_through_local_bases() {
        // A metaclass an imported base carries builds every class under it, so
        // the local class between the import and the dataclass hides nothing.
        // A keyword-only default is no safer to remove than a positional one
        // here: the metaclass reaches `__init__` either way, and the inherited
        // fields are unknown, so no call could be rewritten to make up for it.
        let source = "from dataclasses import dataclass, field\nfrom base import Parent\n\nclass Middle(Parent):\n    pass\n\n@dataclass\nclass Child(Middle):\n    value: int = field(default=1, kw_only=True)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn dotted_submodules_win_over_package_nested_classes() -> Result<(), String> {
        // `import package.module` sets the submodule on the package, so the
        // initializer's namesake class is not what the dotted base reaches.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &initializer,
            "class module:\n    class Base:\n        def target(self, value=1): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "import package.module\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n\nassert Child().run() == 2\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_local_package_namesake_holds_a_dotted_base() -> Result<(), String> {
        // A class written above the subclass takes the package's name over, so
        // the dotted base is that class's nested one and not the submodule the
        // import bound. Preferring the submodule regardless would write the
        // wrong module's field into the inherited call.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "import package.module\n\nclass package:\n    class module:\n        class Base:\n            def target(self, value=7): return value\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n\nassert Child().run() == 7\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=7)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_local_namesake_holds_a_single_component_dotted_base() -> Result<(), String> {
        // The prefix is one name rather than a dotted path, so the walk over
        // its components has only that name to test. A class written above the
        // subclass takes it over just the same, and the base is the nested
        // class here rather than the package the import bound.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("pkg");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &initializer,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "import pkg\n\nclass pkg:\n    class Base:\n        def target(self, value=1): return value\n\nclass Child(pkg.Base):\n    def run(self): return self.target()\n\nassert Child().run() == 1\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=1)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_plain_class_hides_the_enclosing_dataclass_of_the_same_name() -> Result<(), String> {
        // The `Base` the alias reads is the class `outer` writes, which has no
        // fields at all. Walking past it to the module dataclass would name a
        // keyword the class `Child` really inherits from has no field for.
        let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    module: int = 1\n\ndef outer():\n    class Base:\n        pass\n\n    Alias = Base\n\n    @dataclass\n    class Child(Alias):\n        child: int = 3\n\n    return Child()\n\nouter()\n";
        let updated = fixed(source)?;
        assert!(!updated.contains("Child(module="), "{updated}");
        Ok(())
    }

    #[test]
    fn a_safe_redefinition_clears_imported_metaclass_uncertainty() {
        // The second `Base` is the one standing when `Child` is written, and
        // it inherits from nothing this file cannot see, so the uncertainty
        // the first one carried must not outlive it.
        let source = "from dataclasses import dataclass\nfrom base import Parent\n\nclass Base(Parent):\n    pass\n\nclass Base:\n    pass\n\n@dataclass\nclass Child(Base):\n    value: int = 1\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
        assert!(checked
            .signatures
            .iter()
            .any(|signature| signature.positional == ["value"]));
    }

    #[test]
    fn nested_assignments_do_not_clear_module_truthiness() -> Result<(), String> {
        let source = "enabled = True\n\ndef nested():\n    enabled = unknown\n\nif enabled:\n    def target(value=1): return value\n    target()\n";
        assert_eq!(
            fixed(source)?,
            "enabled = True\n\ndef nested():\n    enabled = unknown\n\nif enabled:\n    def target(value): return value\n    target(value=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_rebinding_itself_keeps_imported_metaclass_uncertainty() {
        // `Base` is rebound to a subclass of its own earlier binding, which
        // still reaches the unseen import, so the metaclass that import may
        // carry still builds `Child` and no fix may be offered.
        let source = "from dataclasses import dataclass\nfrom base import Parent\n\nclass Base(Parent):\n    pass\n\nclass Base(Base):\n    pass\n\n@dataclass\nclass Child(Base):\n    value: int = 1\n\nChild()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_class_body_name_is_out_of_scope_for_the_bodies_written_in_it() {
        // A class body is not a closure scope. `Outer.Safe` stands for nothing
        // inside `method`, inside `Mid`, or inside a class deeper still, so
        // every `Child` here is built on the module-level `Safe` that inherits
        // the unseen import. Under a metaclass on that import CPython answers
        // `Child()` and `Child(value=1)` differently, so none may be rewritten.
        let bodies = [
            "    def method(self):\n        @dataclass\n        class Child(Safe):\n            value: int = 1\n        return Child()\n",
            "    class Mid:\n        @dataclass\n        class Child(Safe):\n            value: int = 1\n",
            "    class Mid:\n        class Deeper:\n            @dataclass\n            class Child(Safe):\n                value: int = 1\n",
        ];
        for body in bodies {
            let source = format!("from dataclasses import dataclass\nfrom base import Parent\n\nclass Safe(Parent):\n    pass\n\nclass Outer:\n    class Safe:\n        pass\n\n{body}");
            let checked = check_source(
                Path::new("fixture.py"),
                &source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
            assert!(checked.signatures.is_empty(), "{source}");
        }
    }

    #[test]
    fn a_class_body_name_still_reaches_the_classes_that_body_writes() {
        // The bases of a class statement are read in the body that holds it,
        // where the name written above is bound, and nothing there is unseen,
        // so the fix is offered.
        let source = "from dataclasses import dataclass\n\nclass Safe:\n    pass\n\nclass Outer:\n    class Safe:\n        pass\n\n    def method(self):\n        @dataclass\n        class Child(Safe):\n            value: int = 1\n        return Child()\n\nOuter().method()\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn qualified_pydantic_private_attributes_are_not_model_fields() {
        // `pydantic` may be reached under its own name or an alias, and either
        // spelling is the same `PrivateAttr` call: per-instance state rather
        // than a field the constructor takes. A model base answers that on its
        // own, since an underscore name is no field there whatever it holds,
        // so the class here is a plain dataclass and the call is all there is
        // to read.
        for source in [
            "import pydantic\nfrom dataclasses import dataclass\n\n@dataclass\nclass C:\n    _value: int = pydantic.PrivateAttr(default=1)\n",
            "import pydantic as pd\nfrom dataclasses import dataclass\n\n@dataclass\nclass C:\n    _value: int = pd.PrivateAttr(default=1)\n",
        ] {
            assert!(messages(source, false).is_empty(), "{source}");
        }
        // An ordinary default in the same class is still reported, so the
        // silence above is the call being read rather than the shape being
        // passed over.
        assert_eq!(
            messages(
                "import pydantic\nfrom dataclasses import dataclass\n\n@dataclass\nclass C:\n    _value: int = 1\n",
                false,
            ),
            ["dataclass field `_value` has a default"]
        );
    }

    #[test]
    fn a_comprehension_walrus_rebinds_the_enclosing_name() -> Result<(), String> {
        // A walrus in a comprehension binds in the scope the comprehension is
        // written in, unlike one in a lambda body, which binds in the lambda's
        // own. A list comprehension runs where it is written, and a generator
        // expression runs as soon as anything draws from it, so the rebinding
        // reaches the module name before the class below is built. Skipping
        // the invalidation because the element is only visited and not yet run
        // kept a shape the name no longer holds, and the subclass was called
        // with fields the rebound base never had.
        let head = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    inherited: int = 1\n\nAlias = Base\n";
        let tail = "\n@dataclass\nclass Child(Alias):\n    value: int = 2\n\nChild()\n";
        for rebind in [
            "list((Alias := object) for _ in seed)\n",
            "total = sum(1 for _ in seed if (Alias := object))\n",
            "values = ((Alias := object) for _ in seed)\nlist(values)\n",
            "values = [(Alias := object) for _ in seed]\n",
        ] {
            let updated = fixed(&format!("{head}{rebind}{tail}"))?;
            assert!(!updated.contains("Child(inherited="), "{rebind}\n{updated}");
        }
        // A comprehension with no walrus rebinds nothing, so the alias still
        // stands and the inherited field comes through.
        for kept in ["values = (item for item in seed)\n", ""] {
            let updated = fixed(&format!("{head}{kept}{tail}"))?;
            assert!(
                updated.contains("Child(inherited=1, value=2)"),
                "{kept}\n{updated}"
            );
        }
        Ok(())
    }

    #[test]
    fn enum_member_initializer_defaults_are_retained() {
        // Assigning a member in an `Enum` body calls `__init__` with whatever
        // the assignment holds, so a parameter the members never supply is
        // reached only through its default. Removing it turns class creation
        // itself into a `TypeError`, and there is no call site to rewrite,
        // because the calls are the member assignments the interpreter makes.
        for source in [
            "from enum import Enum\n\nclass E(Enum):\n    A = 1\n    def __init__(self, value, label='x'): self.label = label\n",
            "import enum as enums\n\nclass E(enums.Enum):\n    A = 1\n    def __init__(self, value, label='x'): self.label = label\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
        }
        // The same body with an ordinary base has no implicit call, so the
        // default is still removed: the retention is the enum base, not the
        // method name.
        let control = "class E:\n    def __init__(self, value, label='x'): self.label = label\n";
        let checked = check_source(
            Path::new("fixture.py"),
            control,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn enum_missing_hook_defaults_are_retained() {
        let source = "from enum import Enum\n\nclass E(Enum):\n    A = 1\n    @classmethod\n    def _missing_(cls, value, fallback='x'): return cls.A\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }

    #[test]
    fn an_aliased_class_holds_a_dotted_base_over_a_submodule() -> Result<(), String> {
        // An assignment above the subclass puts a class behind the package's
        // name, so the dotted base reaches that class's nested one rather than
        // the submodule the import bound. Taking the submodule regardless
        // writes the wrong module's default into the inherited call.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "import package.module\n\nclass Local:\n    class module:\n        class Base:\n            def target(self, value=7): return value\n\npackage = Local\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n\nassert Child().run() == 7\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=7)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_self_named_base_uses_the_previous_metaclass_binding() {
        // The name a class statement writes is bound only once its body has
        // run, so a base spelled the same as the class being defined reaches
        // the earlier binding. That earlier `Base` inherits an import this run
        // cannot see, and the redefinition carries the doubt forward: a
        // metaclass there could build `Child` itself, leaving the field's
        // default in place.
        let source = "from dataclasses import dataclass, field\nfrom base import Parent\n\nclass Base(Parent):\n    pass\n\nclass Base(Base):\n    pass\n\n@dataclass\nclass Child(Base):\n    value: int = field(default=1, kw_only=True)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn metaclass_base_lookup_skips_outer_class_scopes() {
        // A class body is not a scope a nested body closes over, so the `Base`
        // an outer class body binds is invisible from the body inside it. The
        // base the nested class really gets is the module-level one, and the
        // import behind that one could bring a metaclass along, so the field's
        // default has to stay.
        let source = "from dataclasses import dataclass, field\nfrom base import Parent\n\nclass Base(Parent):\n    pass\n\nclass Container:\n    @dataclass\n    class Base:\n        pass\n\n    class Inner:\n        @dataclass\n        class Child(Base):\n            value: int = field(default=1, kw_only=True)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn metaclass_base_lookup_uses_the_immediate_class_scope() {
        // The header of a class written directly in another class body is
        // evaluated with that body's names in hand, so the sibling `Base`
        // beside it wins over the one the enclosing function bound. That
        // sibling inherits nothing, which leaves no metaclass to intercept
        // construction and no reason to keep the default.
        let source = "from dataclasses import dataclass, field\n\ndef build():\n    from base import Parent\n\n    class Base(Parent):\n        pass\n\n    class Container:\n        @dataclass\n        class Base:\n            pass\n\n        @dataclass\n        class Child(Base):\n            value: int = field(default=1, kw_only=True)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
        assert_eq!(checked.signatures.len(), 1);
    }
    #[test]
    fn inherited_enum_member_initializer_defaults_are_retained() {
        // Being an enumeration is inherited, so a subclass of one written in
        // this file creates its own members the same implicit way, and reaches
        // `_missing_` the same way too, however many classes and class bodies
        // stand between it and the import.
        for source in [
            "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Child(Base):\n    A = 1\n    def __init__(self, value, label='x'): self.label = label\n",
            "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Mid(Base):\n    pass\n\nclass Child(Mid):\n    A = 1\n    def __init__(self, value, label='x'): self.label = label\n",
            "import enum\n\nclass Base(enum.Enum):\n    pass\n\nclass Child(Base):\n    A = 1\n    def __init__(self, value, label='x'): self.label = label\n",
            "from enum import Enum\n\nclass Outer:\n    class Base(Enum):\n        pass\n\n    class Child(Base):\n        A = 1\n        def __init__(self, value, label='x'): self.label = label\n",
            "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Child(Base):\n    A = 1\n    @classmethod\n    def _missing_(cls, value, fallback='x'): return cls.A\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
            assert!(checked.signatures.is_empty(), "{source}");
        }
    }

    #[test]
    fn a_redefinition_that_is_no_enum_frees_its_subclasses() {
        // A base resolves to the class the name holds where the subclass is
        // written, not to the enumeration that name once stood for, and
        // nothing creates members of an ordinary class.
        let source = "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Base:\n    pass\n\nclass Child(Base):\n    def __init__(self, value, label='x'): self.label = label\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn an_inherited_enum_body_keeps_only_its_implicit_initializer() {
        // Reaching subclasses widens which bodies are enumerations, not which
        // of their methods are called implicitly. Every other method is called
        // through a call site the fixer can rewrite.
        let source = "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Child(Base):\n    A = 1\n    def describe(self, prefix='p'): return prefix\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }
    #[test]
    fn an_enum_hidden_by_a_nested_namesake_still_reaches_deeper_bodies() {
        // A class body binds names for itself alone. A function written in one
        // reads past it, so `Base` there is the module-level enumeration and
        // not the plain class the body wrote, and a class written in a further
        // class body reads past it the same way.
        for source in [
            "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Outer:\n    class Base:\n        pass\n\n    def build():\n        class Child(Base):\n            A = 1\n            def __init__(self, value, label='x'): self.label = label\n",
            "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nclass Outer:\n    class Base:\n        pass\n\n    class Holder:\n        class Child(Base):\n            A = 1\n            def __init__(self, value, label='x'): self.label = label\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
        }
    }

    #[test]
    fn an_enum_in_a_class_body_does_not_reach_the_bodies_written_in_it() {
        // The same rule the other way round: the enumeration this class body
        // writes is no name at all to a function written below it there, which
        // reaches the module-level plain class instead, so nothing creates
        // members and the default is as removable as any other.
        let source = "from enum import Enum\n\nclass Base:\n    pass\n\nclass Outer:\n    class Base(Enum):\n        pass\n\n    def build():\n        class Child(Base):\n            def __init__(self, value, label='x'): self.label = label\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn an_enum_in_a_class_body_still_reaches_the_classes_beside_it() {
        // Only the bodies written in it are out of reach. A class statement
        // later in the same body reads the name the way anything else there
        // does, so this subclass is an enumeration.
        let source = "from enum import Enum\n\nclass Outer:\n    class Base(Enum):\n        pass\n\n    class Child(Base):\n        A = 1\n        def __init__(self, value, label='x'): self.label = label\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
    }
    #[test]
    fn a_rebinding_takes_the_name_off_a_local_enum() {
        // Whatever now stands under the name is what the base below spells, so
        // this class is ordinary and nothing creates members of it.
        let source = "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nBase = object\n\nclass Child(Base):\n    def __init__(self, value, label='x'): self.label = label\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn a_parameter_takes_the_name_off_a_local_enum() {
        // The parameter holds the name for the whole body, so the class
        // written there is built on what the caller passed.
        let source = "from enum import Enum\n\nclass Base(Enum):\n    pass\n\ndef build(Base):\n    class Child(Base):\n        def __init__(self, value, label='x'): self.label = label\n    return Child\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn a_loop_target_takes_the_name_off_a_local_enum() {
        // Every rebinding form goes through the same invalidation, so a loop
        // target hides the enumeration exactly as an assignment does.
        let source = "from enum import Enum\n\nclass Base(Enum):\n    pass\n\nfor Base in bases:\n    pass\n\nclass Child(Base):\n    def __init__(self, value, label='x'): self.label = label\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    /// Write a package whose `module.Base.target` defaults to `2` beside a
    /// case file, fix them all, and hand back what the case file became.
    fn reclaimed_owner_fixture(case_source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(&case, case_source).map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        std::fs::read_to_string(case).map_err(|error| error.to_string())
    }

    #[test]
    fn later_imports_reclaim_dotted_module_owners() -> Result<(), String> {
        // `import package.module` binds `package` again, so the base names
        // the imported class and not the classes nested under the local
        // `package`, which CPython agrees on: the call returns 2.
        let updated = reclaimed_owner_fixture(
            "class package:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\nimport package.module\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n",
        )?;
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_class_above_a_reclaiming_import_still_owns_the_bases_beneath_it() -> Result<(), String> {
        // The import takes the name back only from where it is written, so a
        // subclass above it is still built on the local class. CPython gives
        // the first call 3 and the second 2.
        let updated = reclaimed_owner_fixture(
            "class package:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\nclass Early(package.module.Base):\n    def run(self): return self.target()\n\nimport package.module\n\nclass Late(package.module.Base):\n    def run(self): return self.target()\n",
        )?;
        assert!(updated.contains("self.target(value=3)"), "{updated}");
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn later_imports_reclaim_assignment_aliases() -> Result<(), String> {
        // `Widget` names the local class until the import binds it again, so
        // the subclass below is built on the imported class. CPython gives the
        // call 2.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Widget:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Local:\n    def target(self, value=3): return value\n\nWidget = Local\n\nfrom package.module import Widget\n\nclass Child(Widget):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn later_imports_reclaim_plain_class_names() -> Result<(), String> {
        // The plain-name half of the same rule: `from api import Base` binds
        // `Base` again, so the subclass below is built on the imported class,
        // whose default CPython gives the call as 9.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &api,
            "class Base:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Base:\n    def target(self, value=1): return value\n\nfrom api import Base\n\nclass Child(Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=9)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_class_written_again_below_an_import_keeps_the_name_from_it() -> Result<(), String> {
        // The scope writes `Base` again below the import, so the import never
        // ends up holding the name and the subclass is built on the second
        // local class, which CPython gives the call as 4. The two same-named
        // classes leave the file with no signature to rewrite against, which
        // is #1102 and is unchanged here; what matters is that the import's
        // default is not handed over in its place.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &api,
            "class Base:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Base:\n    def target(self, value=1): return value\n\nfrom api import Base\n\nclass Base:\n    def target(self, value=4): return value\n\nclass Child(Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(!updated.contains("self.target(value=9)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_second_import_reclaims_a_name_its_own_first_import_bound() -> Result<(), String> {
        // The scope imports `Base`, defines a class of the name, and imports it
        // again. It is the second import that takes the name back, so the
        // subclass under it is built on the imported class and CPython gives
        // the call 9. Reading only the first import of a name would leave the
        // local class holding it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let api = directory.path().join("api.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &api,
            "class Base:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "from api import Base\n\nclass Base:\n    def target(self, value=1): return value\n\nfrom api import Base\n\nclass Child(Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[api, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=9)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_reclaiming_import_this_pass_cannot_follow_leaves_the_name_unresolved() -> Result<(), String>
    {
        // `external.py` is not among the checked files, so nothing here can say
        // what the import bound. The class the import took the name from is
        // still not the answer: CPython gives the call 9, and rewriting it
        // against the local class would pass 1. The import's own default is
        // left alone with its module, so leaving the call as it stands is what
        // keeps running.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let external = directory.path().join("external.py");
        let case = directory.path().join("case.py");
        std::fs::write(
            &external,
            "class Base:\n    def target(self, value=9): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Base:\n    def target(self, value=1): return value\n\nfrom external import Base\n\nclass Child(Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(std::slice::from_ref(&case))?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target()"), "{updated}");
        Ok(())
    }

    #[test]
    fn an_import_in_a_suite_that_certainly_runs_reclaims_a_dotted_owner() -> Result<(), String> {
        // A suite the statement is certain to enter binds for the code after
        // it, so the import there hands `package` back to the module and the
        // classes nested under the local namesake are out of reach beneath it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class package:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\nif True:\n    import package.module\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn an_import_in_a_suite_that_certainly_runs_reclaims_an_alias() -> Result<(), String> {
        // The classes a scope defines carry offsets that settle this on their
        // own; a name an assignment put behind a class carries none, so the
        // suite has to hand the reclaim out to the scope around it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Local:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\npackage = Local\n\nif True:\n    import package.module\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_conditional_import_does_not_reclaim_a_dotted_owner() -> Result<(), String> {
        // The suite may not run, so the class written above it is still the
        // one the file may end up with. Neither answer is safe here — see
        // #1100 — and handing the name to the module would strip the local
        // default while leaving the call unable to run without it.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "import os\n\nclass package:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\nif os.environ.get(\"USE_REAL\"):\n    import package.module\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=3)"), "{updated}");
        Ok(())
    }

    #[test]
    fn an_assignment_below_a_suite_import_keeps_the_name_it_binds() -> Result<(), String> {
        // The suite takes the name back with its import and then binds it
        // again, so what stands there when the statement ends is the class the
        // assignment named. Carrying the reclaim out regardless would hand the
        // code below the module instead, and resolve the subclass against a
        // base it is not built on.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Local:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\nif True:\n    import package.module\n    package = Local\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=3)"), "{updated}");
        Ok(())
    }

    #[test]
    fn a_suite_import_below_an_assignment_still_reclaims_the_name() -> Result<(), String> {
        // The other order, which pins the fix above from the other side: the
        // import is what the suite ends with, so the name is the module's and
        // the class the assignment named is out of reach below.
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let package = directory.path().join("package");
        std::fs::create_dir(&package).map_err(|error| error.to_string())?;
        let initializer = package.join("__init__.py");
        let module = package.join("module.py");
        let case = directory.path().join("case.py");
        std::fs::write(&initializer, "").map_err(|error| error.to_string())?;
        std::fs::write(
            &module,
            "class Base:\n    def target(self, value=2): return value\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            &case,
            "class Local:\n    class module:\n        class Base:\n            def target(self, value=3): return value\n\nif True:\n    package = Local\n    import package.module\n\nclass Child(package.module.Base):\n    def run(self): return self.target()\n",
        )
        .map_err(|error| error.to_string())?;
        fix_all(&[initializer, module, case.clone()])?;
        let updated = std::fs::read_to_string(case).map_err(|error| error.to_string())?;
        assert!(updated.contains("self.target(value=2)"), "{updated}");
        Ok(())
    }

    #[test]
    fn enum_generate_next_value_defaults_are_retained() {
        // `auto()` resolves to whatever `_generate_next_value_` returns, and
        // the enum machinery calls it with the member name, the start, the
        // count and the values so far -- four arguments, whether the hook is
        // written as a static method or left bare for the metaclass to wrap.
        // A fifth parameter is reached only through its default, and the call
        // sits inside `enum.py`, so removing the default turns the class
        // statement itself into a `TypeError` with no call site to rewrite.
        for source in [
            "from enum import Enum, auto\n\nclass E(Enum):\n    @staticmethod\n    def _generate_next_value_(name, start, count, last_values, suffix='x'):\n        return name + suffix\n    A = auto()\n",
            "from enum import Enum, auto\n\nclass E(Enum):\n    def _generate_next_value_(name, start, count, last_values, suffix='x'):\n        return name + suffix\n    A = auto()\n",
        ] {
            let checked = check_source(
                Path::new("fixture.py"),
                source,
                false,
                Path::new(""),
                &Reexports::default(),
                &default_bases(),
                true,
            );
            assert_eq!(checked.diagnostics.len(), 1, "{source}");
            assert!(checked.diagnostics[0].fix.is_none(), "{source}");
            assert!(checked.signatures.is_empty(), "{source}");
        }
        // A plain class has no enum machinery to call the hook, so the same
        // method name there is an ordinary function: the retention is the enum
        // base, not the name.
        let control = "class E:\n    @staticmethod\n    def _generate_next_value_(name, start, count, last_values, suffix='x'):\n        return name + suffix\n";
        let checked = check_source(
            Path::new("fixture.py"),
            control,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn annotate_hook_defaults_are_retained() {
        // Python 3.14 reaches a class's annotations by calling the
        // `__annotate__` it holds with the format alone, so a parameter beside
        // that one is only ever filled by its default. Stripping it leaves
        // annotation access raising `TypeError`, and there is no call written
        // anywhere for the fixer to put the argument back into.
        let source = "class C:\n    @staticmethod\n    def __annotate__(format, extra=1):\n        return {'x': extra}\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn module_annotate_hook_defaults_are_retained() {
        // A module carries the same hook, and reading its annotations calls it
        // the same one-argument way. It is written as a plain function rather
        // than in a class body, so it is the module-level protocol hooks it
        // belongs with, beside `__getattr__` and `__dir__`.
        let source = "def __annotate__(format, extra=1):\n    return {'x': extra}\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_nested_annotate_hook_keeps_its_defaults_to_itself() {
        // The module-level reading only covers the module's own hook. One
        // written inside a function is an ordinary local function that nothing
        // calls implicitly, so its default is the fixer's to remove.
        let source = "def build():\n    def __annotate__(format, extra=1):\n        return {'x': extra}\n    return __annotate__(1)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn import_finder_defaults_are_retained() {
        // The import system calls a meta-path finder's `find_spec` itself,
        // handing it the name, the path and the target and nothing else, so a
        // parameter beside those is only ever filled by its default. Removing
        // it leaves the next import raising `TypeError` from inside
        // `importlib`, where there is no written call the fixer could update.
        let source = "class Finder:\n    def find_spec(self, fullname, path, target, extra=1):\n        return None\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_finder_method_beside_find_spec_stays_fixable() {
        // Retention covers the callback the import system reaches for, not
        // every method a finder happens to carry, so an ordinary helper on the
        // same class is still the fixer's to rewrite.
        let source = "class Finder:\n    def find_spec(self, fullname, path, target, extra=1):\n        return self.helper(extra)\n    def helper(self, value=2):\n        return value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }

    #[test]
    fn import_loader_create_module_defaults_are_retained() {
        let source =
            "class Loader:\n    def create_module(self, spec, extra=1):\n        return None\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_loader_method_beside_create_module_stays_fixable() {
        // Retention is keyed to the hook `importlib` reaches for, so an
        // ordinary helper sharing the loader keeps its ordinary treatment.
        let source = "class Loader:\n    def create_module(self, spec, extra=1):\n        return self.helper(extra)\n    def helper(self, value=2):\n        return value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }

    #[test]
    fn import_loader_exec_module_defaults_are_retained() {
        // `importlib` runs a loader's `exec_module` from inside
        // `_bootstrap._load`, handing it the module and nothing more, so a
        // parameter beside it survives only through its default.
        let source =
            "class Loader:\n    def exec_module(self, module, extra=1):\n        module.answer = extra\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_loader_method_beside_exec_module_stays_fixable() {
        // Retention is keyed to the hook name, so a sibling sharing the
        // loader and the signature shape keeps its ordinary treatment.
        let source = "class Loader:\n    def exec_module(self, module, extra=1):\n        module.answer = self.helper(module)\n    def helper(self, module, value=2):\n        return value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }

    #[test]
    fn pickler_persistent_id_defaults_are_retained() {
        let source = "class P:\n    def persistent_id(self, obj, extra=1):\n        return None\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_pickler_method_beside_persistent_id_stays_fixable() {
        // Retention is keyed to the hook `pickle` reaches for, so an ordinary
        // helper sharing the pickler keeps its ordinary treatment.
        let source = "class P:\n    def persistent_id(self, obj, extra=1):\n        return self.helper(extra)\n    def helper(self, value=2):\n        return value\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }

    #[test]
    fn pickler_reducer_override_defaults_are_retained() {
        // `dump` caches a pickler's `reducer_override` and calls it with the
        // object being pickled and nothing else, so a parameter beside it is
        // only ever filled by its default. Removing it leaves `dump` raising
        // `TypeError` from inside `pickle`, where there is no written call
        // the fixer could update.
        let source = "class P:\n    def reducer_override(self, obj, extra=1):\n        return NotImplemented\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_pickler_method_beside_reducer_override_stays_fixable() {
        // A sibling carrying the very same signature is still stripped, which
        // is what pins the retention to the hook name rather than to the
        // shape of the parameter list.
        let source = "class P:\n    def reducer_override(self, obj, extra=1):\n        return self.fallback(obj, extra)\n    def fallback(self, obj, extra=1):\n        return NotImplemented\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }
    #[test]
    fn unpickler_persistent_load_defaults_are_retained() {
        // `_pickle` invokes an unpickler's `persistent_load` with the
        // persistent id alone, so a parameter beside it only ever arrives as
        // its default. The call is made by the interpreter's own unpickling
        // loop rather than by any written line, so dropping the default leaves
        // the next `load` raising `TypeError: U.persistent_load() missing 1
        // required positional argument` with nothing for the fixer to update.
        let source =
            "class U:\n    def persistent_load(self, pid, extra=1):\n        return (pid, extra)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn an_unpickler_method_beside_persistent_load_stays_fixable() {
        // Retention is keyed to the hook name, so a sibling sharing the
        // unpickler and the signature shape keeps its ordinary treatment.
        let source = "class U:\n    def persistent_load(self, pid, extra=1):\n        return self.helper(pid)\n    def helper(self, pid, value=2):\n        return (pid, value)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }

    #[test]
    fn a_method_named_near_persistent_load_stays_fixable() {
        // The unpickling loop looks the hook up under its exact name, so a
        // near miss is an ordinary method and its default is the fixer's.
        let source = "class U:\n    def persistent_loads(self, pid, extra=1):\n        return (pid, extra)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn a_module_level_persistent_load_stays_fixable() {
        // Only an unpickler's attribute is consulted for the hook, so a plain
        // function of that name at module level carries no such obligation.
        let source = "def persistent_load(pid, extra=1):\n    return (pid, extra)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }

    #[test]
    fn inspect_loader_get_code_defaults_are_retained() {
        // `importlib` executes a module by asking its loader for the code
        // object, calling `get_code(fullname)` with the module name alone, so
        // a parameter beside it only ever arrives as its default. That call is
        // made from `<frozen importlib._bootstrap_external>` rather than by
        // any written line, so dropping the default leaves the next import
        // raising `TypeError: L.get_code() missing 1 required positional
        // argument` with nothing for the fixer to update.
        let source = "class L:\n    def get_code(self, fullname, extra=1):\n        return None\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.signatures.is_empty());
    }

    #[test]
    fn a_loader_method_beside_get_code_stays_fixable() {
        // Retention is keyed to the hook name, so a sibling sharing the loader
        // and the signature shape keeps its ordinary treatment.
        let source = "class L:\n    def get_code(self, fullname, extra=1):\n        return self.helper(fullname)\n    def helper(self, fullname, value=2):\n        return (fullname, value)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 2);
        assert!(checked.diagnostics[0].fix.is_none());
        assert!(checked.diagnostics[1].fix.is_some());
    }

    #[test]
    fn a_method_named_near_get_code_stays_fixable() {
        // The import machinery looks the hook up under its exact name, so a
        // near miss is an ordinary method and its default is the fixer's.
        let source =
            "class L:\n    def get_codes(self, fullname, extra=1):\n        return (fullname, extra)\n";
        let checked = check_source(
            Path::new("fixture.py"),
            source,
            false,
            Path::new(""),
            &Reexports::default(),
            &default_bases(),
            true,
        );
        assert_eq!(checked.diagnostics.len(), 1);
        assert!(checked.diagnostics[0].fix.is_some());
    }
}
