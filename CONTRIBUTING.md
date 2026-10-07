# Contributing

Issues and pull requests are welcome.

## Development

Install stable Rust, then run the same checks as CI:

```console
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

The prose in `README.md`, `CHANGELOG.rst`, `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, and `docs/` is linted with [Vale](https://vale.sh).
Install it, then run the same two commands CI runs:

```console
vale sync
vale .
```

`vale sync` downloads the style packages named in `.vale.ini` into `.vale/styles`, which is not tracked.
A term Vale does not know, such as a Python builtin or a name this project has coined, belongs in `.vale/styles/config/vocabularies/no-defaults/accept.txt`; that file also fixes the spelling of every term in it, so add a project's name there the way the project spells it.

Use `cargo run -- path/to/project` to exercise a development build.
Add regression tests for behaviour changes and run `scripts/benchmark-typeshed.sh` when changing parsing, traversal, configuration discovery, or diagnostic generation.

## Rust visibility

CI checks Rust visibility with [Cargo Hawk](https://github.com/astral-sh/hawk) 0.1.15 and Rust 1.99.0.
Hawk includes callers in the CLI, tests, benchmarks, and doctests when checking the supporting library.

Install the [Hawk 0.1.15 release](https://github.com/astral-sh/hawk/releases/tag/0.1.15) for Linux or macOS and put both `cargo-hawk` and `cargo-hawk-driver` on `PATH`.
Install its matching Rust toolchain, then run the check with Cargo from rustup:

```console
rustup toolchain install 1.99.0
cargo +1.99.0 hawk check -D warnings -D hawk::unnecessary_crate_visibility
```

Update the Hawk version, archive checksum, and matching Rust toolchain together in CI.
The pinned compiler applies to this check only.

## Unused dependencies

CI checks for unused Rust dependencies with [cargo-machete](https://github.com/bnjbvr/cargo-machete) 0.9.2.
Run the same check locally:

```console
cargo install --locked cargo-machete --version 0.9.2
cargo machete
```

Review each finding before removing a dependency, including dependencies used by macros or generated code.

## Dependency policy

CI checks Rust dependency advisories, licences, sources, and duplicate versions with [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) 0.20.2.
The policy in `deny.toml` includes development and platform dependencies.
Run the check locally:

```console
cargo install --locked cargo-deny --version 0.20.2
cargo deny --locked check -D warnings
```

The policy rejects unknown sources, unapproved licences, wildcard requirements, new duplicate versions, and advisory warnings.
Existing duplicate versions have exact-version exceptions with reasons in `deny.toml`.
Review those exceptions when updating dependencies and remove them when the dependency graph permits it.


## Pull requests

- Keep each pull request focused.
- Document user-visible behaviour in a news fragment under `newsfragments/change/` (assembled into `CHANGELOG.rst` at release time) and in `docs/reference.md`.
  `README.md` is a short overview: add to it only when the change alters what the tool is for, how it is installed, or how it is invoked.
- Do not weaken the zero-warning Clippy gate.
- Add a `noqa` or declined fix only when the behaviour is intentional and explained.

## Mutation testing

CI uses [cargo-mutants](https://mutants.rs/) to check `previous_line_start`, `next_line_break`, and `line_break_end`.
The pilot runs the source-line unit tests for each mutation, with a 30-second test timeout and a 180-second build timeout.
Missed mutations and timeouts fail the job, and CI uploads the results for review.
The same check also runs weekly and can be started manually.

Install the pinned tool and run the check locally:

```console
cargo install --locked cargo-mutants --version 26.2.0
cargo mutants --in-place --timeout 30 --build-timeout 180
```

The scope and test command are defined in `.cargo/mutants.toml`.
Review surviving mutations before broadening the pilot.
An initial audit also examined `source_line_starts` and prompted more line-ending assertions.
That function remains outside the CI pilot because mutations to its cursor updates can loop indefinitely.
The pilot treats every timeout as a failure rather than accepting timeouts as caught mutations.

The tool is pinned to 26.2.0 because 27.1.0 ignores regex filters for struct-field mutations.
Update the pin after the fix for [cargo-mutants #632](https://github.com/sourcefrog/cargo-mutants/issues/632) is released.

## Fuzz testing

The [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) targets exercise our core routines with generated inputs.
CI runs each target for 60 seconds on pull requests and pushes, and for 10 minutes in the weekly run.
Each input has a 10-second timeout and the input size is capped at 4096 bytes.
Crashes and timeouts fail the job, which uploads failure artifacts.
Fuzz-only entry points are compiled with `cfg(fuzzing)` and do not add APIs to normal builds.

Use Cargo from rustup for the toolchain-qualified commands.
Install the pinned tool and compiler:

```console
cargo install --locked cargo-fuzz --version 0.13.2
rustup toolchain install nightly-2026-09-05 --profile minimal --component rust-src
```

The targets are `source-analysis`, `source-deletions`.
Use one target name in place of `TARGET` below:

```console
mkdir -p fuzz/corpus/TARGET
cp fuzz/seeds/TARGET/* fuzz/corpus/TARGET/
cargo +nightly-2026-09-05 fetch --locked --manifest-path fuzz/Cargo.toml
cargo +nightly-2026-09-05 fuzz run TARGET -- -max_total_time=60 -timeout=10 -rss_limit_mb=2048 -max_len=4096
```

The fuzz package has its own lockfile and a cargo-deny policy that also audits the fuzzing dependencies.
The policy permits NCSA because the LLVM fuzzing runtime requires it.
CI fetches its locked dependencies, builds offline, and checks that the lockfile stays unchanged.
Keep minimized failures as regression tests, and add useful starting inputs to `fuzz/seeds`.
The generated corpus and failure artifacts are ignored by Git.
When using a prebuilt cargo-fuzz binary, pass `--target` with the host Rust target if its default differs from your compiler.
CI explicitly uses `x86_64-unknown-linux-gnu`.
Update the tool and nightly compiler pins together after validating every target.
