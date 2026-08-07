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

- `per_file_enforcement` glob patterns are compiled once per configuration file rather than once per checked file. Over 3,000 files with a 40-pattern table this took a run from 1.1 s to 0.07 s, which is what the same run costs with no patterns at all.

### Changed

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
