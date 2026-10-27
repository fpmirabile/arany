use super::inline_tests::{assert_consumers_agree, body, copied, select, styled};
use super::tests::{run, session};
use super::*;
use crate::presentation::{Output, render_exec};
use crate::terminal::markdown;
use ratatui::style::Modifier;

const CONTAINERS: &str = "7. **first** &amp; item\n   continuation\n8. second\n   - nested `code`\n\n> quoted *words*\n>\n> - ~~~rust&amp;lang\n>   \tvalue\n>   ~~~\n\n-\n- ~~~\n  ~~~";

#[test]
fn lists_quotes_and_nested_code_share_consumers_counts_and_source_copy() {
    for source in [
        CONTAINERS,
        "- **bold**\n- *italic*\n- `code`",
        "- one\n  next\n- two",
        "- first\n\n  second paragraph\n\n- third",
        "> first\nlazy continuation\n> last",
        "-\n-\n-",
        "7)\n8)",
        ">\n> >\n> > quoted",
        "- > quote\n  > second",
        "> - ~~~\n>   code\n>   ~~~",
        "-     indented\n      second",
        "- - ~~~lang\n    code\n    ~~~",
        "- # Heading\n- title\n  ===",
        "> one\r\n>\r\n> two",
        "- [label][ref]\n- [label][ref]\n\n[ref]: https://e.test/a&amp;b",
        "- [ ] literal task\n\n> ~~literal~~\n>\n> ---",
        "- first\n  ***",
        "- first\n  <div>\n  contents\n  </div>",
    ] {
        assert_consumers_agree(source);
        for width in [16, 40, 50, 80, 120] {
            let expected = body(source, width);
            for kind in [
                MessageKind::Assistant,
                MessageKind::AgentResult,
                MessageKind::Preview,
                MessageKind::Incomplete,
            ] {
                let rows = display_rows(kind, source, width, None);
                assert_eq!(
                    rows[1..rows.len() - 1]
                        .iter()
                        .map(|row| row.text.clone())
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(
                    markdown_cost(kind, source, width),
                    (rows.iter().map(|row| row.text.len()).sum(), rows.len())
                );
            }
        }
        let answer = run("objective", source);
        assert_eq!(
            render_exec(&answer, &[], Output::Text, false),
            format!("Answer:\n  {}\n", display_text(source).replace('\n', "\n  "))
        );
    }
    assert_eq!(
        body("7) alpha\n99) beta\n\n> - quoted", 80)
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        ["  7. alpha", "  8. beta", "", "  │ • quoted"]
    );
    assert_eq!(body("-\n-\n-", 80), ["  • ", "  • ", "  • "]);
    assert_eq!(body("> - ~~~\n>   code\n>   ~~~", 80), ["  │ • code"]);
    assert_eq!(body("- ~~~\n  ~~~", 80), ["  • "]);
    assert_eq!(body("- first\n  ***", 80), ["  • first", "    ***"]);
    let rows = display_rows(MessageKind::Assistant, "> - **bold** `code`", 80, None);
    assert_eq!(rows[1].text, "  │ • bold code");
    assert_eq!(styled(&rows[1], Modifier::BOLD), "bold");
    assert_eq!(styled(&rows[1], Modifier::REVERSED), "code");
}

#[test]
fn hanging_indents_tab_columns_and_tiny_widths_keep_content() {
    for (source, expected) in [
        (
            "- alpha beta gamma delta",
            vec!["  • alpha beta ", "    gamma delta"],
        ),
        (
            "> alpha beta gamma delta",
            vec!["  │ alpha beta ", "  │ gamma delta"],
        ),
        ("-\tfoo\n\tbar", vec!["  • foo", "    bar"]),
        (">\t-\tfoo\n>\t\tbar", vec!["  │ • foo", "  │   bar"]),
        ("-\t\tfoo", vec!["  •   foo"]),
        ("> ~~~\n> \tvalue\n> ~~~", vec!["  │     value"]),
    ] {
        assert_eq!(body(source, 16), expected, "{source:?}");
    }
    for source in [CONTAINERS, "> - 中 e\u{301} 👩‍💻 longword", "-\t\tfoo"] {
        for width in [0, 1, 2, 3, 4, 5, 16] {
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            assert!(rows.iter().all(|row| row.text.width() <= width));
            assert_eq!(
                markdown_cost(MessageKind::Assistant, source, width),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
        }
    }
}

fn position(source: &str, width: usize, byte: usize) -> (usize, usize) {
    let rows = message_rows(MessageKind::Assistant, source, width);
    let row = row_of_byte(&rows, byte);
    let cells = rows[row].cells.as_ref().unwrap();
    let index = cells.iter().position(|cell| cell.byte == byte).unwrap();
    let column = 2 + cells[..index]
        .iter()
        .map(|cell| cell.text.width())
        .sum::<usize>();
    (row, column)
}

#[test]
fn container_words_partial_selection_find_and_reading_survive_reflow_and_append() {
    let source = "> - alpha beta gamma delta\n>\n>   ~~~rust&amp;lang\n>   code\n>   ~~~";
    let byte = source.find("beta").unwrap();
    for agent in [false, true] {
        let mut view = session();
        view.runs.push(run("objective", source));
        let mut history = History {
            agent,
            ..History::default()
        };
        history.sync(&view, 16, 100);
        let (row, column) = position(source, 16, byte);
        select(&mut history, &view, row, column, 2);
        assert_eq!(copied(&history, &view), "beta");
        for width in [40, 50, 80, 120, 16] {
            history.sync(&view, width, 100);
            assert_eq!(copied(&history, &view), "beta");
        }
        select(&mut history, &view, row, column, 1);
        let (end, column) = position(source, 16, byte + 2);
        history.extend_selection(&view, end, column);
        assert_eq!(copied(&history, &view), "bet");
        history.clear_selection();
        assert!(history.search(&view, "beta"));
        for width in [16, 40, 50, 80, 120] {
            history.sync(&view, width, 100);
            assert!(
                history
                    .visible(&view)
                    .iter()
                    .any(|row| row.matched && row.text.contains("beta"))
            );
        }
        history.matched_source = None;
        history.matched_row = None;
        let rows = message_rows(MessageKind::Assistant, source, 16);
        let anchor = row_coordinate(&rows[row]);
        for width in [40, 50, 80, 120, 16] {
            history.sync(&view, 16, 2);
            history.top = history.messages[1].start + row;
            history.follow_tail = false;
            history.sync(&view, width, 2);
            let rows = message_rows(MessageKind::Assistant, source, width);
            assert_eq!(
                history.top,
                history.messages[1].start + row_of_coordinate(&rows, anchor)
            );
            let top = history.top;
            view.runs.push(run("append", "tail"));
            history.sync(&view, width, 2);
            assert_eq!(history.top, top);
        }
    }
}

#[test]
fn hostile_nested_text_is_escaped_after_decoding_in_both_consumers() {
    let source = "> - &#27;]52;c;payload&#7; &#x202e;\n>\n>   ~~~&#27;]8;;target&#7;\n>   \u{1b}]52;c;payload\u{7}\u{202e}\n>   ~~~";
    assert_consumers_agree(source);
    assert!(body(source, 120)[0].contains(r"\u{001b}]52;c;payload\u{0007}"));
    for width in [0, 1, 2, 3, 16, 40, 50, 80, 120] {
        for kind in [MessageKind::Assistant, MessageKind::AgentResult] {
            let rows = display_rows(kind, source, width, None);
            assert!(rows.iter().all(|row| !row
                .text
                .chars()
                .any(|c| c.is_control() || c == '\u{202e}')));
            assert_eq!(
                markdown_cost(kind, source, width),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
        }
    }
}

#[test]
fn container_depth_and_generated_rows_fall_back_whole_only_over_the_limit() {
    for extra in [0, 1] {
        for source in [
            format!(
                "{}answer",
                "> ".repeat(markdown::MAX_MARKDOWN_DEPTH - 1 + extra)
            ),
            format!(
                "{}answer",
                "- ".repeat(markdown::MAX_MARKDOWN_DEPTH / 2 + extra)
            ),
        ] {
            assert_consumers_agree(&source);
            let ((_, bytes, rows), cue) = markdown::project(&source, 80, true);
            let ((_, counted_bytes, counted_rows), counted_cue) =
                markdown::project(&source, 80, false);
            assert_eq!((bytes, rows, cue), (counted_bytes, counted_rows, counted_cue));
            assert_eq!(cue, (extra > 0).then_some(markdown::LIMIT_CUE));
        }
        let source = format!("- {}", "a".repeat(markdown::MAX_MARKDOWN_ROWS - 2 + extra));
        let ((drawn, bytes, rows), cue) = markdown::project(&source, 1, true);
        let ((_, counted_bytes, counted_rows), counted_cue) = markdown::project(&source, 1, false);
        assert_eq!((bytes, rows, cue), (counted_bytes, counted_rows, counted_cue));
        assert_eq!(cue, (extra > 0).then_some(markdown::LIMIT_CUE));
        if extra > 0 {
            let (literal, _, _) = markdown::literal(&source, 1, true);
            let text = |rows: Vec<markdown::Row>| {
                rows.into_iter()
                    .flat_map(|row| row.cells)
                    .map(|cell| cell.text)
                    .collect::<String>()
            };
            assert_eq!(text(drawn), text(literal));
        }
    }
}

#[test]
fn repeated_hanging_indents_charge_generated_output_before_whole_message_fallback() {
    let marker = "999999999. ";
    let prefix = marker.repeat(markdown::MAX_MARKDOWN_DEPTH / 2);
    let width = prefix.len() + 1;
    let row_cost = width + 2;
    let at_limit = markdown::MAX_MARKDOWN_OUTPUT_BYTES / row_cost;
    for extra in [0, 1] {
        let source = format!("{prefix}{}", "a".repeat(at_limit + extra));
        let ((_, bytes, rows), cue) = markdown::project(&source, width, true);
        let ((_, counted_bytes, counted_rows), counted_cue) =
            markdown::project(&source, width, false);
        assert_eq!((bytes, rows, cue), (counted_bytes, counted_rows, counted_cue));
        assert_eq!(cue, (extra > 0).then_some(markdown::LIMIT_CUE));
        for kind in [MessageKind::Assistant, MessageKind::AgentResult] {
            let drawn = display_rows(kind, &source, width + 2, None);
            assert_eq!(
                markdown_cost(kind, &source, width + 2),
                (drawn.iter().map(|row| row.text.len()).sum(), drawn.len())
            );
        }
    }
}

#[test]
fn pager_find_steps_container_source_matches_and_reference_uses_through_resize() {
    use crate::terminal::TerminalInput;
    let quoted = "> first\n> second\n> third";
    let rows = message_rows(MessageKind::Assistant, quoted, 16);
    assert_eq!(
        source_matches(quoted, &rows, ">")
            .into_iter()
            .map(|(row, _)| rows[row].text.as_str())
            .collect::<Vec<_>>(),
        ["  │ first", "  │ second", "  │ third"]
    );
    for (source, count) in [
        ("> - needle\n> - needle\n>   ~~~\n>   needle\n>   ~~~", 3),
        ("> - [label][r]\n> - [label][r]\n\n[r]: https://e.test/needle", 2),
    ] {
        let mut view = session();
        view.runs.push(run("objective", source));
        let mut history = History::default();
        history.sync(&view, 16, 100);
        assert!(history.search(&view, "needle"));
        history.last_find = Some("needle".into());
        for index in 0..count {
            for width in [16, 40, 50, 80, 120, 16] {
                history.sync(&view, width, 100);
                let rows = message_rows(MessageKind::Assistant, source, width);
                let matches = source_matches(source, &rows, "needle");
                assert_eq!(matches.len(), count);
                assert_eq!(
                    history.matched_row,
                    Some(history.messages[1].start + matches[index].0)
                );
                assert_eq!(history.matched_source.unwrap().1, matches[index].1);
            }
            if index + 1 < count {
                assert_eq!(
                    history.pager_input(TerminalInput::Character('N'), &view),
                    Pager::Handled
                );
            }
        }
        assert!(!history.step_match(&view, false));
    }
}
