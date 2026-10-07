//! Fuzz-only checks of source analysis and Unicode-aware deletions.

use crate::{apply_edits, lint_source, text_size, Edit, TextRange};

/// Analyze arbitrary source in both visibility modes without external tools.
///
/// # Panics
///
/// Panics if the generated input violates the asserted invariant.
pub fn source_analysis(source: &str) {
    for private_only in [false, true] {
        assert_eq!(
            lint_source(source, private_only),
            lint_source(source, private_only)
        );
    }
}

/// Deleting valid character ranges must retain exactly the other characters.
///
/// # Panics
///
/// Panics if the generated input violates the asserted invariant.
pub fn source_deletions(source: &str, ranges: &[(u16, u16)]) {
    let boundaries: Vec<_> = source
        .char_indices()
        .map(|(index, _)| index)
        .chain([source.len()])
        .collect();
    let ranges: Vec<_> = ranges
        .iter()
        .take(32)
        .map(|&(start, end)| {
            let first = boundaries[usize::from(start) % boundaries.len()];
            let last = boundaries[usize::from(end) % boundaries.len()];
            first.min(last)..first.max(last)
        })
        .collect();
    let edits = ranges
        .iter()
        .map(|range| Edit::deletion(TextRange::new(text_size(range.start), text_size(range.end))))
        .collect();
    let expected: String = source
        .char_indices()
        .filter(|(index, _)| !ranges.iter().any(|range| range.contains(index)))
        .map(|(_, character)| character)
        .collect();
    let (actual, inserted) = apply_edits(source, edits);
    assert_eq!(actual, expected);
    assert_eq!(inserted, 0);
}
