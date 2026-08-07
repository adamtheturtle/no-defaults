[![CI](https://github.com/adamtheturtle/no-defaults/actions/workflows/ci.yml/badge.svg)](https://github.com/adamtheturtle/no-defaults/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/no-defaults.svg)](https://pypi.org/project/no-defaults/)

# no-defaults

A fast, standalone Python linter that forbids defaults in function signatures, dataclasses, and pydantic models.
It is implemented in Rust and parses Python with Ruff's parser.

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

## Usage

```console
no-defaults .
no-defaults --fix .
no-defaults --diff .
no-defaults --private-only src tests
no-defaults --private-only --respect-reexports src
no-defaults --output-format json .
no-defaults --output-format github .
no-defaults --show-settings src/package/api.py
```

Exit status is `0` when clean, `1` when violations are found, and `2` for an operational error.
A path named on the command line that is not a `.py` or `.pyi` file is an operational error, so a mistyped path or a misconfigured `.pre-commit-config.yaml` fails rather than reporting a clean run over nothing. Directories are still walked for Python files only.
Directories are walked in parallel, respecting `.gitignore` and standard hidden-file filters.
Diagnostics use Ruff's concise format and include a summary:

```text
src/example.py:4:17: NOD001 parameter `timeout` of function `connect` has a default
Found 1 error.
```

A file the parser rejects is reported as `NOD000` and the run carries on, so one unparseable file — a Python 2 module kept for reference, a template saved as `.py`, a file caught mid-edit — does not hide every other file's diagnostics:

```text
legacy/print.py:1:7: NOD000 syntax error: Simple statements must be separated by newlines or semicolons
src/example.py:4:17: NOD001 parameter `timeout` of function `connect` has a default
Found 2 errors.
```

In a `.pyi` stub, `x: int = ...` does not declare a default *value* — it is the convention for "this parameter has a default, unspecified here". It is reported, because the stub still describes an optional parameter, but it is not fixed: removing it would make the parameter required, so the stub would stop matching the implementation it describes and type checkers would reject callers that legitimately omit the argument. Outside a stub, `= ...` is an ordinary default and is removed like any other.

`NOD000` has no fix, so `--fix` counts it as remaining and exits `1` rather than claiming a clean result. The file is left out of the fixing pass entirely; the rest of the project is still fixed.

Pass `--fix` to remove defaults automatically. Function parameters and ordinary dataclass assignments become required. For `field(...)`, only the positional default, `default=`, or `default_factory=` argument is removed; other metadata is preserved:

```python
retries: int = field(default=3, kw_only=True)
# becomes
retries: int = field(kw_only=True)
```

After a successful fix, the command exits with status `0` and prints a Ruff-style summary such as `Found 2 errors (2 fixed, 0 remaining).` followed by `Updated 3 call sites.` Writes use an atomic same-directory replacement. `--diff` prints a unified diff, writes nothing, and exits with status `1` when changes are available.

### `--fix` updates call sites

Removing a default makes that argument required, so `--fix` also passes the removed default explicitly at every call it can see in the files you asked it to check:

```python
def connect(host, timeout=30):     # becomes  def connect(host, timeout):
    ...

connect("example.com")             # becomes  connect("example.com", timeout=30)
connect("example.com", 5)          # already supplies it, so it is left alone
```

The same applies to dataclass fields, which become required at construction: `Job("j")` becomes `Job("j", retries=3)`. A `default_factory` becomes the value it produces, so `field(default_factory=list)` adds `tags=[]` — a fresh list per call, which is what the factory gave you.

Arguments are appended as keywords wherever Python allows it, so the change is stable under later edits to the signature. Positional-only parameters are appended positionally instead.

Calls are resolved through the calling file's own imports, not by matching the bare name. An absolute import is looked up first at the importing file's own import root — the first directory above it that is not a package — and otherwise by matching the dotted name against the checked files' paths, which is what lets a test under `tests/` reach a package under `src/`. A one-component import such as `import utils` is only ever looked up at the importer's root, because one filename matches at any depth and `deep/nested/utils.py` is not what it resolves to. A project with its own `connect` does not get `socket.connect` rewritten, and two modules that each define `helper` are told apart. A method is rewritten when reached through `self`, `cls`, or a class the file can name — `Client`, `api.Client`, or one imported by name — which is as far as the receiver's type can be known without inference. What each method already receives is accounted for, so `instance.fetch(url)` and `Client.fetch(instance, url)` are both filled in correctly, and a `staticmethod` is given nothing extra.

Nothing is guessed. A call is left alone, with a warning naming the file and line, when

- it cannot be tied to the definition that was fixed: an unrelated callable of the same name, a name an enclosing function or class binds and so shadows the definition with, a method on a receiver whose type is unknown such as `client.fetch(...)`, or a call through an import this run could not resolve;
- the call unpacks `*args` or `**kwargs`, so what it already supplies is unknown;
- the removed default is not a literal (`value=SENTINEL`, `path=Path.cwd()`), because repeating that text at the call site would depend on names the caller may not have imported, or would re-evaluate the expression;
- a positional-only argument cannot be appended without reordering the call;
- the call's argument is a bare generator expression, which Python allows only when it is the only one, so nothing can follow it;
- the dataclass inherits fields, whose order in the constructor the defining file cannot see. Bases that declare no fields do not count, so a generic dataclass built on `Generic[T]`, `Protocol`, `ABC`, or `object` is still updated;
- the function is named without being called — a bare `@decorator`, or a callback passed as `run(cb)` — because Python calls it somewhere with no argument list to add to.

Fields that `__init__` never accepts are never added to a call: `field(..., init=False)`, a `_: KW_ONLY` marker, and a class whose decorator says `init=False`.

Class names are exempt from the named-without-being-called check, because they appear in annotations and `isinstance` checks constantly and none of those are calls.

Two things `--fix` still cannot reach: **callers outside the files you checked** — for a function that is part of your public API, they are in other people's code — and **calls made dynamically**, through `getattr` or a variable holding the function. A warning after fixing says so, and **your test suite is what confirms the result**.

`--fix` is therefore safest under `private_only = true` with `respect_reexports = true`, where the symbols it touches have no callers outside the project, and it sees the most when you run it over the whole project at once. Under pre-commit, which passes only the changed files, a call in a file that did not change is not in the run and is not updated — run `no-defaults --fix .` by hand when you are removing a default that is called from elsewhere.

`--diff` shows the call-site edits alongside the signature edits, and reports the same warnings, so you can preview the whole change before writing anything.

The default `full` output includes source excerpts and carets. `concise` emits one diagnostic per line, `json` emits a machine-readable array, and `github` emits workflow commands for GitHub Actions annotations.

`--output-format` applies when fixing too. Under `json` and `github` the diagnostics are all that reaches standard output, so the result stays parseable and CI annotations still appear; the `N fixed` summary and the call-site count go to standard error with the other warnings. Under `full` and `concise` both stay on standard output as before.

The linter detects defaults on positional-only, positional-or-keyword, and keyword-only parameters, in `def` signatures and in `lambda` ones alike.

A lambda is anonymous, so `--fix` cannot resolve its call sites the way it resolves a function's; removing a lambda default is reported as a call it left alone. The loop-capture idiom `lambda x=x: ...` relies on its default, so suppress it where you mean it:

```python
handlers = [lambda event, index=index: use(event, index) for index in range(3)]  # noqa: NOD001
```
For classes decorated with `@dataclass` or `@dataclasses.dataclass`, it detects assigned defaults plus `field(default=...)` and `field(default_factory=...)` in the class body. A renaming import is followed, so `from dataclasses import dataclass as dc` makes `@dc` count too; the same holds for `field`, pydantic's `Field`, and the `KW_ONLY` marker.
A class that carries fields through a base class instead, as a pydantic model does, is detected the same way once that base is in [`field_base_classes`](#classes-that-carry-fields), which lists `pydantic.BaseModel` by default.
`ClassVar` assignments are ignored because they are not fields, whether the annotation is bare, qualified, or quoted as in `x: "ClassVar[int]" = 1`. Annotated assignments inside method bodies are ignored because they are locals.

Suppress an individual violation with either a blanket `# noqa` or the rule-specific `# noqa: NOD001` on the line containing the default:

```python
def compatible(timeout=30):  # noqa: NOD001
    pass
```

As in Ruff and flake8, the marker is found anywhere on the line, so it can follow another tool's pragma — which matters because `# type: ignore` has to come first for some mypy versions:

```python
def compatible(timeout=30):  # type: ignore[misc]  # noqa: NOD001
    pass
```

Removing such a directive removes only its own `#` segment, leaving the other pragma in place.

A directive on the line holding `def` covers every parameter of that signature, so a multi-line signature needs one directive rather than one per parameter:

```python
def compatible(  # noqa: NOD001
    timeout=30,
    retries=3,
):
    pass
```

A directive on the `class` line does the same for every field of a dataclass or model:

```python
@dataclass
class Job:  # noqa: NOD001
    retries: int = 3
    tags: list[str] = field(default_factory=list)
```

Decorators do not move either line, and the scope stops at the signature or the class body: methods, nested functions, and nested dataclasses keep their own violations and need their own directives. A directive placed elsewhere in the signature, such as on the closing parenthesis, still applies only to its own line.

Suppress the rule for an entire file with `# ruff: noqa` or `# ruff: noqa: NOD001`. As in Ruff and flake8, the space after the colon is optional, so `# ruff:noqa` and `# flake8:noqa` work too. A file-level directive must be the only thing on its line.

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
respect_reexports = true
field_base_classes = [ "pydantic.BaseModel" ]

[tool.no_defaults.per_file_enforcement]
"tests/**" = "all"
"src/**" = "private"
```

Privacy is judged from the path below the project root, so a checkout living under a directory such as `_work/` is not treated as a private package and the answer does not change with whether you pass a relative or an absolute path.

Private means a name that starts with one underscore. In private-only mode, the rule applies to private modules and packages, private functions and methods, all members of private classes, and private fields of a dataclass or model. For example, all defaults in `_module.py` and `_package/module.py` are checked. Dunder names such as `__init__.py` are not considered private by themselves.

### Classes that carry fields

A `@dataclass` decorator marks a class whose annotated assignments are fields. So does a base class, which is how pydantic works, and `field_base_classes` lists the ones to recognise. It defaults to `[ "pydantic.BaseModel" ]`, and setting it replaces that list rather than adding to it, so `field_base_classes = []` checks decorated classes only.

A base is matched by the last segment of its name, as a decorator is, so `pydantic.BaseModel` recognises `class Job(BaseModel)` and `class Job(pydantic.BaseModel)` alike. Anything else that carries fields this way — `msgspec.Struct`, `sqlmodel.SQLModel`, `typing.NamedTuple` — works once it is listed:

```toml
[tool.no_defaults]
field_base_classes = [ "pydantic.BaseModel", "msgspec.Struct" ]
```

Within such a class, `Field(default=…)` and `Field(default_factory=…)` are reported and `--fix` removes only those arguments, keeping the rest of the metadata, exactly as it does for `field(...)`. Pydantic's `Field(...)` and `Field(default=...)` declare a field with no default, so neither is reported.

A class is recognised only where it names a listed base itself. `class Job(BaseModel)` is checked; `class SubJob(Job)` is not, because knowing that `Job` is a model means resolving imports across files. Where a model has a base beyond the listed one, its fields are still reported, but `--fix` leaves its call sites alone and says so, because that base may declare fields of its own.

### Private modules that are re-exported publicly

By default, privacy is decided from module and symbol names alone. A function defined in `_upload.py` counts as private even when the package's `__init__.py` re-exports it, whether through `__all__` or a plain import. Under `private_only = true` it is still checked, although its defaults are part of the public API, where removing one is a breaking change for callers.

`respect_reexports = true` reads those `__init__.py` files and treats what they export as public:

```python
# src/package/__init__.py
from ._upload import upload

__all__ = ["upload"]
```

```python
# src/package/_upload.py
def upload(source, timeout=30):  # not reported: `upload` is public API
    ...

def _chunk(data, size=8192):     # reported as before
    ...
```

The `--respect-reexports` flag turns this on for every checked file, whatever the configuration says. It only has an effect in private-only mode, since every default is checked otherwise.

#### What counts as a re-export

For each checked file, `no-defaults` reads the `__init__.py` of its directory and of each directory above it, up to the directory holding the `pyproject.toml` that configured the run. A directory without an `__init__.py` contributes nothing but does not end the walk, because a namespace package under a regular one is imported through it: a module in `package/data/` still sees what `package/__init__.py` exports. A file checked from outside any configured project follows its package chain as far as that reaches instead. A private package seals what is inside it: a name re-exported by `_internal/__init__.py` is no more reachable from outside than the module it came from, and that holds for a public sub-package inside `_internal` too.

In each of the files it reads, these make a name public:

- an import that binds it, including `from . import x`, `from ._mod import x`, `import x.y as z`, and imports inside `try` or `if TYPE_CHECKING` blocks;
- an entry in `__all__`, when `__all__` is written out as a list or tuple of string literals.

A name treated as public is public wherever it is defined in that package. A public class carries its members with it — `class Client` re-exported from `_upload.py` keeps the defaults of `fetch`, but `_retry` is private by its own name and is still checked. A re-exported dataclass keeps its field defaults.

A module or package re-exported under its own name counts too. `from . import _upload` makes `package._upload` reachable, so everything in `_upload.py` is treated as being in a public module and only names private in their own right are checked. The same applies to `from . import _internal`, which unseals the private package: its `__init__.py` is read after all.

#### Limitations

The check is by name. It never resolves an import back to the module it came from, so it is deliberately loose in the direction of leaving defaults alone, and it does not see re-exports that leave the package:

- **Names only.** If `__init__.py` exports an unrelated `upload` from another module, a private `upload` elsewhere in the package is treated as public too, and stops being checked. This is what makes chains work — `package/__init__.py` re-exporting from `_internal`, which re-exports from `_upload.py` — without reading every module in the package.
- **`from ... import *` makes everything public.** The names behind a star import cannot be listed without resolving the module, so every name in that package is treated as re-exported and `private_only` effectively stops applying there. Prefer explicit imports in `__init__.py` if you want the rule back.
- **Only a literal `__all__` is read.** One built by a call, a loop, or `__all__ += other.__all__` says nothing this run can use. The imports in the file are still read, and usually cover the same names.
- **Only the file's own package chain is read.** Another package that does `from package._upload import upload` and exports it from its own root is not consulted; nor is a `setup.py`-style re-export outside the package.
- **Deletions and rebinding are not tracked.** A name imported and then `del`-ed in `__init__.py` still counts as re-exported.
- **Every `__init__.py` in the chain must parse.** One that does not is an error, the same as a checked file that does not parse.

Reading these files costs one parse per directory on the way to the project root, shared by every checked file below it, and only in private-only mode with the option on. On the pinned Typeshed checkout described under [Performance](#performance) — 5,368 files across several thousand stub packages — turning the option on adds roughly 0.07 seconds to a run, around a tenth of its time, and drops 4,579 diagnostics to 3,209. A package's `__init__.pyi` counts where there is no `__init__.py`, so a stub-only distribution is read the same way.

#### Turning it on

A signature that is now recognised as public no longer needs the `# noqa: NOD001` that was keeping it quiet, so those directives are reported as `NOD002` unused directives. That is the migration path from the workaround below: turn the option on, then run `--fix`, which removes them.

Where a re-export is not detected, or where you want a public default checked anyway, the per-file levers still apply. Exempt the module:

```toml
[tool.no_defaults.per_file_enforcement]
"src/package/_upload.py" = "none"
```

or suppress the rule on the signatures that are public in practice:

```python
def upload(
    *,
    strategy: Strategy = Strategy.DIFF,  # noqa: NOD001
) -> None:
    """Re-exported from the package root, so the default is public API."""
```

`per_file_enforcement` accepts Ruff-style glob patterns relative to the directory containing `pyproject.toml`. Use `"all"` to reject every default in matching files, `"private"` to reject defaults only in private scopes, or `"none"` to exempt matching files from the rule. `"none"` also wins over `--private-only`, so an exempt file stays exempt. An exempt file keeps its own defaults, but its call sites are still updated when a callable it uses is fixed elsewhere: exemption decides which definitions are checked, not whether the file keeps working. Patterns without a slash match file names at any depth. An initial `!` negates a pattern. If multiple patterns match, the most specific pattern wins; equally specific patterns are resolved lexicographically so results never depend on TOML table order.

The `--private-only` and `--respect-reexports` CLI flags override the configuration for every checked file.

Like Ruff, `no-defaults` discovers the closest `pyproject.toml` containing `[tool.no_defaults]` separately for each file. This supports monorepos with nested configuration; files without a local table continue searching parent directories.

An unrecognised key in `[tool.no_defaults]` is an error, so a misspelled option fails the run rather than silently leaving the defaults in place:

```console
$ no-defaults .
no-defaults: invalid [tool.no_defaults] in pyproject.toml: unknown field `privateonly`, expected one of `private-only`, `private_only`, `respect-reexports`, `respect_reexports`, `per-file-enforcement`, `per_file_enforcement`, `field-base-classes`, `field_base_classes`
$ echo $?
2
```

## Performance

An optimized `1.0.0` development build checked a pinned Typeshed checkout containing 5,368 Python and stub files (12.5 MiB) in a median 0.29 seconds across five warm runs on an Apple Silicon Mac, or roughly 18,000 files per second. It produced 50,974 diagnostics; an earlier full-output measurement used approximately 41 MiB maximum RSS.

CodSpeed runs parser-and-rule benchmarks for representative modules on every pull request and every push to `main`, providing stable comparisons against the default-branch baseline. The scheduled `Typeshed benchmark` remains as a real-project correctness and gross-regression check.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/no-defaults
    rev: v1.1.0
    hooks:
      - id: no-defaults
```

## License

MIT

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and [CHANGELOG.md](CHANGELOG.md).
