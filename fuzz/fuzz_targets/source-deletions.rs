//! Fuzz overlapping deletions at Unicode character boundaries.

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let Ok(source) = std::str::from_utf8(&data[8..]) else {
        return;
    };
    let ranges: Vec<_> = data[..8]
        .chunks_exact(4)
        .map(|chunk| {
            (
                u16::from_le_bytes([chunk[0], chunk[1]]),
                u16::from_le_bytes([chunk[2], chunk[3]]),
            )
        })
        .collect();
    no_defaults::fuzzing::source_deletions(source, &ranges);
});
