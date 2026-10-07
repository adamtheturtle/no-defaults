use super::{line_break_end, next_line_break, previous_line_start, source_line_starts};

#[test]
fn line_starts_use_byte_offsets_for_each_line_ending() {
    for (source, expected) in [
        ("", vec![0]),
        ("abc", vec![0]),
        ("\r\n", vec![0, 2]),
        ("a\r\nb\r\nc", vec![0, 3, 6]),
        ("\r\n\r\n", vec![0, 2, 4]),
        ("a\nb\rc\r\nd", vec![0, 2, 4, 7]),
        ("é\r\nx", vec![0, 4]),
    ] {
        assert_eq!(source_line_starts(source), expected, "{source:?}");
    }
}

#[test]
fn previous_start_follows_the_last_line_break_before_the_offset() {
    for (source, offset, expected) in [
        ("", 0, 0),
        ("abc", 3, 0),
        ("a\nb", 1, 0),
        ("a\nb", 2, 2),
        ("a\nb", 3, 2),
        ("a\rb", 3, 2),
        ("a\r\nb", 4, 3),
        ("é\r\nx", 5, 4),
    ] {
        assert_eq!(
            previous_line_start(source, offset),
            expected,
            "{source:?} at {offset}"
        );
    }
}

#[test]
fn next_break_is_relative_to_the_given_byte_offset() {
    for (source, offset, expected) in [
        ("", 0, 0),
        ("abc", 0, 3),
        ("abc", 3, 3),
        ("ab\nc", 1, 2),
        ("ab\nc", 2, 2),
        ("ab\nc", 3, 4),
        ("ab\rc", 1, 2),
        ("é\r\nx", 2, 2),
    ] {
        assert_eq!(
            next_line_break(source, offset),
            expected,
            "{source:?} at {offset}"
        );
    }
}

#[test]
fn break_end_consumes_the_complete_line_ending() {
    for (source, start, expected) in [
        ("", 0, 0),
        ("abc", 1, 1),
        ("abc", 3, 3),
        ("\r\n", 0, 2),
        ("a\r\nb", 1, 3),
        ("a\rb", 1, 2),
        ("a\nb", 1, 2),
        ("é\r\nx", 2, 4),
    ] {
        assert_eq!(
            line_break_end(source, start),
            expected,
            "{source:?} at {start}"
        );
    }
}
