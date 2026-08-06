[![CI](https://github.com/adamtheturtle/no-defaults/actions/workflows/ci.yml/badge.svg)](https://github.com/adamtheturtle/no-defaults/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/no-defaults.svg)](https://pypi.org/project/no-defaults/)

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
uv tool install no-defaults
```

## Usage

```console
no-defaults .
no-defaults --fix .
no-defaults --diff .
no-defaults --private-only src tests
no-defaults --output-format json .
no-defaults --output-format github .
no-defaults --show-settings src/package/api.py
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

After a successful fix, the command exits with status `0` and prints a Ruff-style summary such as `Found 2 errors (2 fixed, 0 remaining).` Writes use an atomic same-directory replacement. `--diff` prints a unified diff, writes nothing, and exits with status `1` when changes are available.

### `--fix` does not update call sites

`--fix` rewrites signatures and nothing else. Removing a default makes that argument required, so every caller that omitted it now raises `TypeError` at runtime:

```python
def connect(timeout=30):     # becomes  def connect(timeout):
    ...

connect()                    # TypeError: connect() missing 1 required positional argument
```

The same applies to dataclass fields, which become required at construction. The fixed code still imports and still lints clean, so a warning is printed after fixing and **your test suite is what confirms the result**.

Rewriting call sites would mean resolving every call to its definition across the whole project, which this per-file design deliberately avoids. It would not be sufficient either: for a function that is part of your public API, the callers that break are in other people's code.

`--fix` is therefore safest under `private_only = true`, where the symbols it touches have no callers outside the project.

The default `full` output includes source excerpts and carets. `concise` emits one diagnostic per line, `json` emits a machine-readable array, and `github` emits workflow commands for GitHub Actions annotations.

The linter detects defaults on positional-only, positional-or-keyword, and keyword-only parameters.
For classes decorated with `@dataclass` or `@dataclasses.dataclass`, it detects assigned defaults plus `field(default=...)` and `field(default_factory=...)` in the class body.
`ClassVar` assignments are ignored because they are not dataclass fields, and annotated assignments inside method bodies are ignored because they are locals.

Suppress an individual violation with either a blanket `# noqa` or the rule-specific `# noqa: NOD001` on the line containing the default:

```python
def compatible(timeout=30):  # noqa: NOD001
    pass
```

A directive on the line holding `def` covers every parameter of that signature, so a multi-line signature needs one directive rather than one per parameter:

```python
def compatible(  # noqa: NOD001
    timeout=30,
    retries=3,
):
    pass
```

A directive on the `class` line does the same for every field of a dataclass:

```python
@dataclass
class Job:  # noqa: NOD001
    retries: int = 3
    tags: list[str] = field(default_factory=list)
```

Decorators do not move either line, and the scope stops at the signature or the class body: methods, nested functions, and nested dataclasses keep their own violations and need their own directives. A directive placed elsewhere in the signature, such as on the closing parenthesis, still applies only to its own line.

Suppress the rule for an entire file with `# ruff: noqa` or `# ruff: noqa: NOD001`.

A directive that names `NOD001` without suppressing anything is reported as `NOD002` and removed by `--fix`:

```text
src/example.py:1:21: NOD002 unused `noqa` directive for `NOD001`
```

Only directives that name the code are checked. A blanket `# noqa` may exist for another linter, so it is never reported, and a blanket `# ruff: noqa` or `# flake8: noqa` silences every rule in the file, including this one. When `--fix` removes the last code from a directive, it removes the whole comment; otherwise it removes just `NOD001` from the list.

### Using suppressions alongside Ruff

`NOD001` is not a Ruff rule, so Ruff reports every `# noqa: NOD001` as `RUF102 Invalid rule code`.
That diagnostic is fixable, which means `ruff check --fix` deletes the suppression comment and leaves the violation behind for `no-defaults` to report.

Register the prefix as an external code so Ruff leaves the suppressions alone:

```toml
[tool.ruff]
lint.external = [ "NOD" ]
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

### Private modules that are re-exported publicly

Privacy is decided from module and symbol names alone. A function defined in `_upload.py` counts as private even when the package's `__init__.py` re-exports it, whether through `__all__` or a plain import. Under `private_only = true` it is still checked, although its defaults are part of the public API, where removing one is a breaking change for callers.

This is deliberate. `no-defaults` checks each file on its own, which is what lets it resolve configuration per file and stay fast when pre-commit passes only the changed files. It never reads `__init__.py` to work out which names a private module re-exports, so it cannot tell an internal helper from a re-exported one.

Suppress the rule on the signatures that are public in practice:

```python
def upload(
    *,
    strategy: Strategy = Strategy.DIFF,  # noqa: NOD001
) -> None:
    """Re-exported from the package root, so the default is public API."""
```

`per_file_enforcement` accepts Ruff-style glob patterns relative to the directory containing `pyproject.toml`. Use `"all"` to reject every default in matching files or `"private"` to reject defaults only in private scopes. Patterns without a slash match file names at any depth. An initial `!` negates a pattern. If multiple patterns match, the most specific pattern wins; equally specific patterns are resolved lexicographically so results never depend on TOML table order.

The `--private-only` CLI flag overrides the configuration for every checked file.

Like Ruff, `no-defaults` discovers the closest `pyproject.toml` containing `[tool.no_defaults]` separately for each file. This supports monorepos with nested configuration; files without a local table continue searching parent directories.

## Performance

An optimized `1.0.0` development build checked a pinned Typeshed checkout containing 5,368 Python and stub files (12.5 MiB) in a median 0.29 seconds across five warm runs on an Apple Silicon Mac, or roughly 18,000 files per second. It produced 50,974 diagnostics; an earlier full-output measurement used approximately 41 MiB maximum RSS.

CodSpeed runs parser-and-rule benchmarks for representative modules on every pull request and every push to `main`, providing stable comparisons against the default-branch baseline. The scheduled `Typeshed benchmark` remains as a real-project correctness and gross-regression check.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/no-defaults
    rev: v1.0.0
    hooks:
      - id: no-defaults
```

## License

MIT

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and [CHANGELOG.md](CHANGELOG.md).
