use super::inline_tests::{assert_consumers_agree, body, copied, select, styled};
use super::tests::{run, session};
use super::*;
use crate::terminal::markdown::{self, Fragment};

#[test]
fn reference_and_decoded_links_share_inert_consumers_counts_and_original_copy() {
    for (source, expected) in [
        ("[label](a&amp;b)", "label (a&b)"),
        (r"[label](a\(b\))", "label (a(b))"),
        ("[label](a&fjlig;b)", "label (afjb)"),
        ("[label](a&acE;b)", "label (a∾̳b)"),
        ("[label](e&#x301;end)", "label (e\u{301}end)"),
        ("[label](&amp;\u{301}end)", "label (&\u{301}end)"),
        (r"[label](<a\>b>)", "label (a>b)"),
        (
            "[escaped\\]:label]\n\n[escaped\\]:label]: target",
            "escaped]:label (target)\n",
        ),
        (
            "[label][reference]\n\n[reference]: target \"hidden title\"",
            "label (target)\n",
        ),
        (
            "[reference][] [reference]\n\n[reference]: target",
            "reference (target) reference (target)\n",
        ),
        ("[LaBeL]\n\n[label]: a&amp;b", "LaBeL (a&b)\n"),
        ("[label][missing]", "[label][missing]"),
        ("[r]: target\n\n[r]", "\nr (target)"),
    ] {
        assert_consumers_agree(source);
        for kind in [
            MessageKind::Assistant,
            MessageKind::AgentResult,
            MessageKind::Preview,
            MessageKind::Incomplete,
        ] {
            let rows = display_rows(kind, source, 120, None);
            assert_eq!(
                rows[1..rows.len() - 1]
                    .iter()
                    .map(|row| row.text.strip_prefix("  ").unwrap())
                    .collect::<Vec<_>>()
                    .join("\n"),
                expected
            );
        }
    }
    for source in [
        "**[label](a&amp;b)**",
        "[label](a&amp;b)\n===",
        "| h |\n| - |\n| [label](a&amp;b) |",
        "| h |\n| - |\n| [label][r] |\n\n[r]: a&amp;b",
    ] {
        assert_consumers_agree(source);
        let rows = display_rows(MessageKind::Assistant, source, 120, None);
        let row = rows.iter().find(|row| row.text.contains("label")).unwrap();
        assert_eq!(
            styled(row, ratatui::style::Modifier::UNDERLINED),
            "label (a&b)"
        );
    }
}

fn destination(rows: &[MessageRow], site: usize, offset: usize) -> (usize, usize, Coordinate) {
    for (row, rendered) in rows.iter().enumerate() {
        let mut column = 2;
        for cell in rendered.cells.iter().flatten() {
            if cell.coordinate.site == site
                && cell.coordinate.fragment == Fragment::Destination
                && cell.coordinate.offset == offset
            {
                return (row, column, cell.coordinate);
            }
            column += cell.text.width();
        }
    }
    panic!("destination cell absent");
}

#[test]
fn repeated_destinations_keep_distinct_selection_reading_and_match_coordinates() {
    let target = format!(
        "https://example.test/{}/needle/omega",
        "segment/".repeat(12)
    );
    let source = format!("[x][r]\n\n[x][r]\n\n[r]: {target}");
    let second = source.match_indices("[x][r]").nth(1).unwrap().0;
    let needle = target.find("needle").unwrap();
    for agent in [false, true] {
        let mut view = session();
        view.runs.push(run("objective", &source));
        let mut history = History {
            agent,
            ..History::default()
        };
        history.sync(&view, 16, 100);
        let rows = message_rows(MessageKind::Assistant, &source, 16);
        let (row, column, key) = destination(&rows, second, needle);
        let (end_row, end_column, _) = destination(&rows, second, needle + 5);
        select(&mut history, &view, row, column, 1);
        history.extend_selection(&view, end_row, end_column);
        assert_eq!(history.selection.unwrap().anchor.coordinate, key);
        let expected = &source[second..];
        assert_eq!(copied(&history, &view), expected);
        for width in [40, 50, 80, 120, 16] {
            history.sync(&view, width, 100);
            assert_eq!(copied(&history, &view), expected);
            assert_eq!(history.selection.unwrap().anchor.coordinate, key);
            let visible = history.visible(&view);
            let selected: String = visible
                .iter()
                .filter_map(|row| {
                    row.selected.map(|(first, last)| {
                        row.text
                            .chars()
                            .skip(first)
                            .take(last - first)
                            .collect::<String>()
                    })
                })
                .collect();
            assert_eq!(selected, "needle");
        }
        history.clear_selection();
        for width in [40, 50, 80, 120, 16] {
            history.sync(&view, 16, 2);
            history.top = history.messages[1].start + row;
            history.follow_tail = false;
            let anchor = row_coordinate(&rows[row]);
            history.sync(&view, width, 2);
            let reflowed = message_rows(MessageKind::Assistant, &source, width);
            assert_eq!(
                history.top,
                history.messages[1].start + row_of_coordinate(&reflowed, anchor)
            );
            view.runs.push(run("append", "tail"));
            let top = history.top;
            history.sync(&view, width, 2);
            assert_eq!(history.top, top);
        }
        assert!(history.search(&view, "needle"));
        assert_eq!(history.matched_source.unwrap().1.site, 0);
        history.last_find = Some("needle".into());
        assert!(history.step_match(&view, false));
        assert_eq!(history.matched_source.unwrap().1, key);
        for width in [16, 40, 50, 80, 120] {
            history.sync(&view, width, 100);
            let rows = message_rows(MessageKind::Assistant, &source, width);
            assert_eq!(
                history.matched_row,
                Some(history.messages[1].start + row_of_coordinate(&rows, key))
            );
        }
        assert!(!history.step_match(&view, false));
        assert!(history.step_match(&view, true));
        assert_eq!(history.matched_source.unwrap().1.site, 0);
    }
}

#[test]
fn a_find_steps_between_repeated_uses_even_on_one_row() {
    let source = "[x][r] [x][r]\n\n[r]: target \"hidden title\"";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 120, 100);
    assert!(history.search(&view, "target"));
    let row = history.matched_row;
    history.last_find = Some("target".into());
    assert!(history.step_match(&view, false));
    assert_eq!(history.matched_row, row);
    assert_eq!(history.matched_source.unwrap().1.site, 7);
    assert!(history.step_match(&view, true));
    assert_eq!(history.matched_source.unwrap().1.site, 0);
    assert!(history.search(&view, "hidden"));
    history.last_find = Some("hidden".into());
    assert_eq!(history.matched_source.unwrap().1.site, 0);
    assert!(history.step_match(&view, false));
    assert_eq!(history.matched_source.unwrap().1.site, 7);
}

#[test]
fn aliases_preserve_table_code_and_heading_source_selection() {
    let source = "[r]: target\n\n# Heading\n\n~~~\ncode\n~~~\n\n| h |\n| - |\n| [x][r] |";
    assert_consumers_agree(source);
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    for width in [16, 40, 50, 80, 120] {
        history.sync(&view, width, 100);
        let rows = message_rows(MessageKind::Assistant, source, width);
        for word in ["Heading", "code"] {
            let row = rows.iter().position(|row| row.text.contains(word)).unwrap();
            select(&mut history, &view, row, 2, 2);
            assert_eq!(copied(&history, &view), word);
        }
        let site = source.find("[x][r]").unwrap();
        let (row, column, _) = destination(&rows, site, 2);
        select(&mut history, &view, row, column, 1);
        history.extend_selection(&view, row, column + 1);
        let copy = copied(&history, &view);
        assert!(copy.contains("Heading") && copy.contains("code") && copy.ends_with("[x][r]"));
    }
}

#[test]
fn hostile_reference_and_decoded_destinations_are_escaped_after_decoding() {
    for source in [
        "[x](<target&#27;]52;c;payload&#7;&#x202e;&#x85;>)",
        "[x][r]\n\n[r]: <target&#27;]52;c;payload&#7;&#x202e;&#x85;>",
        "[x][r]\n\n[r]: <target\u{1b}]52;c;payload\u{7}\u{202e}\u{85}>",
        "[x](<target\rcarriage>)",
    ] {
        assert_consumers_agree(source);
        for width in [0, 1, 2, 3, 16, 120] {
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            assert!(
                rows.iter()
                    .all(|row| !row.text.chars().any(|c| c.is_control() || c == '\u{202e}'))
            );
            assert_eq!(
                markdown_cost(MessageKind::Assistant, source, width),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
        }
    }
    assert_eq!(
        body("[x](target&#27;&#7;&#x202e;)", 120),
        [r"  x (target\u{001b}\u{0007}\u{202e})"]
    );
}

#[test]
fn repeated_destinations_enforce_the_output_boundary_and_whole_message_fallback() {
    let uses = 128;
    let destination_bytes = markdown::MAX_MARKDOWN_OUTPUT_BYTES / uses - 7;
    // Escaping reaches the renderer's byte cap before the parser's reference fuel runs out.
    let control_bytes = escaped_terminal_bytes("\u{1b}");
    let destination = format!(
        "{}{}",
        "&#27;".repeat(destination_bytes / control_bytes),
        "a".repeat(destination_bytes % control_bytes)
    );
    let padding =
        markdown::MAX_MARKDOWN_OUTPUT_BYTES - uses * (destination_bytes + 4) - (uses + 2) * 2;
    for extra in [0, 1] {
        let source = format!(
            "{} {}\n\n[r]: {destination}",
            "[x][r]\n".repeat(uses).trim_end(),
            "z".repeat(padding - 1 + extra)
        );
        assert!(source.len() < markdown::MAX_MARKDOWN_PARSE_BYTES);
        for capture in [false, true] {
            let ((_, bytes, rows), cue) = markdown::project(&source, MAX_ROW_CELLS, capture);
            if extra == 0 {
                assert!(cue.is_none());
                assert_eq!(bytes + rows * 2, markdown::MAX_MARKDOWN_OUTPUT_BYTES);
            } else {
                assert_eq!(cue, Some(markdown::LIMIT_CUE));
                let (_, literal_bytes, literal_rows) =
                    markdown::literal(&source, MAX_ROW_CELLS, false);
                assert_eq!((bytes, rows), (literal_bytes, literal_rows));
            }
        }
    }
    let source = format!(
        "{}\n\n[r]: {destination}",
        "[x][r]\n".repeat(uses + 1).trim_end()
    );
    for capture in [false, true] {
        let ((_, bytes, rows), cue) = markdown::project(&source, MAX_ROW_CELLS, capture);
        assert_eq!(cue, Some(markdown::LIMIT_CUE));
        let (_, literal_bytes, literal_rows) = markdown::literal(&source, MAX_ROW_CELLS, false);
        assert_eq!((bytes, rows), (literal_bytes, literal_rows));
    }
    let uses = 4;
    let destination = "a".repeat(markdown::MAX_MARKDOWN_ROWS / uses - 5);
    for extra in [0, 1] {
        let source = format!(
            "{}{}\n\n[r]: {destination}",
            "z".repeat(1 + extra),
            "[x][r]\n".repeat(uses).trim_end()
        );
        for capture in [false, true] {
            let ((_, bytes, rows), cue) = markdown::project(&source, 1, capture);
            if extra == 0 {
                assert!(cue.is_none());
                assert_eq!(rows + 2, markdown::MAX_MARKDOWN_ROWS);
            } else {
                assert_eq!(cue, Some(markdown::LIMIT_CUE));
                let (_, literal_bytes, literal_rows) = markdown::literal(&source, 1, false);
                assert_eq!((bytes, rows), (literal_bytes, literal_rows));
            }
        }
    }
}
