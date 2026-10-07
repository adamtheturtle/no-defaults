use proptest::prelude::*;

use super::fixed;
use crate::{line_break_end, next_line_break, previous_line_start, source_line_starts};

proptest! {
    #[test]
    fn generated_line_endings_have_the_constructed_byte_offsets(
        lines in prop::collection::vec(
            ("[^\r\n]{1,20}", prop_oneof![Just("\n"), Just("\r"), Just("\r\n")]),
            0..24,
        ),
        tail in "[^\r\n]{0,20}",
    ) {
        let mut source = String::new();
        let mut starts = vec![0];
        let mut breaks = Vec::new();
        for (content, ending) in lines {
            let start = source.len();
            source.push_str(&content);
            let break_start = source.len();
            source.push_str(ending);
            breaks.push((start, break_start, source.len()));
            starts.push(source.len());
        }
        source.push_str(&tail);
        prop_assert_eq!(source_line_starts(&source), starts.clone());
        for start in starts {
            prop_assert_eq!(previous_line_start(&source, start), start);
        }
        for (start, break_start, end) in breaks {
            prop_assert_eq!(next_line_break(&source, start), break_start);
            prop_assert_eq!(line_break_end(&source, break_start), end);
        }
        prop_assert_eq!(next_line_break(&source, source.len() - tail.len()), source.len());
    }

    #[test]
    fn repeated_empty_lines_have_one_start_per_complete_ending(
        count in 0usize..24,
        ending in prop_oneof![Just("\n"), Just("\r"), Just("\r\n")],
    ) {
        let source = ending.repeat(count);
        let expected: Vec<_> = (0..=count).map(|index| index * ending.len()).collect();
        prop_assert_eq!(source_line_starts(&source), expected);
    }

    #[test]
    fn fixing_generated_defaults_preserves_the_call_and_is_idempotent(
        value in -100_000i32..100_000,
        comment in "[a-zéλ ]{0,24}",
        ending in prop_oneof![Just("\n"), Just("\r"), Just("\r\n")],
    ) {
        let source = format!(
            "def target(value={value}): # {comment}{ending}    return value{ending}{ending}result = target(){ending}"
        );
        let expected = format!(
            "def target(value): # {comment}{ending}    return value{ending}{ending}result = target(value={value}){ending}"
        );
        let rewritten = fixed(&source).map_err(TestCaseError::fail)?;
        prop_assert_eq!(&rewritten, &expected);
        prop_assert_eq!(fixed(&rewritten).map_err(TestCaseError::fail)?, expected);
    }
}
