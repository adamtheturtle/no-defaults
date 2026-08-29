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

Use `cargo run -- path/to/project` to exercise a development build. Add regression tests for behavior changes and run `scripts/benchmark-typeshed.sh` when changing parsing, traversal, configuration discovery, or diagnostic generation.

## Pull requests

- Keep each pull request focused.
- Document user-visible behavior in `CHANGELOG.md`, and in `docs/reference.md`. `README.md` is a short overview: add to it only when the change alters what the tool is for, how it is installed, or how it is invoked.
- Do not weaken the zero-warning Clippy gate.
- Add a `noqa` or declined fix only when the behavior is intentional and explained.
