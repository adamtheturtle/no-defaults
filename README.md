[![CI](https://github.com/adamtheturtle/no-defaults/actions/workflows/ci.yml/badge.svg)](https://github.com/adamtheturtle/no-defaults/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/no-defaults.svg)](https://pypi.org/project/no-defaults/)

# no-defaults

A fast, standalone Python linter that forbids defaults in function signatures, dataclasses, and pydantic models — and removes them for you, updating the call sites in the files you checked so the code still runs. It is implemented in Rust and parses Python with Ruff's parser.

```python
from dataclasses import dataclass, field
from pydantic import BaseModel, Field

def connect(timeout=30):  # NOD001
    pass

@dataclass
class Job:
    retries: int = 3  # NOD001
    tags: list[str] = field(default_factory=list)  # NOD001

class Request(BaseModel):
    method: str = "GET"  # NOD001
    headers: dict[str, str] = Field(default_factory=dict)  # NOD001
    url: str = Field(..., description="required, so no default to remove")
```

## Installation

```console
uv tool install no-defaults
```

The Python package installs [ty](https://docs.astral.sh/ty/) alongside
`no-defaults`. If you install the Rust binary with Cargo instead, install `ty`
separately and make it available on `PATH`; cross-package callback resolution
uses the supported `ty server` interface and fails explicitly when it is absent.

## Usage

```console
no-defaults .                                       # check
no-defaults --fix .                                 # remove defaults, update call sites
no-defaults --diff .                                # preview the same edits, write nothing
no-defaults --private-only src tests                # only underscore-prefixed names
no-defaults --private-only --respect-reexports src  # ... minus ones re-exported publicly
no-defaults --output-format json .                  # also: full, concise, github
no-defaults --show-settings src/package/api.py      # the settings that apply to a file
```

Exit status is `0` when clean, `1` when violations are found, and `2` for an operational error — including a path named on the command line that is not a `.py` or `.pyi` file, so a mistyped path fails rather than reporting a clean run over nothing. Directories are walked in parallel for Python files, respecting `.gitignore` and hidden-file filters.

```text
src/example.py:4:21: NOD001 parameter `timeout` of function `connect` has a default
Found 1 error.
```

| code | meaning |
| --- | --- |
| `NOD000` | a file the parser rejected. The run carries on so one bad file does not hide the rest, and `--fix` leaves that file alone and exits `1` |
| `NOD001` | a default |
| `NOD002` | a `# noqa` naming `NOD001` that suppresses nothing. `--fix` removes it |

## `--fix`

`--fix` removes the default and then passes it explicitly at every call it can resolve in the files you asked it to check, so the argument that just became required is still supplied:

```python
def connect(host, timeout=30):  # becomes  def connect(host, timeout):
    ...

connect("example.com")          # becomes  connect("example.com", timeout=30)
connect("example.com", 5)       # already supplies it, so it is left alone
```

Dataclass and model fields become required at construction the same way, and `field(...)` keeps its other metadata: `field(default=3, kw_only=True)` becomes `field(kw_only=True)`.

Nothing is guessed. A call is left alone, and named in a warning, when it cannot be tied to the definition that changed, when the removed default is not a literal, or in any of the other cases listed in the [reference](docs/reference.md#--fix). Defaults in `.pyi` stubs are reported but never removed, since a stub describes a signature rather than supplying one.

Two things `--fix` cannot reach in general: **callers outside the files you checked**, and **calls made dynamically**. It reads imported Python packages from `PYTHONPATH` and the active virtual environment and asks `ty` to resolve statically visible framework callbacks: a default omitted by one of those dependency calls is retained, while the dependency is never imported, diagnosed, or edited. A warning after fixing covers callers it still cannot see, and **your test suite is what confirms the result**. Run it over the whole project at once, and prefer `private_only` with `respect_reexports`, where the symbols it touches have no callers outside the project.

## Suppressing

`# noqa: NOD001` — or a blanket `# noqa` — on the line holding the default suppresses it. On a `def` or `class` line it covers that whole signature or class body, so a multi-line signature needs one directive rather than one per parameter. `# ruff: noqa: NOD001` on its own line covers a whole file. See the [reference](docs/reference.md#suppressing) for the exact scoping rules.

`NOD001` is not a Ruff rule, so Ruff reports every such comment as `RUF102 Invalid rule code` and `ruff check --fix` deletes it, leaving the violation behind. Register the prefix so Ruff leaves the suppressions alone:

```toml
[tool.ruff]
lint.external = [ "NOD" ]
```

## Configuration

Configuration lives in `pyproject.toml`. Like Ruff, `no-defaults` finds the closest one containing `[tool.no_defaults]` separately for each file, so nested configuration in a monorepo works. An unrecognised key is an error, so a misspelled option fails the run rather than silently leaving the defaults in place.

```toml
[tool.no_defaults]
# Check only private names: private modules and packages, private functions
# and methods, all members of private classes, and private fields.
private_only = true
# ... but treat what a package's `__init__.py` re-exports as public API.
respect_reexports = true
# Bases whose annotated assignments are fields, as pydantic's are. Setting this
# replaces the default rather than adding to it.
field_base_classes = [ "pydantic.BaseModel" ]

# Ruff-style globs, relative to this file. "all", "private", or "none".
[tool.no_defaults.per_file_enforcement]
"tests/**" = "all"
"src/**" = "private"
```

The [reference](docs/reference.md#configuration) covers what counts as private, what counts as a re-export and the limits of detecting one, and how overlapping `per_file_enforcement` patterns are resolved.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/no-defaults
    rev: v2.3.0  # pin to a release tag
    hooks:
      - id: no-defaults
```

pre-commit passes only the changed files, so a call in a file that did not change is not updated. Run `no-defaults --fix .` by hand when you are removing a default that is called from elsewhere.

## License

MIT

See [docs/reference.md](docs/reference.md) for the full behaviour, and [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and [CHANGELOG.rst](CHANGELOG.rst). Semver releases through `2.3.0` are recorded in [CHANGELOG.md](CHANGELOG.md).
