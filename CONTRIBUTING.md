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

## Minimum Rust version

The supported minimum Rust version is 1.92, matching the parser dependency.
CI verifies the `Cargo.toml` requirement on Linux, macOS, and Windows with [cargo-msrv](https://github.com/foresterre/cargo-msrv) 0.19.3.

```console
cargo install --locked cargo-msrv --version 0.19.3
cargo msrv verify --no-log -- cargo check --locked --all-targets --all-features
```

When dependencies require a newer compiler, update the declared minimum and verify it with this command.

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
