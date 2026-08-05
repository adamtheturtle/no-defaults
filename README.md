# no-defaults

A fast, standalone Python linter that forbids defaults in function signatures and dataclasses.
It is implemented in Rust and parses Python with Ruff's parser.

```python
from dataclasses import dataclass, field

def connect(timeout=30):  # NOD001
    pass

@dataclass
class Job:
    retries: int = 3  # NOD001
    tags: list[str] = field(default_factory=list)  # NOD001
```

## Installation

```console
uv tool install git+https://github.com/adamtheturtle/no-defaults
```

This installs from GitHub; the initial release has not yet been published to PyPI.

## Usage

```console
no-defaults .
no-defaults --fix .
no-defaults --private-only src tests
```

Exit status is `0` when clean, `1` when violations are found, and `2` for an operational error.
Directories are walked in parallel, respecting `.gitignore` and standard hidden-file filters.
Diagnostics use Ruff's concise format and include a summary:

```text
src/example.py:4:17: NOD001 parameter `timeout` of function `connect` has a default
Found 1 error.
```

Pass `--fix` to remove defaults automatically. Function parameters and ordinary dataclass assignments become required. For `field(...)`, only the positional default, `default=`, or `default_factory=` argument is removed; other metadata is preserved:

```python
retries: int = field(default=3, kw_only=True)
# becomes
retries: int = field(kw_only=True)
```

After a successful fix, the command exits with status `0` and prints a Ruff-style summary such as `Found 2 errors (2 fixed, 0 remaining).`

The linter detects defaults on positional-only, positional-or-keyword, and keyword-only parameters.
For classes decorated with `@dataclass` or `@dataclasses.dataclass`, it detects assigned defaults plus `field(default=...)` and `field(default_factory=...)`.
`ClassVar` assignments are ignored because they are not dataclass fields.

Suppress an individual violation with either a blanket `# noqa` or the rule-specific `# noqa: NOD001` on the line containing the default:

```python
def compatible(timeout=30):  # noqa: NOD001
    pass
```

## Configuration

Configuration lives in `pyproject.toml`:

```toml
[tool.no_defaults]
private_only = true

[tool.no_defaults.per_file_enforcement]
"tests/**" = "all"
"src/**" = "private"
```

Private means a name that starts with one underscore. In private-only mode, the rule applies to private modules and packages, private functions and methods, all members of private classes, and private dataclass fields. For example, all defaults in `_module.py` and `_package/module.py` are checked. Dunder names such as `__init__.py` are not considered private by themselves.

`per_file_enforcement` accepts Ruff-style glob patterns relative to the directory containing `pyproject.toml`. Use `"all"` to reject every default in matching files or `"private"` to reject defaults only in private scopes. Patterns without a slash match file names at any depth. An initial `!` negates a pattern. If multiple patterns match, the most specific pattern wins; equally specific patterns are resolved lexicographically so results never depend on TOML table order.

The `--private-only` CLI flag overrides the configuration for every checked file.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/no-defaults
    rev: v0.2.0
    hooks:
      - id: no-defaults
```

## License

MIT
