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

The prose in `README.md`, `CHANGELOG.rst`, `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, and `docs/` is linted with [Vale](https://vale.sh). Install it, then run the same two commands CI runs:

```console
vale sync
vale .
```

`vale sync` downloads the style packages named in `.vale.ini` into `.vale/styles`, which is not tracked. A term Vale does not know, such as a Python builtin or a name this project has coined, belongs in `.vale/styles/config/vocabularies/no-defaults/accept.txt`; that file also fixes the spelling of every term in it, so add a project's name there the way the project spells it.

Use `cargo run -- path/to/project` to exercise a development build. Add regression tests for behaviour changes and run `scripts/benchmark-typeshed.sh` when changing parsing, traversal, configuration discovery, or diagnostic generation.

## Pull requests

- Keep each pull request focused.
- Document user-visible behaviour in a news fragment under `newsfragments/change/` (assembled into `CHANGELOG.rst` at release time) and in `docs/reference.md`. `README.md` is a short overview: add to it only when the change alters what the tool is for, how it is installed, or how it is invoked.
- Do not weaken the zero-warning Clippy gate.
- Add a `noqa` or declined fix only when the behaviour is intentional and explained.
