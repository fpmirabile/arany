use super::inline_tests::{assert_consumers_agree, body, copied, select, styled};
use super::tests::{run, session};
use super::*;
use crate::presentation::{Output, render_exec};
use crate::terminal::markdown;
use ratatui::style::Modifier;

#[test]
fn decoded_fence_labels_share_consumers_counts_styles_and_source_copy() {
    for (source, expected) in [
        ("```rust&amp;lang\nbody\n```", "rust&lang"),
        ("  ~~~~  \\*label\\* &copy;  \nbody\n  ~~~~", "*label* ©"),
        ("~~~&fjlig;\n~~~", "fj"),
        ("~~~&#32;label&#32;\n~~~", " label "),
        ("~~~label &unknown; \\q\n~~~", "label &unknown; \\q"),
        ("~~~中 e\u{301} 👩‍💻 &amp;\nbody\n~~~", "中 e\u{301} 👩‍💻 &"),
        (
            "~~~[label](target) **bold** <b>\nbody\n~~~",
            "[label](target) **bold** <b>",
        ),
        ("~~~rust&amp;lang\r\nbody\r\n~~~", "rust&lang"),
        ("~~~rust&amp;lang", "rust&lang"),
    ] {
        assert_consumers_agree(source);
        for kind in [
            MessageKind::Assistant,
            MessageKind::AgentResult,
            MessageKind::Preview,
            MessageKind::Incomplete,
        ] {
            let rows = display_rows(kind, source, 120, None);
            assert_eq!(rows[1].text, format!("  {expected}"));
            assert_eq!(styled(&rows[1], Modifier::BOLD), expected);
            assert!(styled(&rows[1], Modifier::REVERSED).is_empty());
            assert!(styled(&rows[1], Modifier::UNDERLINED).is_empty());
        }
        let mut view = session();
        view.runs.push(run("objective", source));
        assert_eq!(
            render_exec(&view.runs[0], &[], Output::Text, false),
            format!(
                "Answer:\n  {}\n",
                display_text(source).replace('\n', "\n  ")
            )
        );
    }
    assert_eq!(
        body("~~~alpha beta gamma\n~~~", 10),
        ["  alpha be", "  ta gamma"]
    );
}

fn position(source: &str, width: usize, byte: usize) -> (usize, usize) {
    let rows = message_rows(MessageKind::Assistant, source, width);
    let row = row_of_byte(&rows, byte);
    let cells = rows[row].cells.as_ref().unwrap();
    let index = cells
        .iter()
        .position(|cell| cell.source.contains(&byte))
        .unwrap();
    let column = 2 + cells[..index]
        .iter()
        .map(|cell| cell.text.width())
        .sum::<usize>();
    (row, column)
}

#[test]
fn fence_label_fragments_keep_partial_word_find_and_reading_anchors_on_reflow() {
    let source = "~~~alpha beta &amp; \\*gamma\\* &fjlig; omega\nbody\n~~~";
    for agent in [false, true] {
        let mut view = session();
        view.runs.push(run("objective", source));
        let mut history = History {
            agent,
            ..History::default()
        };
        for (query, expected) in [
            ("beta", "beta"),
            ("amp;", "&amp;"),
            ("gamma", "gamma"),
            ("fjlig", "&fjlig;"),
        ] {
            history.sync(&view, 3, 100);
            let (row, column) = position(source, 3, source.find(query).unwrap());
            select(&mut history, &view, row, column, 2);
            assert_eq!(copied(&history, &view), expected);
            for width in [16, 40, 50, 80, 120, 3] {
                history.sync(&view, width, 100);
                assert_eq!(copied(&history, &view), expected);
            }
            history.clear_selection();
        }
        let byte = source.find("beta").unwrap();
        let (row, column) = position(source, 3, byte);
        select(&mut history, &view, row, column, 1);
        let (end, column) = position(source, 3, byte + 2);
        history.extend_selection(&view, end, column);
        assert_eq!(copied(&history, &view), "bet");
        history.sync(&view, 80, 100);
        assert_eq!(copied(&history, &view), "bet");
        history.clear_selection();
        assert!(history.search(&view, "amp;"));
        for width in [3, 16, 40, 50, 80, 120, 3] {
            history.sync(&view, width, 100);
            let rows = message_rows(MessageKind::Assistant, source, width);
            let matched = row_of_byte(&rows, source.find("amp;").unwrap());
            assert_eq!(
                history.matched_row,
                Some(history.messages[1].start + matched)
            );
            assert!(
                history
                    .visible(&view)
                    .iter()
                    .any(|row| row.matched && row.text.contains('&'))
            );
        }
        let rows = message_rows(MessageKind::Assistant, source, 3);
        let coordinate = row_coordinate(&rows[row]);
        for width in [16, 40, 50, 80, 120, 3] {
            history.sync(&view, 3, 2);
            history.top = history.messages[1].start + row;
            history.follow_tail = false;
            history.sync(&view, width, 2);
            let rows = message_rows(MessageKind::Assistant, source, width);
            assert_eq!(
                history.top,
                history.messages[1].start + row_of_coordinate(&rows, coordinate)
            );
            assert!(!history.follow_tail);
            view.runs.push(run("append", "tail"));
            let top = history.top;
            history.sync(&view, width, 2);
            assert_eq!(history.top, top);
        }
    }
}

#[test]
fn atomic_fence_label_entities_keep_distinct_cells_and_original_envelopes() {
    for source in ["~~~x &fjlig; y\nbody\n~~~", "~~~&fjlig;\n~~~"] {
        let ((rows, _, _), cue) = markdown::project(source, 1, true);
        assert!(cue.is_none());
        let cells: Vec<_> = rows.iter().flat_map(|row| &row.cells).collect();
        assert!(
            cells
                .iter()
                .all(|cell| !cell.source.is_empty() && cell.source.end <= source.len())
        );
        assert!(
            cells
                .windows(2)
                .all(|pair| pair[0].source.end <= pair[1].source.start
                    || pair[0].source == pair[1].source)
        );
        let entity = source.find("&fjlig;").unwrap();
        let group: Vec<_> = cells.iter().filter(|cell| cell.byte == entity).collect();
        assert_eq!(group.len(), 2);
        assert_eq!(group[0].source, group[1].source);
        assert_ne!(group[0].coordinate, group[1].coordinate);
        let mut view = session();
        view.runs.push(run("objective", source));
        let mut history = History::default();
        history.sync(&view, 3, 100);
        let (first, column) = position(source, 3, entity);
        for row in [first, first + 1] {
            select(&mut history, &view, row, column, 2);
            assert_eq!(
                copied(&history, &view),
                if source.starts_with("~~~x") {
                    "&fjlig;"
                } else {
                    source
                }
            );
            history.sync(&view, 80, 100);
            assert_eq!(
                copied(&history, &view),
                if source.starts_with("~~~x") {
                    "&fjlig;"
                } else {
                    source
                }
            );
            history.sync(&view, 3, 100);
        }
    }
}

#[test]
fn hostile_fence_labels_are_sanitized_after_decoding_at_every_width() {
    for (label, expected) in [
        (
            "&#27;]52;c;payload&#7;&#x202e;&#x85;&#13;",
            r"\u{001b}]52;c;payload\u{0007}\u{202e}\u{0085}\u{000d}",
        ),
        (
            "\u{1b}]8;;target\u{7}\u{202e}\u{0085}",
            r"\u{001b}]8;;target\u{0007}\u{202e}\u{0085}",
        ),
        (r"\x1b &amp;#27;", r"\x1b &#27;"),
    ] {
        let source = format!("~~~{label}\nbody\n~~~");
        assert_consumers_agree(&source);
        assert_eq!(body(&source, 120)[0], format!("  {expected}"));
        for width in [0, 1, 2, 3, 16, 40, 50, 80, 120] {
            for kind in [
                MessageKind::Assistant,
                MessageKind::AgentResult,
                MessageKind::Preview,
                MessageKind::Incomplete,
            ] {
                let rows = display_rows(kind, &source, width, None);
                assert_eq!(
                    markdown_cost(kind, &source, width),
                    (rows.iter().map(|row| row.text.len()).sum(), rows.len())
                );
                assert!(rows.iter().all(|row| row.text.width() <= width
                    && !row.text.chars().any(|c| c.is_control() || c == '\u{202e}')));
            }
        }
    }
}

#[test]
fn fence_labels_at_the_row_boundary_fall_back_whole_only_when_over() {
    for extra in [0, 1] {
        let source = format!(
            "~~~{}\n~~~",
            "a".repeat(markdown::MAX_MARKDOWN_ROWS - 2 + extra)
        );
        let ((rows, bytes, count), cue) = markdown::project(&source, 1, true);
        let ((_, counted_bytes, counted_rows), counted_cue) = markdown::project(&source, 1, false);
        assert_eq!(
            (bytes, count, cue),
            (counted_bytes, counted_rows, counted_cue)
        );
        assert_eq!(cue, (extra > 0).then_some(markdown::LIMIT_CUE));
        if extra == 0 {
            assert_eq!(rows.len(), markdown::MAX_MARKDOWN_ROWS - 2);
        } else {
            let (literal, _, _) = markdown::literal(&source, 1, true);
            assert_eq!(
                rows.iter()
                    .flat_map(|row| &row.cells)
                    .map(|cell| cell.text.as_str())
                    .collect::<String>(),
                literal
                    .iter()
                    .flat_map(|row| &row.cells)
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            );
        }
        for kind in [MessageKind::Assistant, MessageKind::AgentResult] {
            let drawn = display_rows(kind, &source, 3, None);
            assert_eq!(
                markdown_cost(kind, &source, 3),
                (drawn.iter().map(|row| row.text.len()).sum(), drawn.len())
            );
        }
    }
}
