use std::fmt::Write as _;

fn main() {
    divan::main();
}

fn representative_module(functions: usize) -> String {
    let mut source = String::with_capacity(functions * 160);
    source.push_str("from dataclasses import dataclass, field\n\n");
    for index in 0..functions {
        let _ = write!(
            source,
            "def public_{index}(value: int = 1, *, option: str = 'x') -> int:\n    return value\n\n"
        );
        let _ = write!(
            source,
            "def _private_{index}(value: int = 1) -> int:\n    return value\n\n"
        );
    }
    source.push_str(
        "@dataclass\nclass Model:\n    value: int = 1\n    items: list[int] = field(default_factory=list)\n",
    );
    source
}

#[divan::bench(args = [10, 100, 1_000])]
fn lint_all(bencher: divan::Bencher, functions: usize) {
    bencher
        .with_inputs(|| representative_module(functions))
        .bench_values(|source| no_defaults::lint_source(&source, false));
}

#[divan::bench(args = [10, 100, 1_000])]
fn lint_private(bencher: divan::Bencher, functions: usize) {
    bencher
        .with_inputs(|| representative_module(functions))
        .bench_values(|source| no_defaults::lint_source(&source, true));
}
