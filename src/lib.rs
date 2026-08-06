use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

#[derive(Clone, Debug, Default, Deserialize)]
struct Config {
    #[serde(default, alias = "private-only")]
    private_only: bool,

    #[serde(default, alias = "per-file-enforcement")]
    per_file_enforcement: BTreeMap<String, Enforcement>,
}

#[derive(Clone)]
struct LoadedConfig {
    root: PathBuf,
    config: Config,
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
    fix: TextRange,
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
    let settings = settings_for_files(&files, cli.private_only)?;
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
                |private_only| check_file(path, private_only, fixing),
            )
        })
        .collect();
    let mut diagnostics = Vec::new();
    let mut signatures = Vec::new();
    for result in results {
        let checked = result?;
        diagnostics.extend(checked.diagnostics);
        signatures.extend(checked.signatures);
    }
    diagnostics.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    if fixing && !diagnostics.is_empty() {
        let mut call_sites = call_site_edits(&files, signatures)?;
        for diagnostic in &diagnostics {
            call_sites
                .edits
                .entry(diagnostic.path.clone())
                .or_default()
                .push(Edit {
                    range: diagnostic.fix,
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
        println!(
            "Found {} error{} ({} fixed, 0 remaining).",
            diagnostics.len(),
            if diagnostics.len() == 1 { "" } else { "s" },
            diagnostics.len()
        );
        report_call_sites(&diagnostics, &call_sites.skipped, updated);
        return Ok(false);
    }
    report_diagnostics(&diagnostics, cli.output_format)?;
    Ok(!diagnostics.is_empty())
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
fn report_call_sites(diagnostics: &[Diagnostic], skipped: &[Skipped], updated: usize) {
    let removed = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "NOD001")
        .count();
    if removed == 0 {
        return;
    }
    println!(
        "Updated {updated} call site{}.",
        if updated == 1 { "" } else { "s" }
    );
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
            let source = std::fs::read_to_string(path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            rewrite_calls(path, &source, &definitions, &known)
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
/// Absolute imports are matched by path suffix, because the import root is not
/// known; an ambiguous suffix resolves to nothing rather than to a guess.
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

#[derive(Serialize)]
struct JsonDiagnostic<'a> {
    path: String,
    line: usize,
    column: usize,
    code: &'static str,
    message: &'a str,
}

fn report_diagnostics(diagnostics: &[Diagnostic], format: OutputFormat) -> Result<(), String> {
    match format {
        OutputFormat::Full => {
            for diagnostic in diagnostics {
                println!(
                    "{}:{}:{}: {} {}",
                    diagnostic.path.display(),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.code,
                    diagnostic.message
                );
                if let Ok(source) = std::fs::read_to_string(&diagnostic.path) {
                    if let Some(line) = source.lines().nth(diagnostic.line.saturating_sub(1)) {
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
}

fn settings_for_files(
    files: &[PathBuf],
    cli_private_only: bool,
) -> Result<Vec<FileSettings>, String> {
    let fallback_root = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut directory_cache: BTreeMap<PathBuf, Option<PathBuf>> = BTreeMap::new();
    let mut config_cache: BTreeMap<PathBuf, LoadedConfig> = BTreeMap::new();
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
            }
        };
        let overrides = compile_overrides(&loaded.config.per_file_enforcement)?;
        settings.push(FileSettings {
            project_root: loaded.root.clone(),
            private_only: private_only_for(path, &loaded, &overrides)
                .map(|private_only| cli_private_only || private_only),
        });
    }
    Ok(settings)
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
    let config = table
        .try_into()
        .map_err(|error| format!("invalid [tool.no_defaults] in {}: {error}", path.display()))?;
    Ok(LoadedConfig {
        root: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
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
            let source = std::fs::read_to_string(&path).map_err(|error| {
                format!("could not read {} for fixing: {error}", path.display())
            })?;
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
fn apply_edits(source: &str, mut edits: Vec<Edit>) -> (String, usize) {
    let deletions: Vec<TextRange> = edits
        .iter()
        .filter(|edit| edit.replacement.is_empty())
        .map(|edit| edit.range)
        .collect();
    edits.retain(|edit| {
        edit.replacement.is_empty()
            || !deletions
                .iter()
                .any(|deletion| deletion.contains(edit.range.start()))
    });
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start()));
    let applied = edits
        .iter()
        .filter(|edit| !edit.replacement.is_empty())
        .count();
    let mut fixed = source.to_owned();
    for edit in edits {
        fixed.replace_range(
            edit.range.start().to_usize()..edit.range.end().to_usize(),
            &edit.replacement,
        );
    }
    (fixed, applied)
}

fn write_fixes_atomically(changes: BTreeMap<PathBuf, (String, String)>) -> Result<(), String> {
    for (path, (_, fixed)) in changes {
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

fn private_only_for(
    path: &Path,
    loaded: &LoadedConfig,
    overrides: &[PerFileEnforcement],
) -> Option<bool> {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = resolved.strip_prefix(&loaded.root).unwrap_or(&resolved);
    let filename = relative.file_name().unwrap_or_default();
    let selected = overrides
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
            if is_python(path) {
                files.push(path.clone());
            }
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
    files.dedup();
    Ok(files)
}

fn is_python(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "py" || extension == "pyi")
}

/// Lint Python source without filesystem or configuration-discovery overhead.
///
/// This is primarily exposed for performance benchmarks.
#[doc(hidden)]
pub fn lint_source(source: &str, private_only: bool) -> Result<usize, String> {
    Ok(
        check_source(Path::new("benchmark.py"), source, private_only, false)?
            .diagnostics
            .len(),
    )
}

fn check_file(path: &Path, private_only: bool, signatures: bool) -> Result<Checked, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    check_source(path, &source, private_only, signatures)
}

/// Check one file. `signatures` records what each fixed callable looks like so
/// call sites can be updated; it costs allocation per parameter and per field,
/// so reporting runs leave it off.
fn check_source(
    path: &Path,
    source: &str,
    private_only: bool,
    signatures: bool,
) -> Result<Checked, String> {
    let parsed = parse_module(source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let directives = collect_directives(source, parsed.tokens());
    // A blanket file-level directive silences every rule for the file,
    // including the unused-directive rule itself.
    if directives
        .iter()
        .any(|directive| directive.file_level && !directive.explicit)
    {
        return Ok(Checked::default());
    }
    let mut checker = Checker {
        path,
        source,
        private_only,
        scope: Scope {
            private: is_private_module(path),
            ..Scope::default()
        },
        header: None,
        collect_signatures: signatures,
        classes: Vec::new(),
        signatures: Vec::new(),
        directives,
        diagnostics: Vec::new(),
    };
    for statement in parsed.suite() {
        checker.visit_stmt(statement);
    }
    Ok(checker.finish())
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
    let (file_level, rest) = if alone && lower.starts_with("flake8: noqa") {
        (lower == "flake8: noqa", None)
    } else if let Some(rest) = lower.strip_prefix("ruff: noqa").filter(|_| alone) {
        (true, Some((rest, body_start + "ruff: noqa".len())))
    } else {
        (false, Some((lower.strip_prefix("noqa")?, body_start + 4)))
    };
    let line = TextRange::new(text_size(line_start), text_size(content_end));
    let blanket = || Directive {
        line,
        start: text_size(hash),
        explicit: false,
        fix: TextRange::empty(text_size(hash)),
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
        whole_directive_range(source, line_start, hash, break_start, content_end)
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
    scope: Scope,
    /// Start of the `def` or `class` line that owns the violations being
    /// reported, so one directive there can cover every parameter of a
    /// signature or every field of a dataclass.
    header: Option<TextSize>,
    /// Whether to record what each fixed callable looks like. Only `--fix` and
    /// `--diff` use it, and building it allocates per parameter and per field.
    collect_signatures: bool,
    classes: Vec<ClassCollector>,
    signatures: Vec<Signature>,
    directives: Vec<Directive>,
    diagnostics: Vec<Diagnostic>,
}

/// What the enclosing definitions say about the statement being visited.
#[derive(Clone, Copy, Default)]
struct Scope {
    /// Whether an enclosing module, class, or function is private.
    private: bool,
    /// Whether assignments here are dataclass fields.
    dataclass: bool,
    /// Whether definitions here sit directly in a class body.
    class_body: bool,
}

/// The fields of a class body, gathered so a dataclass can be given a signature
/// once its body has been walked.
struct ClassCollector {
    name: String,
    dataclass: bool,
    /// Whether the class has base classes, whose fields come first in the
    /// constructor and are not visible from this class body.
    inherits: bool,
    /// Whether the decorator generates a constructor at all.
    constructs: bool,
    fields: Vec<String>,
    removed: Vec<Removed>,
}

impl Checker<'_> {
    fn enabled(&self, name: &str) -> bool {
        !self.private_only || self.scope.private || is_private(name)
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
    fn report(&mut self, offset: TextSize, message: String, fix: TextRange) -> bool {
        if self.suppress(offset) {
            return false;
        }
        let (line, column) = line_column(self.source, offset);
        self.diagnostics.push(Diagnostic {
            path: self.path.to_path_buf(),
            line,
            column,
            code: "NOD001",
            message,
            fix,
        });
        true
    }

    /// Add a diagnostic for every directive that named `NOD001` without
    /// suppressing anything, then return the file's diagnostics in order.
    fn finish(mut self) -> Checked {
        for directive in &self.directives {
            if !directive.explicit || directive.used {
                continue;
            }
            let (line, column) = line_column(self.source, directive.start);
            self.diagnostics.push(Diagnostic {
                path: self.path.to_path_buf(),
                line,
                column,
                code: "NOD002",
                message: "unused `noqa` directive for `NOD001`".to_owned(),
                fix: directive.fix,
            });
        }
        self.diagnostics
            .sort_by_key(|diagnostic| (diagnostic.line, diagnostic.column));
        Checked {
            diagnostics: self.diagnostics,
            signatures: self.signatures,
        }
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
                    default.start(),
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

    fn check_dataclass_field(&mut self, statement: &Stmt) {
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
        // constructor's positional order.
        let pseudo_field = annotates_kw_only(&assign.annotation);
        // A field declared `init=False` is not a constructor parameter, so a
        // call must never be given it.
        let constructs = assign
            .value
            .as_deref()
            .is_none_or(|value| !field_excluded_from_init(value));
        // Every other field counts towards the positional order, even one
        // without a default or one this run is not enforcing.
        if self.collect_signatures && !pseudo_field && constructs {
            if let Some(class) = self.classes.last_mut() {
                class.fields.push(name.id.to_string());
            }
        }
        let Some(value) = assign.value.as_deref() else {
            return;
        };
        if !self.enabled(name.id.as_str()) {
            return;
        }
        let Some(default) = field_default(value, assign.annotation.end(), self.source) else {
            return;
        };
        if self.report(
            value.start(),
            format!("dataclass field `{}` has a {}", name.id, default.kind),
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
                    private: outer.private || is_private(function.name.as_str()),
                    // Annotated assignments in a method body are locals, not
                    // fields, so field detection stops at the function boundary.
                    dataclass: false,
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
                self.scope = Scope {
                    private: outer.private || is_private(class.name.as_str()),
                    dataclass: has_dataclass_decorator(class),
                    class_body: true,
                };
                if self.collect_signatures {
                    self.classes.push(ClassCollector {
                        name: class.name.to_string(),
                        dataclass: self.scope.dataclass,
                        inherits: class
                            .arguments
                            .as_ref()
                            .is_some_and(|arguments| !arguments.args.is_empty()),
                        constructs: generates_init(class),
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
                    if collector.dataclass && collector.constructs && !collector.removed.is_empty()
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
            _ if self.scope.dataclass => {
                self.check_dataclass_field(statement);
                walk_stmt(self, statement);
            }
            _ => walk_stmt(self, statement),
        }
    }
}

fn is_private(name: &str) -> bool {
    name.starts_with('_') && !name.starts_with("__")
}

fn is_private_module(path: &Path) -> bool {
    path.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        let name = component.strip_suffix(".py").unwrap_or(&component);
        is_private(name)
    })
}

fn has_dataclass_decorator(class: &ast::StmtClassDef) -> bool {
    class.decorator_list.iter().any(|decorator| {
        let expression = match &decorator.expression {
            Expr::Call(call) => &*call.func,
            expression => expression,
        };
        matches!(expression, Expr::Name(name) if name.id.as_str() == "dataclass")
            || matches!(expression, Expr::Attribute(attribute) if attribute.attr.as_str() == "dataclass")
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

/// Whether an annotation is the `KW_ONLY` marker, which declares no field.
fn annotates_kw_only(annotation: &Expr) -> bool {
    matches!(annotation, Expr::Name(name) if name.id.as_str() == "KW_ONLY")
        || matches!(annotation, Expr::Attribute(attribute) if attribute.attr.as_str() == "KW_ONLY")
}

/// Whether a field is declared `init=False`, which keeps it out of the
/// constructor even though it is still a field.
fn field_excluded_from_init(value: &Expr) -> bool {
    let Expr::Call(call) = value else {
        return false;
    };
    let is_field = matches!(&*call.func, Expr::Name(name) if name.id.as_str() == "field")
        || matches!(&*call.func, Expr::Attribute(attribute) if attribute.attr.as_str() == "field");
    is_field
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

struct FieldDefault {
    kind: &'static str,
    fix: TextRange,
    /// Source text to pass at call sites, when it can be reproduced.
    value: Option<String>,
}

fn field_default(value: &Expr, annotation_end: TextSize, source: &str) -> Option<FieldDefault> {
    let plain = || FieldDefault {
        kind: "default",
        fix: TextRange::new(annotation_end, value.end()),
        value: literal_text(value, source),
    };
    let Expr::Call(call) = value else {
        return Some(plain());
    };
    let is_field = matches!(&*call.func, Expr::Name(name) if name.id.as_str() == "field")
        || matches!(&*call.func, Expr::Attribute(attribute) if attribute.attr.as_str() == "field");
    if !is_field {
        return Some(plain());
    }
    if let Some(first) = call.arguments.args.first() {
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
                    // symbol, when it resolves to a file of its own.
                    let submodule = import
                        .module
                        .as_ref()
                        .map(|parent| format!("{}.{name}", parent.as_str()))
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
    edits: Vec<Edit>,
    skipped: Vec<Skipped>,
}

impl Rewriter<'_> {
    fn skip(&mut self, offset: TextSize, callable: &str, reason: String) {
        let (line, column) = line_column(self.source, offset);
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

    /// The callable an expression names, when the file's own imports say so,
    /// and how many parameters the call has already been given implicitly.
    fn resolve(&self, expression: &Expr) -> Option<(&Signature, usize)> {
        match expression {
            // A bare name is either defined in this file or imported into it.
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
                // `cls`, or the class's own name in the file that defines it.
                if let Expr::Name(receiver) = &*attribute.value {
                    let through_instance = matches!(receiver.id.as_str(), "self" | "cls");
                    let class = if through_instance {
                        self.classes.last()?.clone()
                    } else {
                        receiver.id.to_string()
                    };
                    if let Some(method) = self
                        .definitions
                        .methods
                        .get(&(self.path.to_path_buf(), class))
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
        if let Stmt::ClassDef(class) = statement {
            self.classes.push(class.name.to_string());
            walk_stmt(self, statement);
            self.classes.pop();
            return;
        }
        walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Call(call) => {
                self.called.insert((call.func.start(), call.func.end()));
                self.check_call(call);
            }
            Expr::Name(_) | Expr::Attribute(_) => self.check_reference(expression),
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

fn line_column(source: &str, offset: TextSize) -> (usize, usize) {
    let offset = offset.to_usize();
    let before = &source[..offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |position| position + 1);
    (line, offset - line_start + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(source: &str, private_only: bool) -> Result<Vec<String>, String> {
        Ok(
            check_source(Path::new("fixture.py"), source, private_only, false)?
                .diagnostics
                .into_iter()
                .map(|item| item.message)
                .collect(),
        )
    }

    fn codes(source: &str) -> Result<Vec<&'static str>, String> {
        Ok(check_source(Path::new("fixture.py"), source, false, false)?
            .diagnostics
            .into_iter()
            .map(|item| item.code)
            .collect())
    }

    /// Fix every file in `directory` the way `--fix` does, and report the calls
    /// that were left alone.
    fn fix_all(files: &[PathBuf]) -> Result<Vec<Skipped>, String> {
        let mut diagnostics = Vec::new();
        let mut signatures = Vec::new();
        for path in files {
            let checked = check_file(path, false, true)?;
            diagnostics.extend(checked.diagnostics);
            signatures.extend(checked.signatures);
        }
        let mut call_sites = call_site_edits(files, signatures)?;
        for diagnostic in &diagnostics {
            call_sites
                .edits
                .entry(diagnostic.path.clone())
                .or_default()
                .push(Edit {
                    range: diagnostic.fix,
                    replacement: String::new(),
                });
        }
        let mut updated = 0;
        write_fixes_atomically(fixed_sources(call_sites.edits, &mut updated)?)?;
        for path in files {
            assert!(
                check_file(path, false, false)?.diagnostics.is_empty(),
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
    fn detects_every_parameter_kind() -> Result<(), String> {
        let found = messages("def f(a=1, /, b=2, *, c=3): pass\n", false)?;
        assert_eq!(found.len(), 3);
        Ok(())
    }

    #[test]
    fn detects_dataclass_defaults_but_not_class_vars() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n x: int = 1\n y: list = field(default_factory=list)\n z: ClassVar[int] = 2\n no_default: int = field()\n",
            false,
        )?;
        assert_eq!(found.len(), 2);
        assert!(found[1].contains("default factory"));
        Ok(())
    }

    #[test]
    fn quoted_class_var_annotations_are_not_fields() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n a: \"ClassVar[int]\" = 1\n b: \"typing.ClassVar[int]\" = 2\n c: 'ClassVar' = 3\n d: \"  ClassVar[int]  \" = 4\n",
            false,
        )?;
        assert!(found.is_empty(), "{found:?}");
        Ok(())
    }

    #[test]
    fn quoted_non_class_var_annotations_are_still_fields() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n a: \"int\" = 1\n b: \"NotClassVar[int]\" = 2\n c: \"((\" = 3\n",
            false,
        )?;
        assert_eq!(found.len(), 3, "{found:?}");
        Ok(())
    }

    #[test]
    fn ignores_annotated_locals_in_dataclass_methods() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n x: int = 1\n def _validate(self) -> None:\n  seen: set[str] = set()\n  def inner() -> None:\n   nested: int = 0\n",
            false,
        )?;
        assert_eq!(found, ["dataclass field `x` has a default"]);
        Ok(())
    }

    #[test]
    fn ignores_annotated_locals_in_async_dataclass_methods() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n async def _fetch(self) -> None:\n  chunks: list[str] = []\n",
            false,
        )?;
        assert!(found.is_empty(), "{found:?}");
        Ok(())
    }

    #[test]
    fn detects_fields_of_dataclass_nested_in_a_method() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n def build(self) -> None:\n  local: int = 0\n  @dataclass\n  class Inner:\n   y: int = 2\n",
            false,
        )?;
        assert_eq!(found, ["dataclass field `y` has a default"]);
        Ok(())
    }

    #[test]
    fn resumes_field_detection_after_a_method() -> Result<(), String> {
        let found = messages(
            "@dataclass\nclass C:\n def _validate(self) -> None:\n  local: int = 0\n after: int = 3\n",
            false,
        )?;
        assert_eq!(found, ["dataclass field `after` has a default"]);
        Ok(())
    }

    #[test]
    fn private_only_checks_private_symbols() -> Result<(), String> {
        let found = messages(
            "def public(x=1): pass\ndef _private(x=1): pass\n@dataclass\nclass C:\n public: int = 1\n _private: int = 2\n",
            true,
        )?;
        assert_eq!(found.len(), 2);
        Ok(())
    }

    #[test]
    fn noqa_suppresses_blanket_and_selected_violations() -> Result<(), String> {
        let found = messages(
            "def a(x=1): pass  # noqa\ndef b(x=1): pass  # noqa: E501, NOD001\ndef c(x=1): pass  # noqa: E501\n",
            false,
        )?;
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("function `c`"));
        Ok(())
    }

    #[test]
    fn recognizes_private_modules_and_packages() {
        assert!(is_private_module(Path::new("src/_module.py")));
        assert!(is_private_module(Path::new("src/_package/module.py")));
        assert!(is_private_module(Path::new("src/_package/__init__.py")));
        assert!(!is_private_module(Path::new("src/package/__init__.py")));
        assert!(!is_private_module(Path::new("src/package/module.py")));
    }

    #[test]
    fn per_file_enforcement_supports_tests_all_src_private() -> Result<(), String> {
        let loaded = LoadedConfig {
            root: PathBuf::from("project"),
            config: Config {
                private_only: false,
                per_file_enforcement: BTreeMap::from([
                    ("src/**".to_owned(), Enforcement::Private),
                    ("tests/**".to_owned(), Enforcement::All),
                ]),
            },
        };
        let overrides = compile_overrides(&loaded.config.per_file_enforcement)?;
        assert_eq!(
            private_only_for(Path::new("project/tests/test_api.py"), &loaded, &overrides),
            Some(false)
        );
        assert_eq!(
            private_only_for(Path::new("project/src/package/api.py"), &loaded, &overrides),
            Some(true)
        );
        assert_eq!(
            private_only_for(Path::new("project/scripts/release.py"), &loaded, &overrides),
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn none_enforcement_exempts_matching_files() -> Result<(), String> {
        let loaded = LoadedConfig {
            root: PathBuf::from("project"),
            config: Config {
                private_only: true,
                per_file_enforcement: BTreeMap::from([(
                    "src/package/_compat.py".to_owned(),
                    Enforcement::None,
                )]),
            },
        };
        let overrides = compile_overrides(&loaded.config.per_file_enforcement)?;
        assert_eq!(
            private_only_for(
                Path::new("project/src/package/_compat.py"),
                &loaded,
                &overrides
            ),
            None
        );
        // Files the pattern does not name keep the project-wide setting.
        assert_eq!(
            private_only_for(Path::new("project/src/package/api.py"), &loaded, &overrides),
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
        let settings = settings_for_files(&[exempt], true)?;
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
        assert_eq!(check_file(&path, false, false)?.diagnostics.len(), 5);
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
    fn file_level_noqa_suppresses_rule() -> Result<(), String> {
        assert!(codes("# ruff: noqa: NOD001\ndef f(x=1): pass\n")?.is_empty());
        assert_eq!(codes("# ruff: noqa: E501\ndef f(x=1): pass\n")?, ["NOD001"]);
        assert!(codes("# ruff: noqa\ndef f(x=1): pass\n")?.is_empty());
        assert!(codes("# flake8: noqa\ndef f(x=1): pass\n")?.is_empty());
        Ok(())
    }

    #[test]
    fn a_directive_on_the_def_line_covers_the_whole_signature() -> Result<(), String> {
        assert!(
            codes("def f(  # noqa: NOD001\n    a=1,\n    b=2,\n):\n    pass\n")?.is_empty(),
            "one directive covers every parameter of the signature"
        );
        assert!(
            codes("@decorator\nasync def f(  # noqa: NOD001\n    a=1,\n) -> None:\n    pass\n")?
                .is_empty(),
            "decorators do not move the `def` line"
        );
        assert!(
            codes("def f(  # noqa\n    a=1,\n):\n    pass\n")?.is_empty(),
            "a blanket directive on the `def` line covers the signature too"
        );
        assert_eq!(
            codes("def f(  # noqa: E501\n    a=1,\n):\n    pass\n")?,
            ["NOD001"],
            "directives for other rules suppress nothing"
        );
        Ok(())
    }

    #[test]
    fn a_directive_on_the_def_line_stops_at_the_signature() -> Result<(), String> {
        assert_eq!(
            codes("def f(  # noqa: NOD001\n    a,\n):\n    def inner(b=1): pass\n")?,
            ["NOD002", "NOD001"],
            "a nested function keeps its own violations and leaves the directive unused"
        );
        assert_eq!(
            codes("def f(\n    a=1,\n):  # noqa: NOD001\n    pass\n")?,
            ["NOD001", "NOD002"],
            "only the `def` line carries a signature-wide directive"
        );
        Ok(())
    }

    #[test]
    fn a_signature_directive_leaves_defaults_in_place() -> Result<(), String> {
        let source = "def f(  # noqa: NOD001\n    a=1,\n):\n    pass\n";
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn a_directive_on_the_class_line_covers_every_field() -> Result<(), String> {
        assert!(
            codes(
                "@dataclass\nclass Job:  # noqa: NOD001\n    retries: int = 3\n    tags: list[str] = field(default_factory=list)\n"
            )?
            .is_empty(),
            "one directive covers every field of the dataclass"
        );
        assert_eq!(
            codes("@dataclass\nclass Job:  # noqa: NOD001\n    name: str\n")?,
            ["NOD002"],
            "a class directive that suppresses nothing is unused"
        );
        Ok(())
    }

    #[test]
    fn a_directive_on_the_class_line_stops_at_the_fields() -> Result<(), String> {
        assert_eq!(
            codes(
                "@dataclass\nclass Job:  # noqa: NOD001\n    retries: int = 3\n\n    def run(self, timeout=30): pass\n"
            )?,
            ["NOD001"],
            "a method keeps the violations of its own signature"
        );
        assert_eq!(
            codes(
                "@dataclass\nclass Outer:  # noqa: NOD001\n    @dataclass\n    class Inner:\n        retries: int = 3\n"
            )?,
            ["NOD002", "NOD001"],
            "a nested dataclass needs its own directive"
        );
        Ok(())
    }

    #[test]
    fn a_class_directive_leaves_defaults_in_place() -> Result<(), String> {
        let source = "@dataclass\nclass Job:  # noqa: NOD001\n    retries: int = 3\n";
        assert_eq!(fixed(source)?, source);
        Ok(())
    }

    #[test]
    fn unused_directives_are_reported() -> Result<(), String> {
        assert_eq!(
            codes("def f(x): pass  # noqa: NOD001\n")?,
            ["NOD002"],
            "an inline directive that suppresses nothing is unused"
        );
        assert!(
            codes("def f(x): pass  # noqa\n")?.is_empty(),
            "a blanket directive may serve another linter"
        );
        assert!(
            codes("def f(x): pass  # noqa: E501\n")?.is_empty(),
            "directives for other rules are not this linter's business"
        );
        assert_eq!(
            codes("# ruff: noqa: NOD001\ndef f(x): pass\n")?,
            ["NOD002"],
            "a file-level directive that suppresses nothing is unused"
        );
        assert!(
            codes("# ruff: noqa\ndef f(x): pass  # noqa: NOD001\n")?.is_empty(),
            "a blanket file-level directive silences every rule"
        );
        Ok(())
    }

    #[test]
    fn a_file_level_directive_claims_the_inline_directives_it_covers() -> Result<(), String> {
        assert!(
            codes("# ruff: noqa: NOD001\ndef f(x=1): pass  # noqa: NOD001\n")?.is_empty(),
            "both directives cover the same violation"
        );
        assert_eq!(
            codes("# ruff: noqa: NOD001\ndef f(x=1): pass\ndef g(x): pass  # noqa: NOD001\n")?,
            ["NOD002"],
            "only the inline directive covering nothing is unused"
        );
        Ok(())
    }

    #[test]
    fn private_only_makes_directives_for_public_symbols_unused() -> Result<(), String> {
        let found = check_source(
            Path::new("fixture.py"),
            "def public(x=1): pass  # noqa: NOD001\n",
            true,
            false,
        )?;
        assert_eq!(
            found
                .diagnostics
                .iter()
                .map(|item| item.code)
                .collect::<Vec<_>>()
                .as_slice(),
            ["NOD002"]
        );
        Ok(())
    }

    #[test]
    fn text_that_only_looks_like_a_directive_is_ignored() -> Result<(), String> {
        assert!(
            codes("example = \"# noqa: NOD001\"\n")?.is_empty(),
            "a `#` inside a string does not start a comment"
        );
        assert!(
            codes("def f(x): pass  # noqattention: NOD001\n")?.is_empty(),
            "`noqa` must be a word of its own"
        );
        Ok(())
    }

    #[test]
    fn directives_survive_carriage_returns() -> Result<(), String> {
        assert!(codes("def f(x=1): pass  # noqa: NOD001\r\n")?.is_empty());
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
        let settings = settings_for_files(&[root_file, nested_file], false)?;
        assert_eq!(settings[0].private_only, Some(true));
        assert_eq!(settings[1].private_only, Some(false));
        Ok(())
    }
}
