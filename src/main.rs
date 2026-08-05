use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::visitor::{walk_stmt, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_parser::parse_module;
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
    message: String,
    fix: TextRange,
}

fn main() -> ExitCode {
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
                if setting.private_only {
                    "private"
                } else {
                    "all"
                }
            );
        }
        return Ok(false);
    }
    let results: Vec<Result<Vec<Diagnostic>, String>> = files
        .par_iter()
        .zip(settings.par_iter())
        .map(|(path, setting)| check_file(path, setting.private_only))
        .collect();
    let mut diagnostics = Vec::new();
    for result in results {
        diagnostics.extend(result?);
    }
    diagnostics.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    if (cli.fix || cli.diff) && !diagnostics.is_empty() {
        let changes = fixed_sources(&diagnostics)?;
        if cli.diff {
            print_diffs(&changes);
            return Ok(true);
        }
        write_fixes_atomically(changes)?;
        println!(
            "Found {} error{} ({} fixed, 0 remaining).",
            diagnostics.len(),
            if diagnostics.len() == 1 { "" } else { "s" },
            diagnostics.len()
        );
        return Ok(false);
    }
    report_diagnostics(&diagnostics, cli.output_format)?;
    Ok(!diagnostics.is_empty())
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
                    "{}:{}:{}: NOD001 {}",
                    diagnostic.path.display(),
                    diagnostic.line,
                    diagnostic.column,
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
                    "{}:{}:{}: NOD001 {}",
                    diagnostic.path.display(),
                    diagnostic.line,
                    diagnostic.column,
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
                    code: "NOD001",
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
                    "::error file={},line={},col={},title=NOD001::{}",
                    github_escape_property(&diagnostic.path.display().to_string()),
                    diagnostic.line,
                    diagnostic.column,
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
    private_only: bool,
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
            private_only: cli_private_only || private_only_for(path, &loaded, &overrides),
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

fn fixed_sources(
    diagnostics: &[Diagnostic],
) -> Result<BTreeMap<PathBuf, (String, String)>, String> {
    let mut by_path: BTreeMap<&Path, Vec<TextRange>> = BTreeMap::new();
    for diagnostic in diagnostics {
        by_path
            .entry(&diagnostic.path)
            .or_default()
            .push(diagnostic.fix);
    }
    by_path
        .into_iter()
        .map(|(path, mut ranges)| {
            let source = std::fs::read_to_string(path).map_err(|error| {
                format!("could not read {} for fixing: {error}", path.display())
            })?;
            let mut fixed = source.clone();
            ranges.sort_by_key(|range| std::cmp::Reverse(range.start()));
            for range in ranges {
                fixed.replace_range(range.start().to_usize()..range.end().to_usize(), "");
            }
            parse_module(&fixed).map_err(|error| {
                format!(
                    "refusing to write invalid Python to {} after fixing: {error}",
                    path.display()
                )
            })?;
            Ok((path.to_path_buf(), (source, fixed)))
        })
        .collect()
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

fn private_only_for(path: &Path, loaded: &LoadedConfig, overrides: &[PerFileEnforcement]) -> bool {
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
        Some(Enforcement::All) => false,
        Some(Enforcement::Private) => true,
        None => loaded.config.private_only,
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

fn check_file(path: &Path, private_only: bool) -> Result<Vec<Diagnostic>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let parsed = parse_module(&source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    if has_file_noqa(&source) {
        return Ok(Vec::new());
    }
    let mut checker = Checker {
        path,
        source: &source,
        private_only,
        private_scope: is_private_module(path),
        dataclass_scope: false,
        diagnostics: Vec::new(),
    };
    for statement in parsed.suite() {
        checker.visit_stmt(statement);
    }
    Ok(checker.diagnostics)
}

fn has_file_noqa(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        let Some(directive) = line.strip_prefix('#').map(str::trim) else {
            return false;
        };
        let Some(rest) = directive.strip_prefix("ruff: noqa") else {
            return directive == "flake8: noqa";
        };
        let rest = rest.trim();
        rest.is_empty()
            || rest
                .strip_prefix(':')
                .is_some_and(|codes| codes.split(',').any(|code| code.trim() == "nod001"))
    })
}

struct Checker<'a> {
    path: &'a Path,
    source: &'a str,
    private_only: bool,
    private_scope: bool,
    dataclass_scope: bool,
    diagnostics: Vec<Diagnostic>,
}

impl Checker<'_> {
    fn enabled(&self, name: &str) -> bool {
        !self.private_only || self.private_scope || is_private(name)
    }

    fn report(&mut self, offset: TextSize, message: String, fix: TextRange) {
        if is_suppressed(self.source, offset) {
            return;
        }
        let (line, column) = line_column(self.source, offset);
        self.diagnostics.push(Diagnostic {
            path: self.path.to_path_buf(),
            line,
            column,
            message,
            fix,
        });
    }

    fn check_function(&mut self, function: &ast::StmtFunctionDef) {
        if !self.enabled(function.name.as_str()) {
            return;
        }
        for parameter in function
            .parameters
            .posonlyargs
            .iter()
            .chain(&function.parameters.args)
            .chain(&function.parameters.kwonlyargs)
        {
            if let Some(default) = &parameter.default {
                self.report(
                    default.start(),
                    format!(
                        "parameter `{}` of function `{}` has a default",
                        parameter.parameter.name, function.name
                    ),
                    TextRange::new(parameter.parameter.end(), default.end()),
                );
            }
        }
    }

    fn check_dataclass_field(&mut self, statement: &Stmt) {
        let Stmt::AnnAssign(assign) = statement else {
            return;
        };
        let (Expr::Name(name), Some(value)) = (&*assign.target, assign.value.as_deref()) else {
            return;
        };
        if !self.enabled(name.id.as_str()) || is_class_var(statement) {
            return;
        }
        let Some(default) = field_default(value, assign.annotation.end()) else {
            return;
        };
        self.report(
            value.start(),
            format!("dataclass field `{}` has a {}", name.id, default.kind),
            default.fix,
        );
    }
}

impl<'a> Visitor<'a> for Checker<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(function) => {
                self.check_function(function);
                let old_private = self.private_scope;
                self.private_scope = old_private || is_private(function.name.as_str());
                walk_stmt(self, statement);
                self.private_scope = old_private;
            }
            Stmt::ClassDef(class) => {
                let old_private = self.private_scope;
                let old_dataclass = self.dataclass_scope;
                self.private_scope = old_private || is_private(class.name.as_str());
                self.dataclass_scope = has_dataclass_decorator(class);
                walk_stmt(self, statement);
                self.dataclass_scope = old_dataclass;
                self.private_scope = old_private;
            }
            _ if self.dataclass_scope => {
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

fn is_class_var(statement: &Stmt) -> bool {
    let Stmt::AnnAssign(assign) = statement else {
        return false;
    };
    matches!(&*assign.annotation, Expr::Name(name) if name.id.as_str() == "ClassVar")
        || matches!(&*assign.annotation, Expr::Subscript(subscript)
            if matches!(&*subscript.value, Expr::Name(name) if name.id.as_str() == "ClassVar")
                || matches!(&*subscript.value, Expr::Attribute(attribute) if attribute.attr.as_str() == "ClassVar"))
}

struct FieldDefault {
    kind: &'static str,
    fix: TextRange,
}

fn field_default(value: &Expr, annotation_end: TextSize) -> Option<FieldDefault> {
    let Expr::Call(call) = value else {
        return Some(FieldDefault {
            kind: "default",
            fix: TextRange::new(annotation_end, value.end()),
        });
    };
    let is_field = matches!(&*call.func, Expr::Name(name) if name.id.as_str() == "field")
        || matches!(&*call.func, Expr::Attribute(attribute) if attribute.attr.as_str() == "field");
    if !is_field {
        return Some(FieldDefault {
            kind: "default",
            fix: TextRange::new(annotation_end, value.end()),
        });
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
    let kind = if keyword
        .arg
        .as_ref()
        .is_some_and(|name| name.as_str() == "default_factory")
    {
        "default factory"
    } else {
        "default"
    };
    Some(FieldDefault {
        kind,
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

fn line_column(source: &str, offset: TextSize) -> (usize, usize) {
    let offset = offset.to_usize();
    let before = &source[..offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |position| position + 1);
    (line, offset - line_start + 1)
}

fn is_suppressed(source: &str, offset: TextSize) -> bool {
    let offset = offset.to_usize();
    let line_start = source[..offset]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |position| offset + position);
    let line = &source[line_start..line_end];
    let Some(comment) = line.split_once('#').map(|(_, comment)| comment.trim()) else {
        return false;
    };
    let lower = comment.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("noqa") else {
        return false;
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return true;
    }
    let Some(codes) = rest.strip_prefix(':') else {
        return false;
    };
    codes
        .split(',')
        .any(|code| code.split_whitespace().next() == Some("nod001"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(source: &str, private_only: bool) -> Result<Vec<String>, String> {
        let parsed = parse_module(source).map_err(|error| error.to_string())?;
        let mut checker = Checker {
            path: Path::new("fixture.py"),
            source,
            private_only,
            private_scope: false,
            dataclass_scope: false,
            diagnostics: Vec::new(),
        };
        for statement in parsed.suite() {
            checker.visit_stmt(statement);
        }
        Ok(checker
            .diagnostics
            .into_iter()
            .map(|item| item.message)
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
        assert!(!private_only_for(
            Path::new("project/tests/test_api.py"),
            &loaded,
            &overrides
        ));
        assert!(private_only_for(
            Path::new("project/src/package/api.py"),
            &loaded,
            &overrides
        ));
        assert!(!private_only_for(
            Path::new("project/scripts/release.py"),
            &loaded,
            &overrides
        ));
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
        let diagnostics = check_file(&path, false)?;
        assert_eq!(diagnostics.len(), 5);
        write_fixes_atomically(fixed_sources(&diagnostics)?)?;
        let fixed = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        assert!(fixed.contains("def f(a: int, *, b): pass"));
        assert!(fixed.contains("a: int"));
        assert!(fixed.contains("b: int = field(repr=False)"));
        assert!(fixed.contains("c: int = field(kw_only=True)"));
        assert!(fixed.contains("not_a_field = 3"));
        assert!(check_file(&directory.path().join("example.py"), false)?.is_empty());
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
        let diagnostics = check_file(&path, false)?;
        write_fixes_atomically(fixed_sources(&diagnostics)?)?;
        let fixed = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        assert!(fixed.contains("value: int,  # useful comment"));
        assert!(fixed.contains("flag,"));
        assert!(check_file(&path, false)?.is_empty());
        Ok(())
    }

    #[test]
    fn file_level_noqa_suppresses_rule() {
        assert!(has_file_noqa("# ruff: noqa: NOD001\ndef f(x=1): pass\n"));
        assert!(!has_file_noqa("# ruff: noqa: E501\ndef f(x=1): pass\n"));
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
        assert!(settings[0].private_only);
        assert!(!settings[1].private_only);
        Ok(())
    }
}
