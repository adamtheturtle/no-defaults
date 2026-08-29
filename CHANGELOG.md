# Changelog

All notable changes follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases follow semantic versioning.

## Unreleased

### Added

- `--fix` resolves call sites it previously left alone. A method reached through a constructed instance — `C().fetch()` — an alias bound in a class body, an inherited method reached through `self` or `super()`, and a normal class's `__init__` reached as `C(...)` are all rewritten now. So are calls through star imports, package attributes, unaliased dotted imports, namespace packages, and a `staticmethod` or `classmethod` reached through the binding it was imported under. A nested function or dataclass is resolved within the scope that holds it, and a class nested in another scope is told apart from a module-level class of the same name.
- `PYTHONPATH` is used when resolving imports, so a project whose layout depends on it has its call sites updated. Where an explicit root and the importer's own directory name different files the import is ambiguous, and the call is reported rather than rewritten against a guess.
- `--diff` lists the diagnostics it cannot fix, and `--fix` prints the ones that remain after writing, so neither mode implies everything was handled.
- A `# noqa` may carry an explanation after a blanket directive and after a file-level one, and Flake8's whitespace forms are accepted.

### Changed

- A default on `__new__` is reported but retained because object construction invokes it implicitly and has no explicit call site the fixer can safely update.
- A default on `__del__` is reported but retained because object finalization invokes it implicitly.
- A default on `__getattribute__` is reported but retained because attribute syntax invokes it implicitly.
- A default on `__getattr__` is reported but retained because fallback attribute lookup invokes it implicitly.
- A default on `__setattr__` is reported but retained because attribute assignment invokes it implicitly.
- A default on `__delattr__` is reported but retained because attribute deletion invokes it implicitly.
- A default on `__dir__` is reported but retained because `dir()` invokes it implicitly.
- A default on descriptor `__get__` is reported but retained because attribute reads invoke it implicitly.
- A default on descriptor `__set__` is reported but retained because attribute writes invoke it implicitly.
- A default on descriptor `__delete__` is reported but retained because attribute deletion invokes it implicitly.
- A default on descriptor `__set_name__` is reported but retained because class creation invokes it implicitly.
- A default on metaclass `__instancecheck__` is reported but retained because `isinstance()` invokes it implicitly.
- A default on metaclass `__subclasscheck__` is reported but retained because `issubclass()` invokes it implicitly.
- A default on `__subclasshook__` is reported but retained because ABC subclass checks invoke it implicitly.
- A default on `__class_getitem__` is reported but retained because class subscription invokes it implicitly.
- A default on `__mro_entries__` is reported but retained because base-class resolution invokes it implicitly.
- A default on metaclass `__prepare__` is reported but retained because class creation invokes it implicitly.
- Metaclass `__new__` is covered by the same retention as object `__new__`, preserving implicit class construction too.
- A default on metaclass `__init__` is reported but retained because class creation invokes it implicitly; ordinary class constructors remain fixable.
- A default on metaclass `mro` is reported but retained because class creation invokes it implicitly.
- A default on `__await__` is reported but retained because an `await` expression invokes it implicitly.
- A default on `__sizeof__` is reported but retained because `sys.getsizeof()` invokes it implicitly.
- A default on `__copy__` is reported but retained because `copy.copy()` invokes it implicitly.
- A default on `__deepcopy__` is reported but retained because `copy.deepcopy()` invokes it implicitly.
- A default on `__reduce__` is reported but retained because pickling invokes it implicitly.
- A default on `__reduce_ex__` is reported but retained because versioned pickling invokes it implicitly.
- A default on `__getnewargs__` is reported but retained because pickling invokes it implicitly.
- A default on `__getnewargs_ex__` is reported but retained because extended pickling invokes it implicitly.
- A default on `__getstate__` is reported but retained because pickling invokes it implicitly.
- A default on `__setstate__` is reported but retained because unpickling invokes it implicitly.
- A default on `__replace__` is reported but retained because `copy.replace()` invokes it implicitly.
- A default on `__buffer__` is reported but retained because creating a `memoryview` invokes it implicitly.
- A default on `__release_buffer__` is reported but retained because releasing a `memoryview` invokes it implicitly.
- A default on an iterator's `send()` method is reported but retained because `yield from` invokes it implicitly while forwarding values.
- Defaults on an iterator's `throw()` method are reported but retained because `yield from` invokes it implicitly while forwarding exceptions.
- A default on an iterator's `close()` method is reported but retained because closing a delegating generator invokes it implicitly.
- A default on module-level `__getattr__` is reported but retained because module attribute fallback invokes it implicitly.
- A default on module-level `__dir__` is reported but retained because `dir(module)` invokes it implicitly.
- A default on a parameter of an implicitly called method is reported but never removed. The interpreter is the caller, so there is no call site that could be given the argument back, and removing it changed behaviour. This covers the context manager, iterator and async iterator protocols, `__len__` and `__length_hint__`, subscription, `__contains__`, `__missing__`, `__reversed__`, the comparison, arithmetic, bitwise and matrix operators, `divmod` and `pow`, the `str`, `repr`, `bytes`, `format`, `hash`, `bool`, `index`, `int`, `float`, `complex` and `os.PathLike` conversions, `round`, `trunc`, `floor` and `ceil`, along with `__call__`, `__init_subclass__`, a dataclass's `__post_init__`, and a property getter.
- Defaults in a `.pyi` stub are reported but retained. A stub describes a signature rather than supplying one, so removing the default there does not match what the implementation does.
- A decorator that is not `dataclass` keeps the defaults of what it decorates, because it may replace the callable or the constructor that the rewritten call sites would target. This applies to a decorator on a function and on a class, and in every form it is written in: a bare name, a factory such as `@replace()`, and an attribute such as `@mod.replace`.
- Whether a class carries fields is decided by what its names are bound to rather than by how they are spelled. A locally defined `dataclass`, `field` or `Field`, a `BaseModel` declared in the file, an aliased `ClassVar`, and a `KW_ONLY` marker reached through a `dataclasses` import are each resolved to the definition in force at that point, and a name rebound later stops standing for what it did before.
- A class whose field list is not one reliable shape is left alone. Fields declared inside a conditional, a loop, a `try`, a `match` case, or an `if TYPE_CHECKING:` block do not describe the constructor the class ends up with, and a field that a later statement deletes or overwrites is not treated as settled. Imports in those blocks still bind the names the rest of the file is written against.
- A name beginning with two underscores, other than a dunder, counts as private, and a Pydantic private attribute or an underscore-prefixed model attribute is not a field.

### Fixed

- Class-body aliases created with `staticmethod()` and `classmethod()` carry the wrapper's receiver convention; `property()` aliases retain defaults consumed by descriptor access.
- Class-body control-flow suites are scanned conservatively for method aliases, while nested definition scopes remain excluded.
- Annotated assignments, statically paired destructuring, and named expressions in class bodies record method aliases for call rewriting.
- Transitive class-body method aliases resolve to the original method signature, so calls through every alias receive removed defaults.
- Staticmethod and classmethod aliases imported inside `match` cases are available to call rewriting.
- Staticmethod and classmethod aliases imported inside a `while` body or its `else` suite are available to call rewriting.
- A match-pattern capture invalidates any imported dataclass identity it may replace before its guard and case body are analyzed.
- A named-expression target invalidates any imported dataclass identity it replaces after its value has been evaluated.
- A `with ... as` target invalidates any imported dataclass identity it replaces, both inside and after the suite.
- A `for` target invalidates any imported dataclass identity it may replace, while a statically empty loop leaves the identity unchanged.
- A user-defined helper named `PrivateAttr` is treated as an ordinary configured-model field default; only a resolved Pydantic helper suppresses field handling.
- Assigning over an imported `TYPE_CHECKING` alias invalidates its type-only identity, so a statically live guarded branch is analyzed and fixed normally.
- Assigning over an imported `abc` module alias invalidates its module identity, so a dataclass reached through the replacement namespace contributes its inherited fields.
- Assigning over an imported `typing.Generic` or `Protocol` alias invalidates its structural-base identity, so a dataclass rebound under that name contributes its inherited fields.
- Assigning over an imported Pydantic `Field` helper invalidates that imported identity and retains the replacement call, so its arguments are not edited as though it were still the standard helper.
- Assigning over an imported `dataclasses.field` helper invalidates that imported identity and retains the replacement call, so its arguments are not edited as though it were still the standard helper.
- Assigning over an imported `dataclass` decorator invalidates that imported identity, so later ordinary classes are not rewritten as dataclasses.

- Calls through checked symbols and modules imported directly in a class body now receive removed defaults, while those class imports remain invisible to method lexical lookup.
- Deleting a class-local shadow restores global name lookup for later statements in the class body, so calls to the global callable are rewritten with removed defaults.
- A selected class-body `match` case installs its pattern captures in the class namespace before evaluating the guard and body, so calls on captured values no longer use stale global imports.
- A named expression in a class body binds its target in the class namespace after evaluating its value, so later calls no longer resolve through a stale global import.
- A class-body `except … as` target shadows an imported callable while its handler runs and is deleted afterwards; calls on the caught object remain unchanged and the affected default is retained.
- A class-body `with … as` target shadows an imported callable before the context-manager suite runs; calls on its value remain unchanged and the affected checked default is retained.
- A class-body `for` target shadows an imported callable before the loop body runs; calls on its value remain unchanged and the affected checked default is retained.
- An unresolved dotted import invalidates the top-level name Python binds, rather than leaving an earlier checked module reachable through that stale name.
- An unresolved module-level `from … import name` still invalidates an earlier checked binding for `name`; later calls are left unchanged and the affected default is retained.
- Captures in a selected module-level `match` case shadow imported callables before its guard and body are evaluated, preventing calls on captured values from being rewritten against stale imports.
- Literal module-level `match` statements update bindings only from the selected case; imports in cases that do not match no longer affect later calls.
- A module-level `while … else` suite is skipped when a statically true loop body ends in `break`, so imports in the unentered `else` no longer replace live bindings.
- Imports in a statically false module-level `while` body no longer replace the binding used for later calls; its test is still checked and its `else` suite still runs.
- A module-level `for … else` suite is skipped when a known non-empty literal loop body ends in `break`, so imports in the unentered `else` no longer replace live bindings.
- Imports in the body of a module-level `for` over an empty tuple, list, set, or dictionary no longer replace the binding used for later calls.
- Imports in an exception handler after a non-raising module-level `try: pass` suite no longer replace the successful path's binding for later calls.
- Imports in a statically unreachable module-level `if`, `elif`, or `else` suite no longer replace the binding used to resolve later calls.
- A module-level `except … as` target shadows an imported callable while its exception handler runs, so calls on the caught object are no longer rewritten against the stale import.
- A module-level `with … as` target shadows an imported callable inside the context-manager body, after its context expression has been evaluated.
- A module-level `for` target shadows an imported callable inside the loop body as soon as the first item is assigned, so calls there are no longer rewritten against the stale import.
- Module-level named expressions invalidate an earlier imported callable binding before later calls are resolved, while the expression assigning the replacement still sees the old binding.
- Module-level `def` and `class` statements invalidate an earlier imported callable binding before later calls are resolved. When a checked call can no longer be tied safely to a changed callable, that callable's default is retained rather than merely warning after breaking the caller.
- `--fix` removes wrapping parentheses with a parameter default, so a default such as `x=(1)` no longer leaves an unparseable closing parenthesis behind and prevents the file's other fixes.
- Parenthesized defaults on lambda parameters are removed with their wrapping parentheses too.
- A parenthesized bare dataclass field default is removed through its closing parenthesis, so it no longer prevents every fix in the file.
- A definition named through a symlink and one named through its target are one module, so a call is tied to the definition that was fixed rather than left behind. A directory walk follows symlinked Python files, a fix writes through to the target instead of replacing the link, and a hard-linked file is refused rather than silently detached from its other names. Two spellings of one path given on the command line are checked once.
- Names are bound in statement order, so a call earlier in a file is resolved against what the name meant there. An import, a class-body assignment, a dataclass alias and a module-level rebinding no longer reach backwards over calls that precede them.
- A diagnostic's quoted source line escapes control characters instead of passing them to the terminal, and its caret is placed by display width, so a tab or a wide character does not push it out of line.
- `--fix` keeps a signature valid: a positional default is inserted ahead of the keywords it precedes, a lambda keeps its positional order, and a default that has to stay keeps the defaults after it.
- A removed `default_factory` is not recreated at the call site when its value is a container, so callers do not start sharing one object.
- A closed stdout pipe, a non-UTF-8 source file, and a directory that cannot be walked are each reported without aborting the run, `--diff` rejects the machine-readable output formats it cannot produce, and `--show-settings` rejects being combined with a mode that writes.
- Defaults on the reflected (`__radd__`), augmented-assignment (`__iadd__`), and unary (`__neg__`, `__pos__`, `__abs__`, `__invert__`) operator methods are retained. Python reaches all of them through syntax rather than a written call, so `--fix` had nothing to add the removed default back to and could leave the operator raising `TypeError`.
- A default on a lambda written in an `if` or `elif` test is reported and fixed again. Clause tests were walked for their truthiness but never checked.
- `for _ in {}:` is recognised as a loop that never runs, alongside the empty tuple, list, and set. Its body no longer holds back defaults on later fields.
- The word `noqa` in a note after an unrelated directive — `# noqa: E501  keep noqa` — no longer suppresses `NOD001`. A bare `noqa` counts only when it opens its own comment.
- `Updated N call sites` counts calls rather than edits. A call that needed both a positional and a keyword insertion was counted twice.
- A dataclass that inherits a metaclass from a base in the same file keeps its field defaults, as one naming `metaclass=` directly already did. A metaclass is inherited, and it controls construction before the generated initializer runs.
- A dataclass defined under `if TYPE_CHECKING:` keeps its field defaults. The block does not run, so the class has no constructor at runtime and no call site the fixer can keep in step with it.
- A call in a comprehension's leftmost iterable is updated again. Python evaluates that iterable before the loop targets exist, so a call there is the enclosing one and was being skipped as shadowed while its default was removed.
- Rebinding a package name drops the dotted import under it. After `import pkg.api`, a later `pkg = …` left the `pkg.api` entry in place and calls through it were still rewritten as the original module's.
- A module-level loop or `with` target replaces an imported name, as an assignment to it already did. Calls through the name were still being rewritten as the import's. An `except … as` target does not: the name is deleted when the handler ends, and if the handler never runs the import still binds.
- Two definitions of one name in different branches of an `if`, loop, `with`, `try`, or `match` keep their defaults, as two written side by side already did. Which one survives is not knowable, and `--fix` was stripping both while leaving the call as written.
- Two definitions of one class in different branches that name different bases keep the defaults of every method either ancestry holds. Which base the class ends up with is not knowable, and `--fix` was resolving inherited calls against whichever branch was read last and stripping the defaults behind the others. A branch whose test reads as always true still settles the question, and so does a definition written after the branches. A method the class itself, or a subclass of it below the unsettled one, defines is still resolved: it does not depend on which ancestry ran.
- A name that a branch may or may not rebind leaves the ancestry of a subclass built on it unsettled, as two definitions of the class itself already do. After `if …: Alias = First` / `else: Alias = Second`, or an `if` with no `else` that rebinds a name bound above it, a `class Child(Alias)` was resolved against one of the candidates and the others' defaults were stripped. The parameterized spelling `class Child(Alias[int])` counts as the same base, and a contest found inside a `with` or an `if` whose test reads as true still reaches the code after it. A suite that is certain to run does settle the name it binds, even where it binds what the scope already held, and so does an assignment outside the branches. Reading one name from another hands the doubt on, so `Other = Alias` is no more settled than `Alias` was.
- A class written in an `if`, loop, `with`, `try`, or `match` suite inherits from a class written outside it. A suite opens no scope of its own, but its classes were being resolved as though it did, so a subclass in one was recorded with no bases at all and an inherited call kept its arguments while the default behind it was removed.
- A class name in an alias such as `Alias = Base` is looked up the way Python looks it up: the scope holding it first, then the functions and the module around it, never an enclosing class body, which is not a closure scope. The search stops at the nearest scope that binds the name at all, so a parameter or a rebinding standing in the way hides what is beyond it. A dataclass built on such an alias was taking its inherited fields from a namesake the alias never reaches.
- A name a class body binds by assignment shadows what its bases hold, as a `def` of that name already did. A subclass writing `__init__ = setup` has a constructor of its own, and `--fix` was rewriting its calls against an ancestor's `__init__` with parameters the alias does not take. Only what the body leaves behind counts: a `def` under `if TYPE_CHECKING:`, an `except … as` target, an assignment under `global`, and a name the body `del`etes again are none of them on the class.
- A call inside an annotation in a `.pyi` stub is left alone, as one under `from __future__ import annotations` already was. A stub postpones its annotations without the import, so the call is a type expression and rewriting it edited a file that needed no change.
- A class a class body writes is out of scope for the methods and the nested classes written under it. A class body is not a closure, so `class Child(Base)` inside a method is built on the module-level `Base`; the sibling of that name was hiding an unseen imported base, and the metaclass it may carry, from the classes below it.
- A bare module-level annotation such as `super: object` no longer counts as shadowing the builtin it names. Nothing is put behind the name, so `super()`, `staticmethod`, `property`, and the dataclass field helpers still reach what they always did; an inherited `super().method()` was being left as written while the method's default was removed.
- An alias such as `Alias = Base` written in a class body resolves the name the way Python does: against the enclosing functions and then the module, never against an outer class body, which is not a closure scope, and stopping at the nearest scope that binds the name at all. A dataclass inheriting through such an alias was being given another class's fields.
- `import package.module` makes `package.module.Base` the submodule's class, over a same-named class the package initializer defines. The initializer's class was winning, and inherited calls were rewritten with defaults the real base never had. A class written here above the subclass still takes the name back off the import, and everything spelled under it goes with the name.
- A default on the `__init__` of an `enum.Enum` subclass is reported but retained. Assigning a member calls it implicitly while the class is created, so removing the default made the class itself raise `TypeError`, and there is no written call site to keep in step: the calls are the member assignments the interpreter makes.

## 2.1.0 - 2026-08-07

Anyone on 2.0.0 who uses `--fix` on a project containing `.pyi` stubs should upgrade: see the first entry under `### Fixed`.

### Added

- `--fix` updates the construction sites of a dataclass whose base is a dataclass in the same file. The base's fields come first in the generated constructor, so `Child()` becomes `Child(a=1, b=2)` where `a` is the base's, and a subclass that removed nothing of its own still gets back the defaults its base lost. It stays narrow: one base only, because `dataclasses` walks the reverse MRO to order several; the base must be named directly and its own constructor fully known; and a name two classes share resolves to neither. Anything else keeps the existing warning.

### Changed

- A file whose fix would not have parsed is left alone by itself, rather than aborting the whole run with exit status `2` and writing nothing anywhere. One file this linter has a bug on no longer blocks fixing an entire project. The file is named in a warning, counted as remaining rather than fixed, and the run exits `1` with an accurate summary.

### Fixed

- A default that cannot be removed now keeps the defaults after it. Since 2.0.0 made `= ...` in a stub unfixable, a signature mixing it with an ordinary default was rewritten into `def f(x: int = ..., y: int)`, which Python rejects — on the pinned Typeshed checkout that hit 175 files, and 2.0.0 aborted the whole run over it, fixing nothing anywhere. The dataclass form was worse: `a: int = ...` followed by a fixed `b: int` is valid syntax, so it was written out and raised `TypeError: non-default argument 'b' follows default argument 'a'` at import. Keyword-only parameters and fields are exempt, since order does not constrain them.

### Documentation

- The `## Performance` section quotes measured `2.0.0` figures rather than `1.0.0` ones, gives `full` and `concise` side by side, and drops a caveat about full-output memory that the fixes to its quadratic behaviour made obsolete.
## 2.0.0 - 2026-08-07

This release changes what the linter reports, what `--fix` writes, and which configurations it accepts. Read `### Changed` before upgrading a project that pins an earlier version.

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
- Defaults on `lambda` parameters are reported. A lambda takes the same parameter kinds as a `def` and carries the same late-binding hazard, and the rule was documented as covering defaults in function signatures without excluding them. Because a lambda is anonymous, `--fix` cannot resolve its call sites, so removing one is reported as a call it left alone. The loop-capture idiom `lambda x=x: ...` needs a `# noqa: NOD001`.

### Changed

- A file the parser rejects is reported as a `NOD000` syntax-error diagnostic and the run continues, instead of aborting with exit status `2` and printing nothing for any file. One unparseable file in a tree — a Python 2 module kept for reference, a template saved as `.py`, a file caught mid-edit — no longer hides every other file's diagnostics, which under pre-commit turned into a hook that silently stopped catching regressions. `NOD000` carries no fix, so `--fix` leaves that file alone, counts it as remaining, and exits `1`.
- A leading UTF-8 byte-order mark is no longer counted as source, so diagnostics on the first line of a BOM-prefixed file report the right column instead of three too far right. `--fix` writes the mark back.
- Diagnostic columns count characters rather than bytes, matching Ruff. Non-ASCII text earlier on a line no longer shifts the reported column in `full`, `concise`, `json`, and `github` output, or pushes the `^` in `full` output past what it points at, so editor and CI annotations land in the right place.
- `--fix` writes through a symlink to the file it points at, instead of replacing the link with a regular file and leaving the real source unfixed. Directory walks never followed links, so this only affected a link named on the command line — which is exactly what pre-commit and shell globs produce.
- Files are deduplicated by canonical path, so naming one file twice under different spellings — `d.py` and `./d.py`, a relative and an absolute form, or a symlink and its target — checks and reports it once instead of twice. The first spelling in sorted order is what diagnostics name.
- A path named on the command line that exists but is not a `.py` or `.pyi` file is now an operational error rather than being silently dropped. A run that checked nothing was previously indistinguishable from a clean one, so a mistyped path, a wrong `types` setting in `.pre-commit-config.yaml`, or a shell glob that matched the wrong thing reported success over code it never opened. Directory walks still filter to Python files.
- An unrecognised key in `[tool.no_defaults]` is now an error rather than being silently ignored. Because the presence of that table is also what makes configuration discovery stop at a `pyproject.toml`, a misspelled option previously produced a run that looked configured but used the defaults throughout. This rejects configurations that earlier versions accepted.

### Fixed

- `--fix` no longer stops the removal of a `noqa` directive at a `#` that is part of a comment's prose. `# noqa: NOD001  see #35  # type: ignore` left `#35  # type: ignore` behind; a new comment segment now has to open with whitespace or a pragma's `word:`.
- A local name that a file binds to more than one `dataclasses` or `pydantic` member resolves to neither, instead of to whichever import came last.
- A base class named `Generic`, `Protocol`, `ABC`, or `object` that the file itself defines at module level is no longer taken for the typing construct, so a dataclass built on one keeps its construction sites intact.
- A call whose bare generator expression already supplies every default that was removed is no longer reported as left alone, because nothing needed appending to it.
- A nested `def`, `class`, or `lambda` no longer shadows names in the scope holding it, so a call in the outer scope that really does reach a fixed callable is still updated.
- An absolute import is resolved against the ancestors of the importing file that are not themselves packages, rather than stopping at the first directory without an `__init__.py`. A sibling module at the project root is found from inside a namespace package, a top-level import inside a package no longer resolves to that package's own module, and an import that two candidate roots could answer resolves to neither.
- The `Found N errors (N fixed, N remaining)` summary reaches standard error under `--output-format json` and `github`, as documented, rather than being dropped.
- Module privacy is judged from the path below the project root rather than from every component of the path as written. A checkout living under a directory whose name starts with an underscore — `_work/proj` — no longer has every symbol in it treated as private under `private_only`, and the answer no longer depends on whether a relative or an absolute path was passed on the command line.
- A renaming import of a `dataclasses` or `pydantic` member is followed, so `from dataclasses import dataclass as dc` makes `@dc` a dataclass decorator. The class was previously not treated as a dataclass at all, and its field defaults went unreported. The same applies to `field`, `Field`, and the `KW_ONLY` marker, including imports inside an `if TYPE_CHECKING:` block.
- `--fix` accounts for keyword-only dataclass fields when filling in construction sites. The positional order was built from the fields in source order with nothing consulting `kw_only`, but `dataclasses` moves such a field past the `*` in the generated `__init__`, so every field after it really sat one slot lower than assumed. For `@dataclass class C: a = field(kw_only=True, default=1); b = 2`, `C(5)` became `C(5, b=2)` — which raises `TypeError: got multiple values for argument 'b'` — while the default `a` now needs was dropped. `field(kw_only=...)`, `@dataclass(kw_only=True)`, and the `_: KW_ONLY` marker are all honoured, with the per-field setting winning.
- An absolute import is resolved against the importing file's own import root before falling back to matching its dotted name against the checked files' paths, and a one-component import such as `from utils import helper` is only ever resolved against that root. Matching one filename at any depth meant `import utils` resolved to `anything/at/any/depth/utils.py`, silently rewriting calls to an unrelated function — with no warning, because from the resolver's point of view the call resolved cleanly. A dotted import still reaches another source tree, so a test under `tests/` still reaches a package under `src/`.
- `--fix` leaves alone a call to a name that an enclosing function, class, or lambda binds. Resolution had no notion of local scope, so a parameter, assignment, `for` target, `with ... as`, comprehension variable, or `except ... as` that shared a name with a fixed callable resolved to that callable and had its call rewritten — passing an unexpected keyword to an unrelated object, which the README's "Nothing is guessed" promise said would not happen. Such a call is now skipped with the existing warning.
- `--fix` updates call sites reached through `from . import module`, the idiomatic way to import a sibling. The submodule check was guarded on the import naming a module, which a purely relative import does not, so the name was bound as a symbol of the package and every call through it was skipped with a warning — while the same call through `from package import module` was rewritten. `from .sub import module` worked already and is now covered by a test.
- A call taking a bare generator expression, as in `f(x for x in y)`, is left alone with a warning instead of being rewritten into `f(x for x in y, timeout=30)`, which does not parse. The post-fix parse guard caught that, but by turning the whole run into an operational error: nothing was fixed, in any file, and the exit status was `2`. One such call anywhere in a project blocked fixing everything else. A parenthesized generator is still filled in.
- `--fix` updates the construction sites of a dataclass whose only bases declare no fields. Any base at all previously marked the constructor unknown, so a generic dataclass built on `Generic[T]` — or on `Protocol`, `ABC`, or `object` — had its fields made required while `Box()` was left as it was, raising `TypeError` at runtime with only a warning to mark it. That safe path could never be escaped, however the project was laid out. A base that may carry fields still gives up.
- `--fix` no longer strips `= ...` from a `.pyi` stub. In a stub that is the convention for "this parameter has a default, unspecified here", not a default value, so removing it made the parameter required and stopped the stub matching the implementation it describes. It is still reported; it is now counted as remaining rather than fixed. Outside a stub, `= ...` is an ordinary default and is still removed.
- `--fix` honours `--output-format`. It returned before diagnostics were ever reported, so `--output-format json --fix` printed human-readable text and `--output-format github --fix` produced no annotations at all. Under the machine formats the diagnostics are now all that reaches standard output, with the `N fixed` summary and the call-site count moved to standard error so the output stays parseable; the text formats are unchanged.
- `--fix` no longer panics or silently deletes unrelated code when a `noqa` directive sits inside a multi-line default. Removing the default already removes the directive, so the two deletions overlapped; applying both in turn used the offsets the first had already invalidated. Overlapping deletions are now merged into the span they cover.
- `--fix` no longer deletes a pragma that follows an unused `# noqa: NOD001` on the same line. A `#` runs to end of line, so `# noqa: NOD001  # type: ignore[misc]` is one comment, and removing the directive took the mypy suppression — or a `# pylint: disable`, a `# pragma: no cover`, or a plain explanation — with it. The deletion now stops at the next `#`.
- A `noqa` directive is recognised anywhere in a comment rather than only at its start, matching Ruff and flake8, so `# type: ignore[misc]  # noqa: NOD001` suppresses the rule. That combination previously did nothing and reported nothing, because the directive was never collected at all, and reordering the pragmas is not always possible. `--fix` removes only the directive's own `#` segment, leaving the other pragma in place.
- `# ruff:noqa` and `# flake8:noqa` without a space after the colon are recognised as file-level suppressions. Ruff and flake8 both accept that form, and it is common in the wild; only the spaced variants worked before.

### Performance

- Converting an offset to a line and column is a binary search over each file's line starts rather than a scan from the top of the file, so producing a file's diagnostics is linear in how many it holds instead of quadratic. On a file with 64,000 violations `--output-format concise` went from 5.2 s to 0.06 s, and `full` from 1.7 s at 16,000 to 0.17 s at 64,000.
- The default `full` output reads and indexes each file once instead of rereading it and walking to the reported line for every diagnostic in it. On a file with 16,000 violations this took reporting from 1.7 s to 0.36 s, which is what the same run costs in `concise`.
- `per_file_enforcement` glob patterns are compiled once per configuration file rather than once per checked file. Over 3,000 files with a 40-pattern table this took a run from 1.1 s to 0.07 s, which is what the same run costs with no patterns at all.

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
