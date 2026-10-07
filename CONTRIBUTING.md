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

## Pull requests

- Keep each pull request focused.
- Document user-visible behaviour in a news fragment under `newsfragments/change/` (assembled into `CHANGELOG.rst` at release time) and in `docs/reference.md`.
  `README.md` is a short overview: add to it only when the change alters what the tool is for, how it is installed, or how it is invoked.
- Do not weaken the zero-warning Clippy gate.
- Add a `noqa` or declined fix only when the behaviour is intentional and explained.



## Property testing

The Rust unit tests use [proptest](https://github.com/proptest-rs/proptest) to generate inputs and shrink failures.
Run them with `cargo test --locked --lib property_tests`.
They also run in the normal CI test suite, with 256 cases per property by default.
For a longer local run, set `PROPTEST_CASES=4096`.
Commit generated `proptest-regressions` files when a failure is fixed so the minimal failing input remains covered.
