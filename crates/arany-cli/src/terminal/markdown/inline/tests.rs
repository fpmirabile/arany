use super::*;
use pulldown_cmark::{Options, Parser};

#[test]
fn escaped_backslash_runs_keep_exact_output_and_origins_with_linear_scan_work() {
    for (tail, shown, origins) in [
        ("", "", &[][..]),
        ("&amp;", "&", &[(0, 5)][..]),
        ("\\*", "*", &[(0, 2)][..]),
        (
            "\\&amp;",
            "&amp;",
            &[(0, 2), (2, 3), (3, 4), (4, 5), (5, 6)][..],
        ),
    ] {
        let pairs = (MAX_MARKDOWN_PARSE_BYTES - tail.len()) / 2;
        let source = format!("{}{tail}", "\\".repeat(pairs * 2));
        let expected = format!("{}{shown}", "\\".repeat(pairs));
        let mut flow = Flow::default();
        let (mut previous_end, mut text_events) = (0, 0);
        for (event, range) in Parser::new_ext(&source, Options::ENABLE_TABLES).into_offset_iter() {
            let before = flow.scanned_bytes;
            flow.event(&source, &event, range.clone()).unwrap();
            if matches!(event, Event::Text(_)) {
                assert!(flow.scanned_bytes - before <= range.start - previous_end);
                previous_end = range.end;
                text_events += 1;
            }
        }
        assert!(text_events >= pairs);
        assert_eq!(
            flow.scanned_bytes,
            pairs + usize::from(tail.starts_with('\\'))
        );
        assert!(flow.scanned_bytes <= source.len());
        let cells = flow.take();
        assert_eq!(
            cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            expected
        );
        for (index, cell) in cells[..pairs].iter().enumerate() {
            assert_eq!(cell.byte, index * 2 + 1);
            assert_eq!(cell.source, index * 2..index * 2 + 2);
        }
        assert_eq!(
            cells[pairs..]
                .iter()
                .map(|cell| (cell.source.start - pairs * 2, cell.source.end - pairs * 2))
                .collect::<Vec<_>>(),
            origins
        );
        let ((rows, _, _), cue) = super::super::project(&source, 80, true);
        assert!(cue.is_none());
        assert_eq!(
            rows.iter()
                .flat_map(|row| &row.cells)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            expected
        );
    }
}
