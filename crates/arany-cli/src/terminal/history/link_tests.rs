use super::inline_tests::{assert_consumers_agree, body, copied, select, styled};
use super::tests::{run, session};
use super::*;
use crate::presentation::{Output, render_exec};
use crate::terminal::markdown;
use ratatui::style::Modifier;

#[test]
fn direct_links_are_inert_styled_projections_with_shared_counts_and_source_copy() {
    for (source, expected) in [
        (
            "[label](https://example.test/path)",
            "label (https://example.test/path)",
        ),
        ("[same](same)", "same (same)"),
        ("<https://example.test/path>", "https://example.test/path"),
        ("<reader@example.test>", "reader@example.test"),
        ("[label](../path)", "label (../path)"),
        ("[label](file:///path)", "label (file:///path)"),
        ("[label](custom:inert)", "label (custom:inert)"),
        ("[label](path(one(two)))", "label (path(one(two)))"),
        (
            "[label](<relative path> \"hidden title\")",
            "label (relative path)",
        ),
        ("[label](target 'hidden title')", "label (target)"),
        ("[label](target (hidden title))", "label (target)"),
        ("[label]()", "label ()"),
        ("[](target)", " (target)"),
        ("[]()", " ()"),
        ("[a `](fake)` label](real)", "a ](fake) label (real)"),
        ("[line\nlabel](target)", "line\nlabel (target)"),
        ("[中 e\u{301} 👩‍💻](target)", "中 e\u{301} 👩‍💻 (target)"),
        ("[&amp; \\*label\\*](target)", "& *label* (target)"),
        ("[![alt](image.png)](target)", "![alt](image.png) (target)"),
    ] {
        assert_consumers_agree(source);
        let ((projected, _, _), cue) = markdown::project(source, 1, true);
        assert!(cue.is_none());
        let cells: Vec<_> = projected.iter().flat_map(|row| &row.cells).collect();
        assert!(cells.iter().all(|cell| !cell.source.is_empty()));
        assert!(
            cells
                .windows(2)
                .all(|pair| pair[0].source.end <= pair[1].source.start)
        );
        for kind in [
            MessageKind::Assistant,
            MessageKind::AgentResult,
            MessageKind::Preview,
            MessageKind::Incomplete,
        ] {
            let rows = display_rows(kind, source, 120, None);
            let shown = rows[1..rows.len() - 1]
                .iter()
                .map(|row| row.text.strip_prefix("  ").unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(shown, expected, "{source:?}");
            let underlined = rows
                .iter()
                .map(|row| styled(row, Modifier::UNDERLINED))
                .collect::<String>();
            assert_eq!(underlined, expected.replace('\n', ""));
        }
        let mut view = session();
        view.runs.push(run("objective", source));
        assert_eq!(
            render_exec(&view.runs[0], &[], Output::Text, false),
            format!("Answer:\n  {}\n", source.replace('\n', "\n  "))
        );
    }
}

#[test]
fn link_labels_keep_nested_styles_in_prose_setext_and_tables() {
    let markup = "**[a *label*](target)** plain";
    for source in [
        markup.to_owned(),
        format!("{markup}\n===\n"),
        format!("| h |\n| - |\n| {markup} |"),
    ] {
        assert_consumers_agree(&source);
        let rows = display_rows(MessageKind::Assistant, &source, 120, None);
        let row = rows.iter().find(|row| row.text.contains("label")).unwrap();
        assert_eq!(styled(row, Modifier::UNDERLINED), "a label (target)");
        assert_eq!(styled(row, Modifier::ITALIC), "label");
        assert!(styled(row, Modifier::BOLD).starts_with("a label (target)"));
    }
}

#[test]
fn wrapped_destinations_keep_partial_selection_reading_and_find_anchors() {
    let source = "[label](https://example.test/alpha/beta/gamma/delta/needle/omega) tail";
    for agent in [false, true] {
        let mut view = session();
        view.runs.push(run("objective", source));
        let mut history = History {
            agent,
            ..History::default()
        };
        history.sync(&view, 16, 100);
        let rows = message_rows(MessageKind::Assistant, source, 16);
        let byte = source.find("gamma").unwrap();
        let offset = row_of_byte(&rows, byte);
        let cells = rows[offset].cells.as_ref().unwrap();
        let index = cells.iter().position(|cell| cell.byte == byte).unwrap();
        let column = 2 + cells[..index]
            .iter()
            .map(|cell| cell.text.width())
            .sum::<usize>();
        assert!(offset > 2 && rows[offset].source_byte > source.find("https").unwrap());
        select(&mut history, &view, offset, column, 1);
        history.extend_selection(&view, offset, column + 4);
        assert_eq!(copied(&history, &view), "gamma");
        for width in [40, 50, 80, 120, 16] {
            history.sync(&view, width, 100);
            assert_eq!(copied(&history, &view), "gamma");
        }
        history.clear_selection();
        select(&mut history, &view, offset, column, 2);
        assert_eq!(
            copied(&history, &view),
            "https://example.test/alpha/beta/gamma/delta/needle/omega"
        );
        history.clear_selection();
        history.sync(&view, 16, 2);
        let anchor = rows[offset].source_byte;
        for width in [40, 16, 80, 50, 120, 16] {
            history.sync(&view, 16, 2);
            history.top = history.messages[1].start + offset;
            history.follow_tail = false;
            history.sync(&view, width, 2);
            let reflowed = message_rows(MessageKind::Assistant, source, width);
            assert_eq!(
                history.top,
                history.messages[1].start + row_of_byte(&reflowed, anchor)
            );
            assert!(!history.follow_tail);
            let top = history.top;
            view.runs.push(run("append", "tail"));
            history.sync(&view, width, 2);
            assert_eq!(history.top, top);
        }
        assert!(history.search(&view, "needle"));
        for width in [16, 40, 50, 80, 120, 16] {
            history.sync(&view, width, 100);
            let rows = message_rows(MessageKind::Assistant, source, width);
            let matched = row_of_byte(&rows, source.find("needle").unwrap());
            assert!(
                rows[matched]
                    .cells
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|cell| cell.byte == source.find("needle").unwrap() && cell.text == "n")
            );
            assert_eq!(
                history.matched_row,
                Some(history.messages[1].start + matched)
            );
            let expected = &display_rows(MessageKind::Assistant, source, width, None)[matched].text;
            assert!(
                history
                    .visible(&view)
                    .iter()
                    .any(|row| row.matched && &row.text == expected)
            );
        }
        history.sync(&view, 16, 100);
        select(&mut history, &view, 1, 2, 1);
        let end = source.find(") tail").unwrap();
        let rows = message_rows(MessageKind::Assistant, source, 16);
        let offset = row_of_byte(&rows, end);
        let cells = rows[offset].cells.as_ref().unwrap();
        let index = cells
            .iter()
            .position(|cell| cell.source.contains(&end))
            .unwrap();
        let column = 2 + cells[..index]
            .iter()
            .map(|cell| cell.text.width())
            .sum::<usize>();
        history.extend_selection(&view, offset, column);
        assert_eq!(copied(&history, &view), &source[..=end]);
    }
}

#[test]
fn collapsed_link_and_image_suffixes_keep_literal_text_and_source_origins() {
    for (markup, expected) in [
        ("[reference][]", "[reference][]"),
        ("[reference][][]", "[reference][][]"),
        ("[reference] []", "[reference] []"),
        ("![reference][]", "![reference][]"),
        ("![reference][][]", "![reference][][]"),
        ("![reference] []", "![reference] []"),
        ("**[reference][]**", "[reference][]"),
        ("*![reference][]*", "![reference][]"),
        ("[![reference][]](target)", "![reference][] (target)"),
    ] {
        let source = format!("{markup}\n\n[reference]: https://example.test/target");
        assert_consumers_agree(&source);
        for kind in [
            MessageKind::Assistant,
            MessageKind::AgentResult,
            MessageKind::Preview,
            MessageKind::Incomplete,
        ] {
            let rows = display_rows(kind, &source, 120, None);
            assert_eq!(rows[1].text, format!("  {expected}"), "{markup}");
        }
        if let Some(suffix) = markup.find("][]").map(|end| end + 1) {
            let ((rows, _, _), cue) = markdown::project(&source, 1, true);
            assert!(cue.is_none());
            for (index, text) in [(suffix, "["), (suffix + 1, "]")] {
                assert!(
                    rows.iter().flat_map(|row| &row.cells).any(|cell| {
                        cell.byte == index && cell.source.contains(&index) && cell.text == text
                    }),
                    "{markup}: missing suffix byte {index}"
                );
            }
        }
    }
}

#[test]
fn hostile_destinations_stay_escaped_and_decoded_and_reference_links_stay_literal() {
    for source in [
        "[label](<https://example.test/\u{1b}]52;c;payload\u{7}\u{202e}\u{0085}>)",
        "[label](<https://example.test/&#27;]52;c;payload&#7;&#x202e;>)",
        "[label](https://example.test/a&amp;b)",
        r"[label](https://example.test/a\(b\))",
        "[label](<target\rcarriage>)",
        "[label][reference]\n\n[reference]: https://example.test/target",
        "[reference][] [reference]\n\n[reference]: https://example.test/target",
        "![image](https://example.test/image.png) <b>html</b>",
    ] {
        assert_consumers_agree(source);
        for width in [0, 1, 2, 3, 16, 40, 50, 80, 120] {
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            assert_eq!(
                markdown_cost(MessageKind::Assistant, source, width),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
            assert!(rows.iter().all(|row| row.text.width() <= width
                && !row.text.chars().any(|c| c.is_control() || c == '\u{202e}')));
        }
        if source.contains("&amp;")
            || source.contains("&#27;")
            || source.contains("\\(")
            || source.contains("[reference]")
            || source.starts_with("![")
        {
            assert_eq!(
                body(source, 120)
                    .iter()
                    .map(|row| row.strip_prefix("  ").unwrap())
                    .collect::<Vec<_>>()
                    .join("\n"),
                source
            );
        }
    }
    let source = "[label](<target\u{1b}\u{7}\u{202e}>)";
    assert_eq!(
        body(source, 120),
        [r"  label (target\u{001b}\u{0007}\u{202e})"]
    );
    let ((rows, _, _), cue) = markdown::project(source, 1, true);
    assert!(cue.is_none());
    for pair in rows
        .iter()
        .flat_map(|row| &row.cells)
        .collect::<Vec<_>>()
        .windows(2)
    {
        assert!(pair[0].source.end <= pair[1].source.start);
    }
}
