//! Fuzz source analysis in both visibility modes.

#![no_main]

libfuzzer_sys::fuzz_target!(|source: &str| {
    no_defaults::fuzzing::source_analysis(source);
});
