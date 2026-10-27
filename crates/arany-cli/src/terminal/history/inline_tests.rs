use super::tests::{run, session};
use super::*;
use crate::terminal::markdown;
use crate::terminal::view::{Palette, draw_history_rows, native_line};
use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Modifier};

const MIXED: &str = "Plain **bold** and *italic* with `code` &amp; \\*escaped\\*.\nsecond line  \nthird after hard break";
const WIDTHS: [usize; 5] = [16, 40, 50, 80, 120];

pub(super) fn styled(row: &HistoryRow, modifier: Modifier) -> String {
    row.spans
        .iter()
        .filter(|span| span.style.add_modifier.contains(modifier))
        .map(|span| &*span.content)
        .collect()
}

pub(super) fn body(source: &str, width: usize) -> Vec<String> {
    let rows = display_rows(MessageKind::Assistant, source, width, None);
    rows[1..rows.len() - 1]
        .iter()
        .map(|row| row.text.clone())
        .collect()
}

pub(super) fn select(
    history: &mut History,
    view: &SessionView,
    row: usize,
    column: usize,
    clicks: u8,
) {
    history.top = history.messages[1].start;
    history.select_at(view, row, column, clicks);
}

pub(super) fn copied(history: &History, view: &SessionView) -> String {
    history.selection_text(view).unwrap().unwrap()
}

fn assert_inline_styles(line: &[ratatui::buffer::Cell], spans: &[Span<'_>]) {
    let mut column = 0;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            if grapheme.width() > 0 {
                assert_eq!(line[column].modifier, span.style.add_modifier);
            }
            column += grapheme.width();
        }
    }
}

pub(super) fn assert_consumers_agree(source: &str) {
    for width in WIDTHS {
        let mut view = session();
        view.runs.push(run("objective", source));
        let palette = Palette { color: false };
        for agent in [false, true] {
            let mut history = History {
                agent,
                ..History::default()
            };
            history.sync(&view, width, 100);
            let rows = history.visible(&view);
            assert_eq!(rows.len(), history.total_rows);
            let area = Rect::new(0, 0, width as u16, 100);
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| draw_history_rows(frame, area, &rows, palette))
                .unwrap();
            for (line, row) in terminal.backend().buffer().content.chunks(width).zip(&rows) {
                let mut skip = 0;
                let text: String = line
                    .iter()
                    .filter_map(|cell| {
                        if skip > 0 {
                            skip -= 1;
                            return None;
                        }
                        skip = cell.symbol().width().saturating_sub(1);
                        Some(cell.symbol())
                    })
                    .collect();
                let native = native_line(row, area.width, palette);
                assert_eq!(
                    text.trim_end(),
                    native.to_string().trim_end(),
                    "width {width}"
                );
                if !row.spans.is_empty() {
                    assert_eq!(native.spans, row.spans);
                    assert_inline_styles(line, &native.spans);
                }
            }
            history.top = history.messages[1].start;
            history.select_at(&view, 1, 2, 1);
            history.extend_selection(&view, history.messages[1].rows - 1, width);
            assert_eq!(copied(&history, &view), display_text(source));
            history.sync(&view, 24, 100);
            assert_eq!(copied(&history, &view), display_text(source));
        }
        let rows = display_rows(MessageKind::Assistant, source, width, None);
        let (bytes, count) = markdown_cost(MessageKind::Assistant, source, width);
        assert_eq!(bytes, rows.iter().map(|row| row.text.len()).sum::<usize>());
        assert_eq!(count, rows.len());
        assert_eq!(
            classic_run_cost(&view.runs[0], width).1,
            message_rows(MessageKind::User, "objective", width).len() + rows.len() + 2
        );
        assert!(rows.iter().all(|row| row.text.width() <= width
            && !row.text.contains(['\r', '\u{1b}', '\u{7}', '\u{202e}'])));
    }
}

#[test]
fn paragraphs_decode_inline_syntax_and_share_consumers_counts_and_source_copy() {
    assert_eq!(
        body(MIXED, 80)
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        [
            "  Plain bold and italic with code & *escaped*.",
            "  second line",
            "  third after hard break"
        ]
    );
    let rows = display_rows(MessageKind::Assistant, MIXED, 80, None);
    assert_eq!(styled(&rows[1], Modifier::BOLD), "bold");
    assert_eq!(styled(&rows[1], Modifier::ITALIC), "italic");
    assert_eq!(styled(&rows[1], Modifier::REVERSED), "code");
    assert_eq!(styled(&rows[2], Modifier::BOLD), "");
    for source in [
        MIXED,
        "alpha beta gamma delta epsilon zeta eta theta iota kappa",
        "&#27;]52;c;x&#7; \u{1b}]52;c;y\u{7} \u{202e}**bi\u{202e}di**\n\\\u{1b}",
        "中 e\u{301} 👩‍💻 **中文 mixed 👩‍💻** wide text that wraps",
        "line **one\ntwo** line `a\nb` end\n\nnext *paragraph*",
        "Heading\n=======\n\nbody `code`\n\nSub\n---\n",
        "[link](https://example.test/a&amp;b) ![alt](x.png) <b>html</b>",
        "a\r\nb **c**\r\n\r\nd",
        "before <span\nclass=\"x\">after",
        "before <span\r\nclass=\"x\">after",
        "x <!-- a\nb --> y",
    ] {
        assert_consumers_agree(source);
    }
}

#[test]
fn setext_headings_are_bold_with_the_underline_hidden_and_copied_with_the_word() {
    let source = "Result\n======\n\nnext";
    assert_eq!(body(source, 80), ["  Result", "  ", "  next"]);
    let rows = display_rows(MessageKind::Assistant, source, 80, None);
    assert_eq!(styled(&rows[1], Modifier::BOLD), "Result");
    assert_eq!(styled(&rows[3], Modifier::BOLD), "");
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 80, 100);
    select(&mut history, &view, 1, 2, 2);
    assert_eq!(copied(&history, &view), "Result\n======");
}

#[test]
fn visible_words_select_their_source_across_delimiters_and_the_joined_line_break() {
    let source = "`alpha\nbeta` tail **two words**";
    assert_eq!(body(source, 80), ["  alpha beta tail two words"]);
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 80, 100);
    select(&mut history, &view, 1, 8, 2);
    assert_eq!(copied(&history, &view), "beta`");
    select(&mut history, &view, 1, 2, 2);
    assert_eq!(copied(&history, &view), "`alpha");
    select(&mut history, &view, 1, 23, 2);
    assert_eq!(copied(&history, &view), "words**");
}

#[test]
fn an_entity_decoding_to_several_graphemes_is_one_atomic_source_group() {
    let source = "x &fjlig; y";
    assert_eq!(body(source, 80), ["  x fj y"]);
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 3, 100);
    assert_eq!(body(source, 3), ["  x", "   ", "  f", "  j", "   ", "  y"]);
    for row in [3, 4] {
        select(&mut history, &view, row, 2, 2);
        assert_eq!(copied(&history, &view), "&fjlig;");
        history.sync(&view, 40, 100);
        assert_eq!(copied(&history, &view), "&fjlig;");
        history.sync(&view, 3, 100);
    }
    assert!(history.search(&view, "fjlig"));
    history.sync(&view, 16, 100);
    assert!(
        history
            .visible(&view)
            .iter()
            .any(|row| row.matched && row.text.contains("fj"))
    );
}

#[test]
fn wrapped_paragraphs_keep_find_and_selection_across_reflow() {
    let source = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 16, 100);
    assert!(history.search(&view, "epsilon"));
    for width in [24, 40, 16, 120] {
        history.sync(&view, width, 100);
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.matched && row.text.contains("epsilon")),
            "width {width}"
        );
    }
    history.sync(&view, 16, 100);
    let rows = body(source, 16);
    assert!(rows.len() > 3 && rows.iter().all(|row| row.width() <= 16));
    select(&mut history, &view, 2, 3, 2);
    let word = copied(&history, &view);
    assert!(rows[1].contains(&word) && source.contains(&word));
    history.sync(&view, 120, 100);
    assert_eq!(copied(&history, &view), word);
}

#[test]
fn tiny_widths_keep_content_and_counts() {
    let source = "ab **cd** `e`";
    assert_eq!(
        body(source, 3),
        ["  a", "  b", "   ", "  c", "  d", "   ", "  e"]
    );
    for width in [0, 1, 2, 3] {
        let rows = display_rows(MessageKind::Assistant, source, width, None);
        assert_eq!(
            markdown_cost(MessageKind::Assistant, source, width),
            (rows.iter().map(|row| row.text.len()).sum(), rows.len())
        );
        assert!(rows.iter().all(|row| row.text.width() <= width));
    }
    let long = "w".repeat(markdown::MAX_MARKDOWN_PARSE_BYTES);
    for width in [16, 80] {
        let rows = display_rows(MessageKind::Assistant, &long, width, None);
        assert_eq!(
            markdown_cost(MessageKind::Assistant, &long, width),
            (rows.iter().map(|row| row.text.len()).sum(), rows.len())
        );
    }
}

#[test]
fn decoded_graphemes_map_to_ordered_non_empty_source_ranges() {
    for source in [
        "plain text",
        "a &amp; b &#35; &copy; x",
        "\\*not emphasis\\* \\\\ end",
        "`a\nb` tail",
        "`a\n  b` tail",
        "text  \nhard\\\nbreak",
        "**bold** *it* `c`",
        "&fjlig; end",
        "line one\nline two\nline three",
        "`` a`b `` and ` x `",
        "\\\\&amp; x",
        "**&fjlig;** and *&fjlig; x*",
        "`&a\nb` &amp;",
        "`a\r\nb` tail",
        "one  \n\\\ntwo",
        "\\\nlead",
        "| h |\n| - |\n| `a\\|b` |",
    ] {
        for width in [16, 80] {
            let ((rows, _, _), cue) = markdown::project(source, width, true);
            assert!(cue.is_none());
            let cells: Vec<_> = rows.iter().flat_map(|row| &row.cells).collect();
            for cell in &cells {
                assert!(
                    !cell.source.is_empty() && cell.source.end <= source.len(),
                    "{source:?}: {:?}",
                    cell.source
                );
            }
            for pair in cells.windows(2) {
                let (previous, next) = (&pair[0].source, &pair[1].source);
                assert!(
                    next.start >= previous.start
                        && (next.start >= previous.end || next == previous),
                    "{source:?}: {previous:?} then {next:?}"
                );
            }
        }
    }
}

#[test]
fn normalized_code_starting_with_an_ampersand_keeps_per_grapheme_provenance() {
    let source = "`&a\nzed`";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 3, 100);
    assert_eq!(body(source, 3), ["  &", "  a", "   ", "  z", "  e", "  d"]);
    assert!(history.search(&view, "zed"));
    assert!(
        history
            .visible(&view)
            .iter()
            .any(|row| row.matched && row.text == "  z")
    );
    for width in [3, 40, 3] {
        history.sync(&view, width, 100);
        select(
            &mut history,
            &view,
            if width == 3 { 5 } else { 1 },
            if width == 3 { 2 } else { 6 },
            2,
        );
        assert_eq!(copied(&history, &view), "zed`", "width {width}");
    }
}

#[test]
fn atomic_entity_groups_widen_together_and_select_alike_in_both_directions() {
    let source = "**&fjlig;**";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 3, 100);
    assert_eq!(body(source, 3), ["  f", "  j"]);
    for (from, to) in [(1, 2), (2, 1)] {
        select(&mut history, &view, from, 2, 1);
        history.extend_selection(&view, to, 2);
        assert_eq!(copied(&history, &view), source);
        history.sync(&view, 40, 100);
        assert_eq!(copied(&history, &view), source);
        history.sync(&view, 3, 100);
    }
}

#[test]
fn escaped_pipes_in_table_code_keep_both_source_bytes() {
    let source = "| h |\n| - |\n| `a\\|b` |";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 80, 100);
    assert_eq!(body(source, 80)[1].trim_end(), "  a|b");
    select(&mut history, &view, 2, 3, 1);
    history.extend_selection(&view, 2, 2);
    history.extend_selection(&view, 2, 3);
    assert!(history.has_selection());
    assert_eq!(copied(&history, &view), "\\|");
    assert!(history.search(&view, "a\\|b"));
    assert!(
        history
            .visible(&view)
            .iter()
            .any(|row| row.matched && row.text.contains("a|b"))
    );
}

#[test]
fn an_entity_after_an_escaped_backslash_is_not_an_escape() {
    let source = "\\\\&amp; x";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 3, 100);
    assert_eq!(body(source, 3), ["  \\", "  &", "   ", "  x"]);
    select(&mut history, &view, 2, 2, 2);
    assert_eq!(copied(&history, &view), "&amp;");
    assert!(history.search(&view, "&amp;"));
    assert!(
        history
            .visible(&view)
            .iter()
            .any(|row| row.matched && row.text == "  &")
    );
    history.sync(&view, 80, 100);
    select(&mut history, &view, 1, 3, 2);
    assert_eq!(copied(&history, &view), "&amp;");
}

#[test]
fn multiline_inline_html_keeps_source_rows_selection_find_and_reflow() {
    for source in [
        "before <span\nclass=\"x\">after",
        "before <span\r\nclass=\"x\">after",
    ] {
        assert_eq!(body(source, 80), ["  before <span", "  class=\"x\">after"]);
        let mut view = session();
        view.runs.push(run("objective", source));
        let mut history = History::default();
        history.sync(&view, 80, 100);
        select(&mut history, &view, 2, 2, 2);
        assert_eq!(copied(&history, &view), "class");
        assert!(history.search(&view, "class"));
        for width in [16, 80] {
            history.sync(&view, width, 100);
            assert!(
                history
                    .visible(&view)
                    .iter()
                    .any(|row| row.matched && row.text.contains("class"))
            );
        }
    }
}

#[test]
fn crlf_inside_inline_code_maps_each_joined_space_to_one_source_byte() {
    let source = "`a\r\nzed`";
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 3, 100);
    assert_eq!(body(source, 3), ["  a", "   ", "   ", "  z", "  e", "  d"]);
    assert!(history.search(&view, "zed"));
    assert!(
        history
            .visible(&view)
            .iter()
            .any(|row| row.matched && row.text == "  z")
    );
    for width in [3, 40, 3] {
        history.sync(&view, width, 100);
        let (row, column) = if width == 3 { (5, 2) } else { (1, 7) };
        select(&mut history, &view, row, column, 2);
        assert_eq!(copied(&history, &view), "zed`", "width {width}");
    }
}

#[test]
fn consecutive_hard_breaks_keep_their_empty_rows() {
    let source = "one  \n\\\ntwo";
    for width in [80, 16] {
        assert_eq!(body(source, width), ["  one", "  ", "  two"]);
    }
    let mut view = session();
    view.runs.push(run("objective", source));
    let mut history = History::default();
    history.sync(&view, 80, 100);
    select(&mut history, &view, 3, 2, 2);
    assert_eq!(copied(&history, &view), "two");
    assert!(history.search(&view, "two"));
    history.sync(&view, 16, 100);
    assert!(
        history
            .visible(&view)
            .iter()
            .any(|row| row.matched && row.text == "  two")
    );
}

fn assert_leading_rows(source: &str, anchors: &[usize], first: &Range<usize>) {
    for width in [16, 80] {
        let rows = message_rows(MessageKind::Assistant, source, width);
        let expected: Vec<_> = std::iter::repeat_n("  ", anchors.len())
            .chain(["  lead"])
            .collect();
        assert_eq!(body(source, width), expected);
        for (index, anchor) in anchors.iter().enumerate() {
            assert_eq!(rows[index + 1].source_byte, *anchor);
            assert!(rows[index + 1].cells.as_ref().unwrap().is_empty());
            assert_eq!(row_of_byte(&rows, *anchor), index + 1);
            assert!(
                rows.iter()
                    .flat_map(|row| &row.cells)
                    .flatten()
                    .all(|cell| !cell.source.contains(anchor))
            );
        }
        let lead = anchors.len() + 1;
        assert_eq!(rows[lead].source_byte, first.start);
        assert_eq!(&rows[lead].cells.as_ref().unwrap()[0].source, first);
        assert_eq!(row_of_byte(&rows, source.find("lead").unwrap()), lead);
    }
}

#[test]
fn leading_hard_breaks_keep_exact_rows_find_and_reading_anchors() {
    for (source, anchors, first) in [
        ("\\\nlead", &[0][..], 2..3),
        ("\\\n\\\nlead", &[0, 2][..], 4..5),
        ("**\\\nlead**", &[0][..], 4..5),
        ("\\\n**lead**", &[0][..], 2..5),
    ] {
        assert_consumers_agree(source);
        assert_leading_rows(source, anchors, &first);
        for agent in [false, true] {
            let mut view = session();
            view.runs.push(run("objective", source));
            let mut history = History {
                agent,
                ..History::default()
            };
            history.sync(&view, 80, 2);
            for index in 0..anchors.len() {
                history.top = history.messages[1].start + index + 1;
                history.follow_tail = false;
                for (width, append) in [(16, false), (16, true), (80, false), (80, true)] {
                    if append {
                        view.runs.push(run("append", "tail"));
                    }
                    history.sync(&view, width, 2);
                    assert_eq!(history.top, history.messages[1].start + index + 1);
                    assert_eq!(history.visible(&view)[0].text, "  ");
                    assert!(!history.follow_tail);
                }
            }
            assert!(history.search(&view, "\\"));
            history.last_find = Some("\\".into());
            for index in 0..anchors.len() {
                if index > 0 {
                    assert!(history.step_match(&view, false));
                }
                for (width, append) in [(16, false), (16, true), (80, false), (80, true)] {
                    if append {
                        view.runs.push(run("append", "tail"));
                    }
                    history.sync(&view, width, 2);
                    assert_eq!(
                        history.matched_row,
                        Some(history.messages[1].start + index + 1)
                    );
                    assert_eq!(
                        history
                            .visible(&view)
                            .iter()
                            .filter(|row| row.matched)
                            .map(|row| row.text.as_str())
                            .collect::<Vec<_>>(),
                        ["  "]
                    );
                }
            }
            assert!(!history.step_match(&view, false));
        }
    }
}

#[test]
fn links_and_images_keep_accumulated_styles_in_prose_and_tables() {
    for (markup, shown, bold, italic) in [
        ("**[label](dest)**", "label (dest)", "label (dest)", ""),
        ("*![alt](x.png)*", "![alt](x.png)", "", "![alt](x.png)"),
        (
            "***[label](dest)***",
            "label (dest)",
            "label (dest)",
            "label (dest)",
        ),
        (
            "***![alt](x.png)***",
            "![alt](x.png)",
            "![alt](x.png)",
            "![alt](x.png)",
        ),
        (
            "**[a *label*](dest)**",
            "a label (dest)",
            "a label (dest)",
            "label",
        ),
        (
            "*![**alt**](x.png)*",
            "![**alt**](x.png)",
            "",
            "![**alt**](x.png)",
        ),
        (
            "***[label](a&amp;b)***",
            "[label](a&amp;b)",
            "[label](a&amp;b)",
            "[label](a&amp;b)",
        ),
    ] {
        for source in [
            format!("{markup} plain"),
            format!("| h |\n| - |\n| {markup} plain |"),
        ] {
            assert_consumers_agree(&source);
            for kind in [
                MessageKind::Assistant,
                MessageKind::AgentResult,
                MessageKind::Preview,
                MessageKind::Incomplete,
            ] {
                let rows = display_rows(kind, &source, 120, None);
                let row = rows.iter().find(|row| row.text.contains(shown)).unwrap();
                assert_eq!(row.text, format!("  {shown} plain"));
                for (modifier, expected) in [(Modifier::BOLD, bold), (Modifier::ITALIC, italic)] {
                    assert_eq!(styled(row, modifier), expected);
                }
            }
        }
    }
}
