use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::visitor::{walk_stmt, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextSize};
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(version, about = "Forbid defaults in Python functions and dataclasses")]
struct Cli {
    /// Python files or directories to check.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Only enforce private (underscore-prefixed) functions, classes, and fields.
    #[arg(long)]
    private_only: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    private_only: bool,
}

#[derive(Debug)]
struct Diagnostic {
    path: PathBuf,
    line: usize,
    column: usize,
    message: String,
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
    let config = load_config()?;
    let private_only = cli.private_only || config.private_only;
    let files = collect_files(&cli.paths)?;
    let results: Vec<Result<Vec<Diagnostic>, String>> = files
        .par_iter()
        .map(|path| check_file(path, private_only))
        .collect();
    let mut diagnostics = Vec::new();
    for result in results {
        diagnostics.extend(result?);
    }
    diagnostics.sort_by(|left, right| {
        (&left.path, left.line, left.column).cmp(&(&right.path, right.line, right.column))
    });
    for diagnostic in &diagnostics {
        eprintln!(
            "{}:{}:{}: NOD001 {}",
            diagnostic.path.display(),
            diagnostic.line,
            diagnostic.column,
            diagnostic.message
        );
    }
    Ok(!diagnostics.is_empty())
}

fn load_config() -> Result<Config, String> {
    let mut directory = std::env::current_dir().map_err(|error| error.to_string())?;
    loop {
        let path = directory.join("pyproject.toml");
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            let value: toml::Value = toml::from_str(&text)
                .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
            return value
                .get("tool")
                .and_then(|tool| tool.get("no_defaults"))
                .cloned()
                .map_or_else(
                    || Ok(Config::default()),
                    |table| {
                        table.try_into().map_err(|error| {
                            format!("invalid [tool.no_defaults] in {}: {error}", path.display())
                        })
                    },
                );
        }
        if !directory.pop() {
            return Ok(Config::default());
        }
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

    fn report(&mut self, offset: TextSize, message: String) {
        if is_suppressed(self.source, offset) {
            return;
        }
        let (line, column) = line_column(self.source, offset);
        self.diagnostics.push(Diagnostic {
            path: self.path.to_path_buf(),
            line,
            column,
            message,
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
                );
            }
        }
    }

    fn check_dataclass_field(&mut self, statement: &Stmt) {
        let (name, value) = match statement {
            Stmt::AnnAssign(assign) => match (&*assign.target, assign.value.as_deref()) {
                (Expr::Name(name), Some(value)) => (name.id.as_str(), value),
                _ => return,
            },
            Stmt::Assign(assign) if assign.targets.len() == 1 => match &assign.targets[0] {
                Expr::Name(name) => (name.id.as_str(), &*assign.value),
                _ => return,
            },
            _ => return,
        };
        if !self.enabled(name) || is_class_var(statement) {
            return;
        }
        let Some(kind) = field_default_kind(value) else {
            return;
        };
        self.report(
            value.start(),
            format!("dataclass field `{name}` has a {kind}"),
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

fn field_default_kind(value: &Expr) -> Option<&'static str> {
    let Expr::Call(call) = value else {
        return Some("default");
    };
    let is_field = matches!(&*call.func, Expr::Name(name) if name.id.as_str() == "field")
        || matches!(&*call.func, Expr::Attribute(attribute) if attribute.attr.as_str() == "field");
    if !is_field {
        return Some("default");
    }
    if call.arguments.keywords.iter().any(|keyword| {
        keyword
            .arg
            .as_ref()
            .is_some_and(|name| name.as_str() == "default_factory")
    }) {
        Some("default factory")
    } else if call.arguments.keywords.iter().any(|keyword| {
        keyword
            .arg
            .as_ref()
            .is_some_and(|name| name.as_str() == "default")
    }) || !call.arguments.args.is_empty()
    {
        Some("default")
    } else {
        None
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
}
