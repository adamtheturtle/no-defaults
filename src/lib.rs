use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::token::{TokenKind, Tokens};
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
    code: &'static str,
    message: String,
    fix: TextRange,
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

/// Lint Python source without filesystem or configuration-discovery overhead.
///
/// This is primarily exposed for performance benchmarks.
#[doc(hidden)]
pub fn lint_source(source: &str, private_only: bool) -> Result<usize, String> {
    Ok(check_source(Path::new("benchmark.py"), source, private_only)?.len())
}

fn check_file(path: &Path, private_only: bool) -> Result<Vec<Diagnostic>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    check_source(path, &source, private_only)
}

fn check_source(path: &Path, source: &str, private_only: bool) -> Result<Vec<Diagnostic>, String> {
    let parsed = parse_module(source)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let directives = collect_directives(source, parsed.tokens());
    // A blanket file-level directive silences every rule for the file,
    // including the unused-directive rule itself.
    if directives
        .iter()
        .any(|directive| directive.file_level && !directive.explicit)
    {
        return Ok(Vec::new());
    }
    let mut checker = Checker {
        path,
        source,
        private_only,
        private_scope: is_private_module(path),
        dataclass_scope: false,
        header: None,
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
    private_scope: bool,
    dataclass_scope: bool,
    /// Start of the `def` or `class` line that owns the violations being
    /// reported, so one directive there can cover every parameter of a
    /// signature or every field of a dataclass.
    header: Option<TextSize>,
    directives: Vec<Directive>,
    diagnostics: Vec<Diagnostic>,
}

impl Checker<'_> {
    fn enabled(&self, name: &str) -> bool {
        !self.private_only || self.private_scope || is_private(name)
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

    fn report(&mut self, offset: TextSize, message: String, fix: TextRange) {
        if self.suppress(offset) {
            return;
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
    }

    /// Add a diagnostic for every directive that named `NOD001` without
    /// suppressing anything, then return the file's diagnostics in order.
    fn finish(mut self) -> Vec<Diagnostic> {
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
        self.diagnostics
    }

    fn check_function(&mut self, function: &ast::StmtFunctionDef) {
        if !self.enabled(function.name.as_str()) {
            return;
        }
        // The function's own range starts at its first decorator, so the name
        // locates the `def` line that a signature-wide directive sits on.
        let enclosing = self.header;
        self.header = Some(line_start(self.source, function.name.start()));
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
        self.header = enclosing;
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
                let old_dataclass = self.dataclass_scope;
                self.private_scope = old_private || is_private(function.name.as_str());
                // Annotated assignments in a method body are locals, not
                // fields, so field detection stops at the function boundary.
                self.dataclass_scope = false;
                walk_stmt(self, statement);
                self.dataclass_scope = old_dataclass;
                self.private_scope = old_private;
            }
            Stmt::ClassDef(class) => {
                let old_private = self.private_scope;
                let old_dataclass = self.dataclass_scope;
                let old_header = self.header;
                self.private_scope = old_private || is_private(class.name.as_str());
                self.dataclass_scope = has_dataclass_decorator(class);
                // As with a `def` line, the name locates the `class` line a
                // directive covering every field sits on.
                self.header = Some(line_start(self.source, class.name.start()));
                walk_stmt(self, statement);
                self.header = old_header;
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
        Ok(check_source(Path::new("fixture.py"), source, private_only)?
            .into_iter()
            .map(|item| item.message)
            .collect())
    }

    fn codes(source: &str) -> Result<Vec<&'static str>, String> {
        Ok(check_source(Path::new("fixture.py"), source, false)?
            .into_iter()
            .map(|item| item.code)
            .collect())
    }

    fn fixed(source: &str) -> Result<String, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = directory.path().join("example.py");
        std::fs::write(&path, source).map_err(|error| error.to_string())?;
        let diagnostics = check_file(&path, false)?;
        write_fixes_atomically(fixed_sources(&diagnostics)?)?;
        let fixed = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        assert!(check_file(&path, false)?.is_empty(), "{fixed:?}");
        Ok(fixed)
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
        )?;
        assert_eq!(
            found
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
