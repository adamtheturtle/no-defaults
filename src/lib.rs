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
use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_parser::{parse_expression, parse_module};
use ruff_text_size::{Ranged, TextRange, TextSize};
use serde::{Deserialize, Serialize};
use similar::TextDiff;

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
    #[arg(long)]
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

    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn matches(&self, base: &Expr) -> bool {
        match base {
            Expr::Name(name) => self.names.contains(name.id.as_str()),
            Expr::Attribute(attribute) => self.names.contains(attribute.attr.as_str()),
            // `class Job(BaseModel, Generic[T])` names its base through a
            // subscript, as a generic model does.
            Expr::Subscript(subscript) => self.matches(&subscript.value),
            _ => false,
        }
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

#[derive(Debug)]
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
    /// Source text to pass at call sites, when the default can be reproduced
    /// without depending on names that the caller may not have imported.
    value: Option<String>,
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

impl Callable {
    /// Whether the name refers to a function, whose appearance outside a call
    /// is worth a warning. A class name appears in annotations and `isinstance`
    /// checks all the time, so those stay quiet.
    fn is_function(&self) -> bool {
        matches!(self, Self::Function | Self::Method { .. })
    }
}

/// What a name in a file refers to, as far as the import statements say.
#[derive(Clone, Debug)]
enum Binding {
    /// A module, so `name.attribute(...)` resolves into that file.
    Module(PathBuf),
    /// A symbol imported from a module, so `name(...)` resolves to it.
    Symbol(PathBuf, String),
}

/// A replacement for a range of a file. Deletions carry an empty replacement;
/// call-site rewrites carry an empty range and the text to insert.
#[derive(Clone, Debug)]
struct Edit {
    range: TextRange,
    replacement: String,
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
    /// Every name a fixed callable goes by, so a call that cannot be resolved
    /// to one can still be reported rather than silently left behind.
    names: BTreeSet<String>,
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
}

/// The calls one file makes to fixed callables.
#[derive(Default)]
struct FileCallSites {
    edits: Vec<Edit>,
    skipped: Vec<Skipped>,
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
    let results: Vec<Result<Checked, String>> = files
        .par_iter()
        .zip(settings.par_iter())
        .map(|(path, setting)| {
            // An exempt file contributes no violations and no signatures, but
            // its calls are still rewritten when a callable it uses is fixed
            // elsewhere. Exemption is about which definitions are checked, not
            // about leaving the file broken at runtime.
            setting.private_only.map_or_else(
                || Ok(Checked::default()),
                |private_only| {
                    check_file(
                        path,
                        private_only,
                        &setting.project_root,
                        &setting.reexports,
                        &setting.field_bases,
                        fixing,
                    )
                },
            )
        })
        .collect();
    let mut diagnostics = Vec::new();
    let mut signatures = Vec::new();
    let mut skipped = Vec::new();
    for result in results {
        let checked = result?;
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
    report_diagnostics(&diagnostics, cli.output_format)?;
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
        call_sites
            .edits
            .entry(diagnostic.path.clone())
            .or_default()
            .push(Edit {
                range,
                replacement: String::new(),
            });
    }
    let mut updated = 0;
    let changes = fixed_sources(call_sites.edits, &mut updated)?;
    if cli.diff {
        print_diffs(&changes);
        warn_about_skipped_calls(&call_sites.skipped);
        return Ok(true);
    }
    write_fixes_atomically(changes)?;
    // A syntax error carries no fix, so it is still there afterwards and the
    // run has to say so rather than claim everything was fixed.
    let remaining = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.fix.is_none())
        .count();
    match cli.output_format {
        // The machine formats carry the diagnostics themselves. A summary line
        // on stdout would make the JSON unparseable, so the counts stay in the
        // human formats where they belong.
        OutputFormat::Json | OutputFormat::Github => {
            report_diagnostics(diagnostics, cli.output_format)?;
        }
        OutputFormat::Full | OutputFormat::Concise => println!(
            "Found {} error{} ({} fixed, {remaining} remaining).",
            diagnostics.len(),
            if diagnostics.len() == 1 { "" } else { "s" },
            diagnostics.len() - remaining,
        ),
    }
    report_call_sites(diagnostics, &call_sites.skipped, updated, cli.output_format);
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
fn report_call_sites(
    diagnostics: &[Diagnostic],
    skipped: &[Skipped],
    updated: usize,
    format: OutputFormat,
) {
    let removed = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "NOD001" && diagnostic.fix.is_some())
        .count();
    if removed == 0 {
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
    eprintln!(
        "warning: {removed} default{} removed. Call sites in the checked files were \
         updated, but callers outside them, and calls made dynamically, were not. \
         Run your tests.",
        if removed == 1 { "" } else { "s" },
    );
}

/// Insert the removed defaults as explicit arguments at every call the checked
/// files make to a callable that `--fix` changed.
///
/// A call resolves only when the calling file's own imports say it refers to
/// the file that was fixed. A bare name match is not enough: a project with its
/// own `connect` must not have `socket.connect` rewritten.
fn call_site_edits(files: &[PathBuf], signatures: Vec<Signature>) -> Result<CallSites, String> {
    let mut definitions = Definitions::default();
    for signature in signatures {
        definitions.names.insert(signature.name.clone());
        let table = match &signature.kind {
            Callable::Method { class, .. } => definitions
                .methods
                .entry((signature.path.clone(), class.clone()))
                .or_default(),
            Callable::Function | Callable::Dataclass => definitions
                .symbols
                .entry(signature.path.clone())
                .or_default(),
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
    let known: BTreeSet<&Path> = files.iter().map(PathBuf::as_path).collect();
    let results: Vec<Result<FileCallSites, String>> = files
        .par_iter()
        .map(|path| {
            let source = read_source(path)?;
            // A file the parser rejects holds no calls this pass can see. It
            // is reported as a syntax error by the checker, so skipping it
            // here leaves the rest of the project fixable.
            match rewrite_calls(path, &source, &definitions, &known) {
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
    }
    call_sites.skipped.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    Ok(call_sites)
}

/// Find the checked file a dotted module name refers to.
///
/// An absolute import is resolved against the importer's own import root, and
/// failing that by path suffix, because the import root of another source tree
/// is not known. A single-component import is only ever resolved against the
/// importer's root, since one filename matches at any depth; an ambiguous
/// suffix resolves to nothing rather than to a guess.
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
        for candidate in [
            directory.with_extension("py"),
            directory.join("__init__.py"),
        ] {
            if known.contains(candidate.as_path()) {
                return Some(candidate);
            }
        }
        return None;
    }
    if parts.is_empty() {
        return None;
    }
    // The importer's own root is where Python would look first, and it is the
    // only root this can establish from the tree alone.
    if let Some(mut candidate) = import_root(importer) {
        for part in &parts {
            candidate.push(part);
        }
        for candidate in [
            candidate.with_extension("py"),
            candidate.join("__init__.py"),
        ] {
            if known.contains(candidate.as_path()) {
                return Some(candidate);
            }
        }
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
        format!("{}.py", parts[parts.len() - 1]),
        "__init__.py".to_owned(),
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

/// The directory an absolute import in `importer` resolves against.
///
/// Python resolves `import utils` against `sys.path`, which a per-file linter
/// cannot know. What it can establish is the importer's own position: walk out
/// of the packages containing it, and the first directory that is not a package
/// is where its own top-level imports are rooted.
fn import_root(importer: &Path) -> Option<PathBuf> {
    let mut directory = importer.parent()?.to_path_buf();
    while package_init(&directory).is_some() {
        if !directory.pop() {
            return None;
        }
    }
    Some(directory)
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

impl SourceLines {
    fn read(path: &Path) -> Option<Self> {
        let text = read_source(path).ok()?;
        // An empty file has no lines at all, so it gets no first line either.
        let starts = if text.is_empty() {
            Vec::new()
        } else {
            std::iter::once(0)
                .chain(
                    text.match_indices('\n')
                        .map(|(position, _)| position + 1)
                        .filter(|start| *start < text.len()),
                )
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

fn report_diagnostics(diagnostics: &[Diagnostic], format: OutputFormat) -> Result<(), String> {
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
                println!("{space:width$} |", space = "", width = width);
                println!("{} | {}", diagnostic.line, line);
                println!(
                    "{space:width$} | {caret:>column$}",
                    space = "",
                    caret = "^",
                    width = width,
                    column = diagnostic.column
                );
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
    if !diagnostics.is_empty() {
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
    let mut reexport_cache: BTreeMap<PathBuf, PackageReexports> = BTreeMap::new();
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
            package_reexports(directory, &loaded.root, &mut reexport_cache)?.names
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
    root: &Path,
    cache: &mut BTreeMap<PathBuf, PackageReexports>,
) -> Result<PackageReexports, String> {
    if let Some(cached) = cache.get(directory) {
        return Ok(cached.clone());
    }
    let init = package_init(directory);
    let climbs = directory != root && (directory.starts_with(root) || init.is_some());
    let inherited = match directory.parent().filter(|_| climbs) {
        Some(parent) => package_reexports(parent, root, cache)?,
        None => PackageReexports::default(),
    };
    let name = directory.file_name().unwrap_or_default().to_string_lossy();
    // The directory the walk stops at is not reached by an import, so its own
    // name says nothing about what is inside it.
    let sealed =
        inherited.sealed || (climbs && is_private(&name) && !inherited.names.covers(&name));
    let package = match init {
        Some(init) if !sealed => {
            let mut names = (*inherited.names).clone();
            collect_reexports(&init, &mut names)?;
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
    cache.insert(directory.to_path_buf(), package.clone());
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

fn collect_reexports(path: &Path, reexports: &mut Reexports) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let parsed = parse_module(&source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let mut collector = ReexportCollector { reexports };
    for statement in parsed.suite() {
        collector.visit_stmt(statement);
    }
    Ok(())
}

/// Gathers what an `__init__.py` binds: imported names, and the strings of a
/// literal `__all__`.
struct ReexportCollector<'a> {
    reexports: &'a mut Reexports,
}

impl ReexportCollector<'_> {
    /// Add the entries of an `__all__` that is written out as a list or tuple
    /// of string literals. One computed from a call or another module's
    /// `__all__` says nothing this run can read.
    fn collect_all(&mut self, value: &Expr) {
        let elements = match value {
            Expr::List(list) => &list.elts,
            Expr::Tuple(tuple) => &tuple.elts,
            _ => return,
        };
        for element in elements {
            if let Expr::StringLiteral(string) = element {
                self.reexports.names.insert(string.value.to_string());
            }
        }
    }
}

fn is_dunder_all(target: &Expr) -> bool {
    matches!(target, Expr::Name(name) if name.id.as_str() == "__all__")
}

impl<'a> Visitor<'a> for ReexportCollector<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        self.reexports.wildcard = true;
                    } else {
                        let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                        self.reexports.names.insert(bound.to_string());
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
                    self.reexports.names.insert(bound);
                }
            }
            Stmt::Assign(assign) if assign.targets.iter().any(is_dunder_all) => {
                self.collect_all(&assign.value);
            }
            Stmt::AnnAssign(assign) if is_dunder_all(&assign.target) => {
                if let Some(value) = assign.value.as_deref() {
                    self.collect_all(value);
                }
            }
            Stmt::AugAssign(assign) if is_dunder_all(&assign.target) => {
                self.collect_all(&assign.value);
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
) -> Result<BTreeMap<PathBuf, (String, String)>, String> {
    edits
        .into_iter()
        .map(|(path, edits)| {
            let source = read_source(&path)?;
            let (fixed, applied) = apply_edits(&source, edits);
            *updated += applied;
            parse_module(&fixed).map_err(|error| {
                format!(
                    "refusing to write invalid Python to {} after fixing: {error}",
                    path.display()
                )
            })?;
            Ok((path, (source, fixed)))
        })
        .collect()
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
    let applied = insertions.len();
    let mut edits: Vec<Edit> = insertions
        .into_iter()
        .chain(deletions.into_iter().map(|range| Edit {
            range,
            replacement: String::new(),
        }))
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

fn write_fixes_atomically(changes: BTreeMap<PathBuf, (String, String)>) -> Result<(), String> {
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
        let permissions = std::fs::metadata(&path)
            .map_err(|error| {
                format!(
                    "could not inspect {} before fixing: {error}",
                    path.display()
                )
            })?
            .permissions();
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
        print!(
            "{}",
            TextDiff::from_lines(source, fixed).unified_diff().header(
                &format!("a/{}", path.display()),
                &format!("b/{}", path.display())
            )
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
            files.extend(
                WalkBuilder::new(path)
                    .standard_filters(true)
                    .build()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                    .map(ignore::DirEntry::into_path)
                    .filter(|entry| is_python(entry)),
            );
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
) -> Result<Checked, String> {
    let source = read_source(path)?;
    Ok(check_source(
        path,
        &source,
        private_only,
        project_root,
        reexports,
        field_bases,
        signatures,
    ))
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
    let mut aliases = Aliases::default();
    aliases.collect(parsed.suite());
    let mut checker = Checker {
        path,
        source,
        private_only,
        reexports,
        field_bases,
        aliases,
        scope: Scope {
            private: is_private_module(path, project_root, reexports),
            ..Scope::default()
        },
        header: None,
        collect_signatures: signatures,
        lines: LineIndex::new(source),
        classes: Vec::new(),
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
            boundary(body[..*index].chars().next_back())
                && boundary(body[index + "noqa".len()..].chars().next())
        })
        .map(|(index, _)| index)
}

fn parse_directive(source: &str, hash: usize) -> Option<Directive> {
    let line_start = source[..hash].rfind('\n').map_or(0, |end| end + 1);
    let break_start = source[hash..]
        .find('\n')
        .map_or(source.len(), |end| hash + end);
    let content_end = source[..break_start].trim_end_matches('\r').len();
    let comment = source.get(hash + 1..content_end)?;
    let body = comment.trim_start();
    let body_start = hash + 1 + (comment.len() - body.len());
    let lower = body.to_ascii_lowercase();
    let alone = source[line_start..hash].trim().is_empty();
    let flake8 = alone.then(|| file_level_prefix(&lower, "flake8")).flatten();
    let ruff = alone.then(|| file_level_prefix(&lower, "ruff")).flatten();
    let (file_level, rest) = if let Some((rest, _)) = flake8 {
        // A `# flake8: noqa` with anything appended is not a directive.
        (rest.is_empty(), None)
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
        source[after_codes..content_end].find('#').map_or_else(
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
        let line_end = if source[break_start..].starts_with('\n') {
            break_start + 1
        } else {
            break_start
        };
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
    /// Whether definitions here sit directly in a class body.
    class_body: bool,
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
    style: Option<FieldStyle>,
    /// Whether the class has base classes, whose fields come first in the
    /// constructor and are not visible from this class body. The base that
    /// made the class carry fields does not count: `BaseModel` contributes
    /// none of its own.
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

impl Checker<'_> {
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
    fn report(&mut self, default: &Expr, message: String, fix: TextRange) -> bool {
        self.report_with(default.start(), message, self.fixable(default, fix))
    }

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
        let stub = self
            .path
            .extension()
            .is_some_and(|extension| extension == "pyi");
        if stub && matches!(default, Expr::EllipsisLiteral(_)) {
            return None;
        }
        Some(fix)
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
        for parameter in parameters
            .posonlyargs
            .iter()
            .chain(&parameters.args)
            .chain(&parameters.kwonlyargs)
        {
            let Some(default) = &parameter.default else {
                continue;
            };
            removed |= self.report(
                default,
                format!(
                    "parameter `{}` of lambda has a default",
                    parameter.parameter.name
                ),
                TextRange::new(parameter.parameter.end(), default.end()),
            );
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

    fn check_function(&mut self, function: &ast::StmtFunctionDef) {
        if !self.enabled(function.name.as_str()) {
            return;
        }
        // The function's own range starts at its first decorator, so the name
        // locates the `def` line that a signature-wide directive sits on.
        let enclosing = self.header;
        self.header = Some(line_start(self.source, function.name.start()));
        let mut removed = Vec::new();
        for parameter in function
            .parameters
            .posonlyargs
            .iter()
            .chain(&function.parameters.args)
            .chain(&function.parameters.kwonlyargs)
        {
            if let Some(default) = &parameter.default {
                if self.report(
                    default,
                    format!(
                        "parameter `{}` of function `{}` has a default",
                        parameter.parameter.name, function.name
                    ),
                    TextRange::new(parameter.parameter.end(), default.end()),
                ) && self.collect_signatures
                {
                    removed.push(Removed {
                        parameter: parameter.parameter.name.to_string(),
                        value: literal_text(default, self.source),
                    });
                }
            }
        }
        self.header = enclosing;
        if removed.is_empty() {
            return;
        }
        let parameter_name =
            |parameter: &ast::ParameterWithDefault| parameter.parameter.name.to_string();
        self.signatures.push(Signature {
            name: function.name.to_string(),
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
                Some(class) if self.scope.class_body => Callable::Method {
                    class: class.name.clone(),
                    receiver: method_receiver(function),
                },
                _ => Callable::Function,
            },
            complete: true,
            removed,
        });
    }

    fn check_field(&mut self, style: FieldStyle, statement: &Stmt) {
        let Stmt::AnnAssign(assign) = statement else {
            return;
        };
        let Expr::Name(name) = &*assign.target else {
            return;
        };
        if is_class_var(statement) {
            return;
        }
        // A `_: KW_ONLY` marker is not a field, so it takes no place in the
        // constructor's positional order. It also makes every field after it
        // keyword-only, which does hold for the rest of the class body.
        let pseudo_field = annotates_kw_only(&assign.annotation, &self.aliases);
        // A field declared `init=False` is not a constructor parameter, so a
        // call must never be given it.
        let constructs = assign
            .value
            .as_deref()
            .is_none_or(|value| !field_excluded_from_init(value, &self.aliases));
        // `field(kw_only=...)` decides for one field, over whatever the
        // decorator or a marker said for the class.
        let kw_only = assign
            .value
            .as_deref()
            .and_then(|value| field_says_kw_only(value, &self.aliases))
            .unwrap_or_else(|| self.classes.last().is_some_and(|class| class.kw_only));
        if let Some(class) = self.classes.last_mut() {
            if pseudo_field {
                class.kw_only = true;
            }
            // Every other field that the constructor takes positionally counts
            // towards the order, even one without a default or one this run is
            // not enforcing. A keyword-only field holds no position.
            else if constructs && !kw_only {
                class.fields.push(name.id.to_string());
            }
        }
        let Some(value) = assign.value.as_deref() else {
            return;
        };
        if !self.enabled(name.id.as_str()) {
            return;
        }
        let Some(default) =
            field_default(value, assign.annotation.end(), self.source, &self.aliases)
        else {
            return;
        };
        if self.report(
            value,
            format!("{} `{}` has a {}", style.noun(), name.id, default.kind),
            default.fix,
        ) && self.collect_signatures
            && constructs
        {
            if let Some(class) = self.classes.last_mut() {
                class.removed.push(Removed {
                    parameter: name.id.to_string(),
                    value: default.value,
                });
            }
        }
    }
}

impl<'a> Visitor<'a> for Checker<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(function) => {
                self.check_function(function);
                let outer = self.scope;
                self.scope = Scope {
                    private: self.encloses_private(function.name.as_str(), outer),
                    // Annotated assignments in a method body are locals, not
                    // fields, so field detection stops at the function boundary.
                    fields: None,
                    class_body: false,
                };
                walk_stmt(self, statement);
                self.scope = outer;
            }
            Stmt::ClassDef(class) => {
                let outer = self.scope;
                let old_header = self.header;
                // As with a `def` line, the name locates the `class` line a
                // directive covering every field sits on.
                self.header = Some(line_start(self.source, class.name.start()));
                let style = field_style(class, self.field_bases, &self.aliases);
                self.scope = Scope {
                    private: self.encloses_private(class.name.as_str(), outer),
                    fields: style,
                    class_body: true,
                };
                if self.collect_signatures {
                    self.classes.push(ClassCollector {
                        name: class.name.to_string(),
                        style,
                        inherits: inherits_fields(class, style, self.field_bases),
                        constructs: generates_init(class),
                        kw_only: decorator_says_kw_only(class),
                        fields: Vec::new(),
                        removed: Vec::new(),
                    });
                }
                walk_stmt(self, statement);
                if let Some(collector) = self
                    .collect_signatures
                    .then(|| self.classes.pop())
                    .flatten()
                {
                    if collector.style.is_some()
                        && collector.constructs
                        && !collector.removed.is_empty()
                    {
                        self.signatures.push(Signature {
                            name: collector.name,
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
                self.scope = outer;
            }
            _ => {
                if let Some(style) = self.scope.fields {
                    self.check_field(style, statement);
                }
                walk_stmt(self, statement);
            }
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Lambda(lambda) = expression {
            self.check_lambda(lambda);
        }
        walk_expr(self, expression);
    }
}

fn is_private(name: &str) -> bool {
    name.starts_with('_') && !name.starts_with("__")
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
        is_private(name) && !reexports.covers(name)
    })
}

/// What makes a class carry fields, if anything does. The decorator wins where
/// both apply, because a `@dataclass` built on a configured base still follows
/// the `dataclasses` rules.
fn field_style(
    class: &ast::StmtClassDef,
    bases: &FieldBases,
    aliases: &Aliases,
) -> Option<FieldStyle> {
    if has_dataclass_decorator(class, aliases) {
        return Some(FieldStyle::Dataclass);
    }
    (!bases.is_empty() && class_bases(class).any(|base| bases.matches(base)))
        .then_some(FieldStyle::Base)
}

/// Whether the class takes fields from somewhere this class body cannot see.
///
/// The base that made it carry fields is not such a place: `BaseModel` declares
/// no fields, so a model naming it directly has its whole constructor here. Any
/// other base might declare some, including another model, which is why a
/// subclass of a subclass is left alone rather than half understood.
fn inherits_fields(
    class: &ast::StmtClassDef,
    style: Option<FieldStyle>,
    bases: &FieldBases,
) -> bool {
    match style {
        Some(FieldStyle::Base) => {
            class_bases(class).any(|base| !bases.matches(base) && !carries_no_fields(base))
        }
        _ => class_bases(class).any(|base| !carries_no_fields(base)),
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
fn carries_no_fields(base: &Expr) -> bool {
    let base = match base {
        Expr::Subscript(subscript) => &*subscript.value,
        expression => expression,
    };
    let name = match base {
        Expr::Name(name) => name.id.as_str(),
        Expr::Attribute(attribute) => attribute.attr.as_str(),
        _ => return false,
    };
    matches!(name, "Generic" | "Protocol" | "ABC" | "object")
}

fn class_bases(class: &ast::StmtClassDef) -> impl Iterator<Item = &Expr> {
    class
        .arguments
        .as_deref()
        .into_iter()
        .flat_map(|arguments| arguments.args.iter())
}

/// Local names that an aliased import bound to a member of `dataclasses` or
/// `pydantic`.
///
/// Decorators, annotations, and field calls are matched by the name they are
/// written with, so `from dataclasses import dataclass as dc` would otherwise
/// leave `@dc` unrecognised and the whole class unchecked.
#[derive(Debug, Default)]
struct Aliases(BTreeMap<String, String>);

impl Aliases {
    /// The `dataclasses` or `pydantic` member `name` was imported as, or `name`
    /// itself when the file did not rename anything to it.
    fn resolve<'a>(&'a self, name: &'a str) -> &'a str {
        self.0.get(name).map_or(name, String::as_str)
    }

    /// Collect the renaming imports of a module, including those nested in an
    /// `if TYPE_CHECKING:` block or a function body.
    fn collect(&mut self, statements: &[Stmt]) {
        for statement in statements {
            match statement {
                Stmt::ImportFrom(import) => {
                    let carries_fields = import.module.as_ref().is_some_and(|module| {
                        matches!(module.split('.').next(), Some("dataclasses" | "pydantic"))
                    });
                    if !carries_fields {
                        continue;
                    }
                    for alias in &import.names {
                        if let Some(local) = &alias.asname {
                            self.0.insert(local.to_string(), alias.name.to_string());
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
                Stmt::FunctionDef(function) => self.collect(&function.body),
                Stmt::ClassDef(class) => self.collect(&class.body),
                Stmt::With(block) => self.collect(&block.body),
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

fn has_dataclass_decorator(class: &ast::StmtClassDef, aliases: &Aliases) -> bool {
    class.decorator_list.iter().any(|decorator| {
        let expression = match &decorator.expression {
            Expr::Call(call) => &*call.func,
            expression => expression,
        };
        matched_name(expression, aliases) == Some("dataclass")
    })
}

/// Whether the decorator leaves the class with a generated `__init__`.
fn generates_init(class: &ast::StmtClassDef) -> bool {
    !class.decorator_list.iter().any(|decorator| {
        let Expr::Call(call) = &decorator.expression else {
            return false;
        };
        call.arguments.keywords.iter().any(|keyword| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|name| name.as_str() == "init")
                && matches!(&keyword.value, Expr::BooleanLiteral(literal) if !literal.value)
        })
    })
}

/// Whether the decorator says `kw_only=True`, making every field of the class
/// keyword-only in the generated constructor.
fn decorator_says_kw_only(class: &ast::StmtClassDef) -> bool {
    class.decorator_list.iter().any(|decorator| {
        let Expr::Call(call) = &decorator.expression else {
            return false;
        };
        call.arguments
            .keywords
            .iter()
            .any(|keyword| keyword_is(keyword, "kw_only") == Some(true))
    })
}

/// Whether a `field(...)` call says `kw_only=`, and what it said. `None` where
/// it does not say, so the class-wide setting stands.
fn field_says_kw_only(value: &Expr, aliases: &Aliases) -> Option<bool> {
    let Expr::Call(call) = value else {
        return None;
    };
    if matched_name(&call.func, aliases) != Some("field") {
        return None;
    }
    call.arguments
        .keywords
        .iter()
        .find_map(|keyword| keyword_is(keyword, "kw_only"))
}

/// The boolean a keyword argument was given, when it is named `name` and its
/// value is written as a literal.
fn keyword_is(keyword: &ast::Keyword, name: &str) -> Option<bool> {
    if keyword.arg.as_ref()?.as_str() != name {
        return None;
    }
    match &keyword.value {
        Expr::BooleanLiteral(literal) => Some(literal.value),
        _ => None,
    }
}

/// Whether an annotation is the `KW_ONLY` marker, which declares no field.
fn annotates_kw_only(annotation: &Expr, aliases: &Aliases) -> bool {
    matched_name(annotation, aliases) == Some("KW_ONLY")
}

/// Whether a field is declared `init=False`, which keeps it out of the
/// constructor even though it is still a field.
fn field_excluded_from_init(value: &Expr, aliases: &Aliases) -> bool {
    let Expr::Call(call) = value else {
        return false;
    };
    matched_name(&call.func, aliases) == Some("field")
        && call.arguments.keywords.iter().any(|keyword| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|name| name.as_str() == "init")
                && matches!(&keyword.value, Expr::BooleanLiteral(literal) if !literal.value)
        })
}

fn is_class_var(statement: &Stmt) -> bool {
    let Stmt::AnnAssign(assign) = statement else {
        return false;
    };
    annotates_class_var(&assign.annotation)
}

/// Whether an annotation names `ClassVar`, bare, qualified, or quoted.
///
/// `dataclasses` resolves a string annotation textually, so
/// `x: "ClassVar[int]" = 1` really is a class variable rather than a field.
fn annotates_class_var(annotation: &Expr) -> bool {
    match annotation {
        Expr::Name(name) => name.id.as_str() == "ClassVar",
        Expr::Attribute(attribute) => attribute.attr.as_str() == "ClassVar",
        Expr::Subscript(subscript) => annotates_class_var(&subscript.value),
        // Quoted annotations are only one level deep: the contents of
        // `"ClassVar[int]"` are an expression, not another string. Surrounding
        // whitespace is trimmed because `dataclasses` accepts it and the
        // parser would reject the leading indentation.
        Expr::StringLiteral(literal) => {
            parse_expression(literal.value.to_str().trim()).is_ok_and(|parsed| {
                match parsed.expr() {
                    Expr::StringLiteral(_) => false,
                    expression => annotates_class_var(expression),
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
fn field_call<'a>(value: &'a Expr, aliases: &Aliases) -> Option<(&'a ast::ExprCall, FieldCall)> {
    let Expr::Call(call) = value else {
        return None;
    };
    match matched_name(&call.func, aliases)? {
        "field" => Some((call, FieldCall::Dataclasses)),
        "Field" => Some((call, FieldCall::Pydantic)),
        _ => None,
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
) -> Option<FieldDefault> {
    let plain = || FieldDefault {
        kind: "default",
        fix: TextRange::new(annotation_end, value.end()),
        value: literal_text(value, source),
    };
    let Some((call, style)) = field_call(value, aliases) else {
        return Some(plain());
    };
    if let Some(first) = call.arguments.args.first() {
        // `x: int = Field(...)` is pydantic's way of writing a field with no
        // default at all, so there is nothing to report or remove.
        if style == FieldCall::Pydantic && first.is_ellipsis_literal_expr() {
            return None;
        }
        return Some(FieldDefault {
            kind: "default",
            fix: argument_removal_range(
                first.range(),
                call.arguments
                    .args
                    .get(1)
                    .map(Ranged::range)
                    .or_else(|| call.arguments.keywords.first().map(Ranged::range)),
                None,
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
        factory_call_text(&keyword.value)
    } else {
        literal_text(&keyword.value, source)
    };
    Some(FieldDefault {
        kind,
        value,
        fix: argument_removal_range(
            keyword.range(),
            call.arguments.keywords.get(index + 1).map(Ranged::range),
            index
                .checked_sub(1)
                .and_then(|previous| call.arguments.keywords.get(previous))
                .map(Ranged::range),
        ),
    })
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

/// What a method is given ahead of its written arguments, from its decorators.
fn method_receiver(function: &ast::StmtFunctionDef) -> Receiver {
    let decorated = |wanted: &str| {
        function
            .decorator_list
            .iter()
            .any(|decorator| matches!(&decorator.expression, Expr::Name(name) if name.id == wanted))
    };
    if decorated("staticmethod") {
        Receiver::None
    } else if decorated("classmethod") {
        Receiver::Class
    } else {
        Receiver::Instance
    }
}

/// The source text of a default that a call site can repeat verbatim.
///
/// Only self-contained literals qualify. A default such as `SENTINEL` or
/// `Path.cwd()` depends on names the caller may not have imported, and copying
/// it would change what the call means, so those are left to the reader.
fn literal_text(expression: &Expr, source: &str) -> Option<String> {
    is_literal(expression).then(|| {
        source[expression.range().start().to_usize()..expression.range().end().to_usize()]
            .to_owned()
    })
}

fn is_literal(expression: &Expr) -> bool {
    match expression {
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::UnaryOp(unary) => is_literal(&unary.operand),
        Expr::Tuple(tuple) => tuple.elts.iter().all(is_literal),
        Expr::List(list) => list.elts.iter().all(is_literal),
        Expr::Set(set) => set.elts.iter().all(is_literal),
        Expr::Dict(dict) => dict
            .items
            .iter()
            .all(|item| item.key.as_ref().is_some_and(is_literal) && is_literal(&item.value)),
        _ => false,
    }
}

/// The value a `default_factory` produces, for factories whose result can be
/// written as a literal. Any other factory is left alone.
fn factory_call_text(factory: &Expr) -> Option<String> {
    let Expr::Name(name) = factory else {
        return None;
    };
    match name.id.as_str() {
        "list" => Some("[]".to_owned()),
        "dict" => Some("{}".to_owned()),
        "tuple" => Some("()".to_owned()),
        "set" => Some("set()".to_owned()),
        "frozenset" => Some("frozenset()".to_owned()),
        _ => None,
    }
}

/// Find the calls in `source` that relied on a default `--fix` removed, and
/// build the edits that pass that default explicitly instead.
fn rewrite_calls(
    path: &Path,
    source: &str,
    definitions: &Definitions,
    known: &BTreeSet<&Path>,
) -> Result<FileCallSites, String> {
    let parsed = parse_module(source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let mut bindings = BTreeMap::new();
    collect_bindings(parsed.suite(), path, known, &mut bindings);
    let mut rewriter = Rewriter {
        path,
        source,
        definitions,
        bindings,
        classes: Vec::new(),
        called: BTreeSet::new(),
        scopes: Vec::new(),
        lines: LineIndex::new(source),
        edits: Vec::new(),
        skipped: Vec::new(),
    };
    for statement in parsed.suite() {
        rewriter.visit_stmt(statement);
    }
    Ok(FileCallSites {
        edits: rewriter.edits,
        skipped: rewriter.skipped,
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
                    // Without an alias, `import a.b` binds `a`, and only a
                    // single-component import gives a usable module name.
                    let bound = match alias.asname.as_ref() {
                        Some(name) => name.to_string(),
                        None if !module.contains('.') => module.to_owned(),
                        None => continue,
                    };
                    if let Some(file) = resolve_module(module, 0, importer, known) {
                        bindings.insert(bound, Binding::Module(file));
                    }
                }
            }
            Stmt::ImportFrom(import) => {
                let module = import.module.as_ref().map_or("", ast::Identifier::as_str);
                let Some(file) = resolve_module(module, import.level, importer, known) else {
                    continue;
                };
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
                    bindings.insert(
                        bound,
                        submodule.map_or_else(
                            || Binding::Symbol(file.clone(), name.to_owned()),
                            Binding::Module,
                        ),
                    );
                }
            }
            Stmt::FunctionDef(function) => {
                collect_bindings(&function.body, importer, known, bindings);
            }
            Stmt::ClassDef(class) => collect_bindings(&class.body, importer, known, bindings),
            Stmt::If(branch) => {
                collect_bindings(&branch.body, importer, known, bindings);
                for clause in &branch.elif_else_clauses {
                    collect_bindings(&clause.body, importer, known, bindings);
                }
            }
            Stmt::Try(block) => {
                collect_bindings(&block.body, importer, known, bindings);
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_bindings(&handler.body, importer, known, bindings);
                }
                collect_bindings(&block.orelse, importer, known, bindings);
                collect_bindings(&block.finalbody, importer, known, bindings);
            }
            _ => {}
        }
    }
}

/// The names a function or class body binds.
///
/// A call to a name bound here does not go to a module-level definition of the
/// same name, so it must not be rewritten as though it did. Where a name cannot
/// be ruled out — a binding in a branch never taken, a comprehension variable
/// that really has a scope of its own — it is collected anyway: over-collecting
/// costs a call left alone with a warning, and under-collecting costs a wrong
/// rewrite.
#[derive(Default)]
struct BoundNames(BTreeSet<String>);

impl BoundNames {
    /// Collect the names an assignment target binds. An attribute or subscript
    /// target rebinds nothing by its own name.
    fn bind(&mut self, target: &Expr) {
        match target {
            Expr::Name(name) => {
                self.0.insert(name.id.to_string());
            }
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|element| self.bind(element)),
            Expr::List(list) => list.elts.iter().for_each(|element| self.bind(element)),
            Expr::Starred(starred) => self.bind(&starred.value),
            _ => {}
        }
    }

    fn parameters(&mut self, parameters: &ast::Parameters) {
        for parameter in parameters
            .posonlyargs
            .iter()
            .chain(&parameters.args)
            .chain(&parameters.kwonlyargs)
        {
            self.0.insert(parameter.parameter.name.to_string());
        }
        for parameter in [&parameters.vararg, &parameters.kwarg]
            .into_iter()
            .flatten()
        {
            self.0.insert(parameter.name.to_string());
        }
    }

    fn comprehensions(&mut self, generators: &[ast::Comprehension]) {
        for generator in generators {
            self.bind(&generator.target);
        }
    }

    /// The names bound anywhere inside a function, including its parameters.
    fn of_function(function: &ast::StmtFunctionDef) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.parameters(&function.parameters);
        for statement in &function.body {
            collector.visit_stmt(statement);
        }
        collector.0
    }

    fn of_body(body: &[Stmt]) -> BTreeSet<String> {
        let mut collector = Self::default();
        for statement in body {
            collector.visit_stmt(statement);
        }
        collector.0
    }

    fn of_lambda(lambda: &ast::ExprLambda) -> BTreeSet<String> {
        let mut collector = Self::default();
        if let Some(parameters) = &lambda.parameters {
            collector.parameters(parameters);
        }
        collector.0
    }
}

impl<'a> Visitor<'a> for BoundNames {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Assign(assign) => assign.targets.iter().for_each(|target| self.bind(target)),
            Stmt::AnnAssign(assign) => self.bind(&assign.target),
            Stmt::AugAssign(assign) => self.bind(&assign.target),
            Stmt::For(loop_statement) => self.bind(&loop_statement.target),
            Stmt::With(block) => {
                for item in &block.items {
                    if let Some(target) = &item.optional_vars {
                        self.bind(target);
                    }
                }
            }
            Stmt::FunctionDef(function) => {
                self.0.insert(function.name.to_string());
                self.parameters(&function.parameters);
            }
            Stmt::ClassDef(class) => {
                self.0.insert(class.name.to_string());
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
                    self.0.insert(bound);
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map_or_else(|| alias.name.to_string(), ToString::to_string);
                    self.0.insert(bound);
                }
            }
            Stmt::Try(block) => {
                for handler in &block.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        self.0.insert(name.to_string());
                    }
                }
            }
            _ => {}
        }
        walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Named(named) => self.bind(&named.target),
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.parameters(parameters);
                }
            }
            Expr::ListComp(comprehension) => self.comprehensions(&comprehension.generators),
            Expr::SetComp(comprehension) => self.comprehensions(&comprehension.generators),
            Expr::DictComp(comprehension) => self.comprehensions(&comprehension.generators),
            Expr::Generator(comprehension) => self.comprehensions(&comprehension.generators),
            _ => {}
        }
        walk_expr(self, expression);
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
    path: &'a Path,
    source: &'a str,
    definitions: &'a Definitions,
    /// What each imported name in this file refers to.
    bindings: BTreeMap<String, Binding>,
    /// The class bodies being walked, so `self.method(...)` can be resolved.
    classes: Vec<String>,
    /// Ranges of the expressions being called, so that the same expression is
    /// not later mistaken for a reference that never calls the function.
    called: BTreeSet<(TextSize, TextSize)>,
    /// The names bound by each enclosing function, class, and lambda scope. A
    /// name bound in one of them shadows a module-level definition, so a call
    /// to it does not go where the definition went.
    scopes: Vec<BTreeSet<String>>,
    /// Where each line of `source` starts, for the same reason the checker
    /// keeps one.
    lines: LineIndex,
    edits: Vec<Edit>,
    skipped: Vec<Skipped>,
}

impl Rewriter<'_> {
    /// Whether an enclosing function, class, or lambda binds `name`, so a call
    /// to it does not reach a definition of the same name elsewhere.
    fn shadowed(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.contains(name))
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
    fn receiving_class(&self, receiver: &Expr) -> Option<(PathBuf, String, bool)> {
        if let Expr::Name(name) = receiver {
            if matches!(name.id.as_str(), "self" | "cls") {
                return Some((self.path.to_path_buf(), self.classes.last()?.clone(), true));
            }
            return match self.bindings.get(name.id.as_str()) {
                // `from api import Client` names a class of another file.
                Some(Binding::Symbol(file, symbol)) => Some((file.clone(), symbol.clone(), false)),
                Some(Binding::Module(_)) => None,
                None => Some((self.path.to_path_buf(), name.id.to_string(), false)),
            };
        }
        let Expr::Attribute(attribute) = receiver else {
            return None;
        };
        let dotted = dotted_name(&attribute.value)?;
        match self.bindings.get(&dotted)? {
            Binding::Module(file) => Some((file.clone(), attribute.attr.to_string(), false)),
            Binding::Symbol(..) => None,
        }
    }

    /// The callable an expression names, when the file's own imports say so,
    /// and how many parameters the call has already been given implicitly.
    fn resolve(&self, expression: &Expr) -> Option<(&Signature, usize)> {
        match expression {
            // A bare name is either defined in this file or imported into it —
            // unless an enclosing scope binds it, in which case it is neither.
            Expr::Name(name) if self.shadowed(name.id.as_str()) => None,
            Expr::Name(name) => match self.bindings.get(name.id.as_str()) {
                Some(Binding::Symbol(file, symbol)) => Some((
                    self.definitions.symbols.get(file)?.get(symbol)?.as_ref()?,
                    0,
                )),
                Some(Binding::Module(_)) => None,
                None => Some((
                    self.definitions
                        .symbols
                        .get(self.path)?
                        .get(name.id.as_str())?
                        .as_ref()?,
                    0,
                )),
            },
            Expr::Attribute(attribute) => {
                // A method's receiver type is only known when it is `self`,
                // `cls`, or a class this file can name.
                if let Some((file, class, through_instance)) =
                    self.receiving_class(&attribute.value)
                {
                    if let Some(method) = self
                        .definitions
                        .methods
                        .get(&(file, class))
                        .and_then(|methods| methods.get(attribute.attr.as_str()))
                    {
                        let signature = method.as_ref()?;
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
                }
                let dotted = dotted_name(&attribute.value)?;
                let Some(Binding::Module(file)) = self.bindings.get(&dotted) else {
                    return None;
                };
                Some((
                    self.definitions
                        .symbols
                        .get(file)?
                        .get(attribute.attr.as_str())?
                        .as_ref()?,
                    0,
                ))
            }
            _ => None,
        }
    }

    fn check_call(&mut self, call: &ast::ExprCall) {
        let name = match &*call.func {
            Expr::Name(name) => name.id.as_str(),
            Expr::Attribute(attribute) => attribute.attr.as_str(),
            _ => return,
        };
        let Some((signature, bound)) = self.resolve(&call.func) else {
            // A name a fixed callable also goes by, reached some other way: an
            // unrelated `connect`, a method on a receiver whose type is not
            // known, or a call through an unresolved import. Rewriting it would
            // break working code, so say so instead.
            if self.definitions.names.contains(name) {
                self.skip(
                    call.start(),
                    name,
                    "this call cannot be tied to the definition that was fixed".to_owned(),
                );
            }
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
        if arguments.is_empty() {
            return;
        }
        let arguments = arguments.join(", ");
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
        });
    }
}

/// The arguments a call must gain to keep meaning what it meant before the
/// defaults were removed, or the reason the call has to be left alone.
fn missing_arguments(
    call: &ast::Arguments,
    signature: &Signature,
    bound: usize,
) -> Result<Vec<String>, String> {
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
    // Python allows a bare generator expression as an argument only when it is
    // the sole one, so appending anything after it would not parse.
    if call
        .args
        .iter()
        .any(|argument| matches!(argument, Expr::Generator(generator) if !generator.parenthesized))
    {
        return Err(
            "the call's argument is a bare generator expression, which Python allows only \
             when it is the only one"
                .to_owned(),
        );
    }
    let positional = call.args.len();
    let named: Vec<&str> = call
        .keywords
        .iter()
        .filter_map(|keyword| keyword.arg.as_ref().map(ast::Identifier::as_str))
        .collect();
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
            // A positional-only argument can only be appended when it fills the
            // very next slot and no keyword argument precedes it.
            if !call.keywords.is_empty() || slot != Some(bound + positional + appended.len()) {
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
    appended.extend(keywords);
    Ok(appended)
}

impl<'a> Visitor<'a> for Rewriter<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::ClassDef(class) => {
                self.classes.push(class.name.to_string());
                self.scopes.push(BoundNames::of_body(&class.body));
                walk_stmt(self, statement);
                self.scopes.pop();
                self.classes.pop();
            }
            Stmt::FunctionDef(function) => {
                self.scopes.push(BoundNames::of_function(function));
                walk_stmt(self, statement);
                self.scopes.pop();
            }
            _ => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Call(call) => {
                self.called.insert((call.func.start(), call.func.end()));
                self.check_call(call);
            }
            Expr::Name(_) | Expr::Attribute(_) => self.check_reference(expression),
            Expr::Lambda(lambda) => {
                self.scopes.push(BoundNames::of_lambda(lambda));
                walk_expr(self, expression);
                self.scopes.pop();
                return;
            }
            _ => {}
        }
        walk_expr(self, expression);
    }
}

/// The offset of the first character of the line containing `offset`.
fn line_start(source: &str, offset: TextSize) -> TextSize {
    text_size(
        source[..offset.to_usize()]
            .rfind('\n')
            .map_or(0, |end| end + 1),
    )
}

/// The offset each line of a source starts at.
///
/// Built once per file, so converting an offset to a line is a binary search
/// rather than a scan from the top of the file. Scanning made producing a
/// file's diagnostics quadratic in how many it holds.
struct LineIndex(Vec<usize>);

impl LineIndex {
    fn new(source: &str) -> Self {
        Self(
            std::iter::once(0)
                .chain(source.match_indices('\n').map(|(position, _)| position + 1))
                .collect(),
        )
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
            )?;
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
                .push(Edit {
                    range,
                    replacement: String::new(),
                });
        }
        let mut updated = 0;
        write_fixes_atomically(fixed_sources(call_sites.edits, &mut updated)?)?;
        for path in files {
            assert!(
                check_file(
                    path,
                    false,
                    Path::new(""),
                    &Reexports::default(),
                    &default_bases(),
                    false,
                )?
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
    fn detects_every_parameter_kind() {
        let found = messages("def f(a=1, /, b=2, *, c=3): pass\n", false);
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn detects_dataclass_defaults_but_not_class_vars() {
        let found = messages(
            "@dataclass\nclass C:\n x: int = 1\n y: list = field(default_factory=list)\n z: ClassVar[int] = 2\n no_default: int = field()\n",
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
    fn an_alias_from_an_unrelated_module_is_not_resolved() {
        assert!(
            messages(
                "from elsewhere import thing as dataclass\n\n@dataclass\nclass C:\n    x: int = 1\n",
                false
            )
            .len()
                == 1,
            "a bare `dataclass` is still matched by name, as it always was"
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
    fn class_level_assignments_that_declare_no_field_are_left_alone() {
        let found = messages(
            "class Job(BaseModel):\n model_config = ConfigDict(frozen=True)\n kind: ClassVar[str] = \"job\"\n def build(self) -> None:\n  seen: set[str] = set()\n",
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
            "@dataclass\nclass C:\n a: \"ClassVar[int]\" = 1\n b: \"typing.ClassVar[int]\" = 2\n c: 'ClassVar' = 3\n d: \"  ClassVar[int]  \" = 4\n",
            false,
        );
        assert!(found.is_empty(), "{found:?}");
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
            "import os\nfrom ._internal import upload as send\n__all__ = [\"send\", \"Job\"]\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            internal.join("__init__.py"),
            "from ._upload import buried\n",
        )
        .map_err(|error| error.to_string())?;
        let reexports = package_reexports(&internal, directory.path(), &mut BTreeMap::new())?.names;
        assert!(reexports.covers("send"));
        assert!(reexports.covers("Job"));
        assert!(reexports.covers("os"));
        assert!(
            !reexports.covers("buried"),
            "a private package re-exports nothing to the outside"
        );
        assert!(!reexports.wildcard);
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
            package_reexports(&root, directory.path(), &mut BTreeMap::new())?
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
        std::fs::write(root.join("__init__.py"), "from . import _internal\n")
            .map_err(|error| error.to_string())?;
        std::fs::write(
            internal.join("__init__.py"),
            "from ._upload import reachable\n",
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(deeper.join("__init__.py"), "from ._mod import deep\n")
            .map_err(|error| error.to_string())?;
        let mut cache = BTreeMap::new();
        let reexports = package_reexports(&deeper, directory.path(), &mut cache)?.names;
        assert!(reexports.covers("reachable"));
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
        let reexports = package_reexports(&data, directory.path(), &mut BTreeMap::new())?.names;
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
        let reexports = package_reexports(&package, &root, &mut BTreeMap::new())?.names;
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
        let package = package_reexports(&deeper, directory.path(), &mut BTreeMap::new())?;
        assert!(package.sealed);
        assert!(package.names.covers("shown"));
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
        let reexports = package_reexports(&root, directory.path(), &mut BTreeMap::new())?.names;
        assert!(reexports.covers("upload"));
        assert!(
            !is_private_module(Path::new("package/_upload.pyi"), Path::new(""), &reexports),
            "a stub's extension must not hide the module name"
        );
        assert!(is_private_module(
            Path::new("package/_other.pyi"),
            Path::new(""),
            &reexports
        ));
        Ok(())
    }

    #[test]
    fn a_file_outside_a_package_has_no_reexports() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let reexports =
            package_reexports(directory.path(), directory.path(), &mut BTreeMap::new())?.names;
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
            )?
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
        assert_eq!(
            fixed("def f(x): pass  # noqa: NOD001\r\n")?,
            "def f(x): pass\r\n"
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
    fn a_positional_only_argument_is_never_appended_after_a_keyword() -> Result<(), String> {
        assert_eq!(
            skipped_reasons("def f(a=1, /, *, b=2): pass\nf(b=3)\n")?
                .first()
                .map(String::as_str),
            Some("`a` is positional-only and cannot be appended to this call"),
            "appending `1` after `b=3` would not parse"
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
    fn a_function_named_without_being_called_is_reported() -> Result<(), String> {
        assert_eq!(
            skipped_reasons("def deco(cls=None): return cls\n\n\n@deco\ndef f(): pass\n")?
                .first()
                .map(String::as_str),
            Some(
                "it is named here without being called, so the removed default cannot be supplied"
            ),
            "a bare decorator calls the function with no argument list to add to"
        );
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
    fn every_shape_that_binds_a_name_shadows_it() -> Result<(), String> {
        for binding in [
            "connect = open",
            "for connect in []: pass",
            "with open(\"f\") as connect: pass",
            "import connect",
            "from os import path as connect",
            "def connect(): pass",
            "class connect: pass",
            "[connect for connect in []]",
            "if (connect := open): pass",
            "(lambda connect: connect(\"h\"))(open)",
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
    fn a_class_body_binding_shadows_a_module_level_definition() -> Result<(), String> {
        let source = "def connect(host, timeout=30):\n    pass\n\n\n\
                      class Client:\n    connect = staticmethod(open)\n\n    \
                      def run(self):\n        connect(\"h\")\n";
        assert_eq!(
            fixed(source)?,
            source.replace("host, timeout=30", "host, timeout")
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
    fn a_kw_only_marker_makes_the_fields_after_it_keyword_only() -> Result<(), String> {
        let source =
            "@dataclass\nclass E:\n    a: int = 1\n    _: KW_ONLY\n    b: int = 2\n\n\nE()\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass\nclass E:\n    a: int\n    _: KW_ONLY\n    b: int\n\n\nE(a=1, b=2)\n"
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
    fn a_field_kept_out_of_the_constructor_is_not_passed() -> Result<(), String> {
        // `init=False` keeps the field out of `__init__`, so naming it in a
        // call would raise `TypeError: unexpected keyword argument`.
        assert_eq!(
            fixed(
                "@dataclass\nclass C:\n    x: int = 1\n    y: int = field(default=5, init=False)\n\n\nC()\n"
            )?,
            "@dataclass\nclass C:\n    x: int\n    y: int = field(init=False)\n\n\nC(x=1)\n"
        );
        Ok(())
    }

    #[test]
    fn a_kw_only_marker_takes_no_argument_position() -> Result<(), String> {
        // `_: KW_ONLY` declares no field, so it must not consume the slot that
        // tells whether `b` was already supplied positionally.
        assert_eq!(
            fixed("@dataclass\nclass C:\n    a: int\n    _: KW_ONLY\n    b: int = 2\n\n\nC(1)\n")?,
            "@dataclass\nclass C:\n    a: int\n    _: KW_ONLY\n    b: int\n\n\nC(1, b=2)\n"
        );
        Ok(())
    }

    #[test]
    fn a_class_without_a_generated_constructor_is_left_alone() -> Result<(), String> {
        let source = "@dataclass(init=False)\nclass C:\n    x: int = 1\n\n\nC()\n";
        assert_eq!(
            fixed(source)?,
            "@dataclass(init=False)\nclass C:\n    x: int\n\n\nC()\n"
        );
        Ok(())
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
}
