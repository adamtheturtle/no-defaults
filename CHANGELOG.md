# Changelog

All notable changes follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases follow semantic versioning.

## Unreleased

### Added

- Defaults on pydantic models are reported. A class is now checked for fields when it names a base class from `field_base_classes`, which defaults to `[ "pydantic.BaseModel" ]`, as well as when it carries `@dataclass`. Its violations are named `class field` where a dataclass's are named `dataclass field`. Codebases built on `BaseModel` will see violations that earlier versions passed over in silence; `field_base_classes = []` restores the old behaviour, and the same setting extends the rule to `msgspec.Struct`, `sqlmodel.SQLModel`, or anything else that carries fields through a base class.
- `Field(default=…)` and `Field(default_factory=…)` are fixed the way `field(...)` is, removing only those arguments and keeping the rest of the metadata. Pydantic writes a field with no default as `Field(...)` or `Field(default=...)`, and neither is reported. A model's call sites gain the removed default as a keyword argument like any other.
- `--show-settings` reports `field-base-classes`.

- `--fix` now updates call sites. Every call in the checked files that relied on a removed default gains it as an explicit argument, so `connect("h")` becomes `connect("h", timeout=30)` and `Job("j")` becomes `Job("j", retries=3)`. A `default_factory` becomes the value it produces. Arguments are appended as keywords except for positional-only parameters. `--diff` previews these edits too. This supersedes the 1.1.0 warning that call sites were left alone.
- Calls are resolved through the calling file's own imports rather than by matching the bare name, so a project with its own `connect` does not have `socket.connect` rewritten, and two modules that each define `helper` are told apart. A method is rewritten when reached through `self`, `cls`, or a class the file can name, whether local, imported, or reached through an imported module, and what it already receives is accounted for, so `instance.fetch(url)`, `Client.fetch(instance, url)`, and a `staticmethod` reached through either are each filled in correctly.
- `--fix` warns, naming the file and line, about each call it left alone: one it cannot tie to the definition that was fixed, a call unpacking `*args` or `**kwargs`, a removed default that is not a literal, a positional-only argument that cannot be appended, a dataclass that inherits fields, or a function named without being called such as a bare `@decorator`. It still warns that callers outside the checked files, and dynamic calls, are beyond its reach.
- Fields that `__init__` never accepts are never added to a call: `field(..., init=False)`, a `_: KW_ONLY` marker, and a class whose decorator says `init=False`.
- A file exempted with `per_file_enforcement = "none"` keeps its own defaults but still has its call sites updated when a callable it uses is fixed elsewhere.
- `respect_reexports`, and the matching `--respect-reexports` flag, treat a name that a package's `__init__.py` re-exports as public in private-only mode. A helper in `_upload.py` that the package root exports through an import or `__all__` keeps its defaults, because they are public API. A module re-exported under its own name, as in `from . import _upload`, makes what it holds public in the same way. Names behind a `from ... import *` cannot be listed, so every name in that package counts as re-exported. Off by default.
- `--show-settings` reports `respect-reexports` alongside the enforcement level.
- A package's `__init__.pyi` is read where it has no `__init__.py`, so a stub-only distribution is treated as a package too. A namespace package, which has neither, no longer hides what the packages above it re-export.

### Performance

- Converting an offset to a line and column is a binary search over each file's line starts rather than a scan from the top of the file, so producing a file's diagnostics is linear in how many it holds instead of quadratic. On a file with 64,000 violations `--output-format concise` went from 5.2 s to 0.06 s, and `full` from 1.7 s at 16,000 to 0.17 s at 64,000.
- The default `full` output reads and indexes each file once instead of rereading it and walking to the reported line for every diagnostic in it. On a file with 16,000 violations this took reporting from 1.7 s to 0.36 s, which is what the same run costs in `concise`.
- `per_file_enforcement` glob patterns are compiled once per configuration file rather than once per checked file. Over 3,000 files with a 40-pattern table this took a run from 1.1 s to 0.07 s, which is what the same run costs with no patterns at all.

### Added

- Defaults on `lambda` parameters are reported. A lambda takes the same parameter kinds as a `def` and carries the same late-binding hazard, and the rule was documented as covering defaults in function signatures without excluding them. Because a lambda is anonymous, `--fix` cannot resolve its call sites, so removing one is reported as a call it left alone. The loop-capture idiom `lambda x=x: ...` needs a `# noqa: NOD001`.

### Fixed

- Module privacy is judged from the path below the project root rather than from every component of the path as written. A checkout living under a directory whose name starts with an underscore — `_work/proj` — no longer has every symbol in it treated as private under `private_only`, and the answer no longer depends on whether a relative or an absolute path was passed on the command line.
- A renaming import of a `dataclasses` or `pydantic` member is followed, so `from dataclasses import dataclass as dc` makes `@dc` a dataclass decorator. The class was previously not treated as a dataclass at all, and its field defaults went unreported. The same applies to `field`, `Field`, and the `KW_ONLY` marker, including imports inside an `if TYPE_CHECKING:` block.
- `--fix` updates the construction sites of a dataclass whose only bases declare no fields. Any base at all previously marked the constructor unknown, so a generic dataclass built on `Generic[T]` — or on `Protocol`, `ABC`, or `object` — had its fields made required while `Box()` was left as it was, raising `TypeError` at runtime with only a warning to mark it. That safe path could never be escaped, however the project was laid out. A base that may carry fields still gives up.
- `--fix` no longer strips `= ...` from a `.pyi` stub. In a stub that is the convention for "this parameter has a default, unspecified here", not a default value, so removing it made the parameter required and stopped the stub matching the implementation it describes. It is still reported; it is now counted as remaining rather than fixed. Outside a stub, `= ...` is an ordinary default and is still removed.
- `--fix` honours `--output-format`. It returned before diagnostics were ever reported, so `--output-format json --fix` printed human-readable text and `--output-format github --fix` produced no annotations at all. Under the machine formats the diagnostics are now all that reaches standard output, with the `N fixed` summary and the call-site count moved to standard error so the output stays parseable; the text formats are unchanged.
- `--fix` no longer panics or silently deletes unrelated code when a `noqa` directive sits inside a multi-line default. Removing the default already removes the directive, so the two deletions overlapped; applying both in turn used the offsets the first had already invalidated. Overlapping deletions are now merged into the span they cover.
- `--fix` no longer deletes a pragma that follows an unused `# noqa: NOD001` on the same line. A `#` runs to end of line, so `# noqa: NOD001  # type: ignore[misc]` is one comment, and removing the directive took the mypy suppression — or a `# pylint: disable`, a `# pragma: no cover`, or a plain explanation — with it. The deletion now stops at the next `#`.
- A `noqa` directive is recognised anywhere in a comment rather than only at its start, matching Ruff and flake8, so `# type: ignore[misc]  # noqa: NOD001` suppresses the rule. That combination previously did nothing and reported nothing, because the directive was never collected at all, and reordering the pragmas is not always possible. `--fix` removes only the directive's own `#` segment, leaving the other pragma in place.
- `# ruff:noqa` and `# flake8:noqa` without a space after the colon are recognised as file-level suppressions. Ruff and flake8 both accept that form, and it is common in the wild; only the spaced variants worked before.

### Changed

- A file the parser rejects is reported as a `NOD000` syntax-error diagnostic and the run continues, instead of aborting with exit status `2` and printing nothing for any file. One unparseable file in a tree — a Python 2 module kept for reference, a template saved as `.py`, a file caught mid-edit — no longer hides every other file's diagnostics, which under pre-commit turned into a hook that silently stopped catching regressions. `NOD000` carries no fix, so `--fix` leaves that file alone, counts it as remaining, and exits `1`.
- A leading UTF-8 byte-order mark is no longer counted as source, so diagnostics on the first line of a BOM-prefixed file report the right column instead of three too far right. `--fix` writes the mark back.
- Diagnostic columns count characters rather than bytes, matching Ruff. Non-ASCII text earlier on a line no longer shifts the reported column in `full`, `concise`, `json`, and `github` output, or pushes the `^` in `full` output past what it points at, so editor and CI annotations land in the right place.
- `--fix` writes through a symlink to the file it points at, instead of replacing the link with a regular file and leaving the real source unfixed. Directory walks never followed links, so this only affected a link named on the command line — which is exactly what pre-commit and shell globs produce.
- Files are deduplicated by canonical path, so naming one file twice under different spellings — `d.py` and `./d.py`, a relative and an absolute form, or a symlink and its target — checks and reports it once instead of twice. The first spelling in sorted order is what diagnostics name.
- A path named on the command line that exists but is not a `.py` or `.pyi` file is now an operational error rather than being silently dropped. A run that checked nothing was previously indistinguishable from a clean one, so a mistyped path, a wrong `types` setting in `.pre-commit-config.yaml`, or a shell glob that matched the wrong thing reported success over code it never opened. Directory walks still filter to Python files.
- An unrecognised key in `[tool.no_defaults]` is now an error rather than being silently ignored. Because the presence of that table is also what makes configuration discovery stop at a `pyproject.toml`, a misspelled option previously produced a run that looked configured but used the defaults throughout. This rejects configurations that earlier versions accepted.

### Documentation

- Document which call sites `--fix` updates, which it deliberately leaves alone, and why.

## 1.1.0 - 2026-08-06

### Added

- A `noqa` directive on the line holding `def` suppresses `NOD001` for every parameter of that signature, and one on the line holding `class` suppresses it for every field of a dataclass, so neither needs one directive per violation. The scope stops at the signature or the class body, and such a directive is reported as `NOD002` when it covers no defaults.
- `NOD002` reports a `noqa` directive that names `NOD001` without suppressing anything, inline or file-level. `--fix` removes the directive, or just the `NOD001` code when the directive lists others. Blanket directives are never reported because they may belong to another linter, and a blanket `# ruff: noqa` or `# flake8: noqa` still silences the whole file.
- `per_file_enforcement` accepts `"none"` to exempt matching files from the rule, for private modules whose API is re-exported publicly. It takes precedence over `--private-only`.
- `--fix` now warns that call sites are not updated when it removes defaults.

### Fixed

- A quoted `ClassVar` annotation such as `x: "ClassVar[int]" = 1` is no longer reported as a dataclass field. `dataclasses` resolves the string textually, so it is a class variable, and `--fix` deleted the attribute.

### Documentation

- Document that `--fix` rewrites signatures only, so callers that relied on a removed default fail at runtime.
- Document registering `NOD` in Ruff's `lint.external`, without which `ruff check --fix` deletes `# noqa: NOD001` suppressions.
- Document that a private module re-exported from a public one is still checked, and that `# noqa: NOD001` is the remedy.

## 1.0.1 - 2026-08-05

### Fixed

- Annotated assignments inside the methods of a dataclass are no longer reported as dataclass fields. They are locals, and `--fix` removed their values to leave bare annotations that raised `NameError` at runtime.

### Added

- Continuous Rust performance tracking with CodSpeed and Divan benchmarks.

## 1.0.0 - 2026-08-05

### Added

- Ruff-style closest-configuration discovery and per-file enforcement.
- Ruff-style full, concise, JSON, and GitHub Actions output.
- `--fix`, `--diff`, `--show-settings`, inline `noqa`, and file-level `ruff: noqa` support.
- Atomic fixes that preserve non-default dataclass `field(...)` metadata.
- A pinned Typeshed benchmark and real-project integration fixture.
- Cross-platform wheel, GitHub Release, and trusted PyPI publishing automation.

### Changed

- Unannotated dataclass class attributes are no longer treated as fields.

## 0.2.0 - 2026-08-05

- Added per-file enforcement, private modules, Ruff-like summaries, and automatic fixes.

## 0.1.0 - 2026-08-05

- Initial public release.
