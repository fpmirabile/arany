use super::tests::{run, session};
use super::*;
use crate::presentation::{Output, render_exec};
use crate::terminal::markdown;
use crate::terminal::view::{Palette, draw_history_rows, native_line};
use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Modifier};

const HEADING_CASES: &[(&str, &str, bool)] = &[
    (
        "# Result ###\r\n\r\n```\r\nraw\r\n```\r\n\r\nnext",
        "Result",
        true,
    ),
    (r"  ## \*Result", r"\*Result", true),
    (
        "# Result\rretained text",
        r"# Result\u{000d}retained text",
        false,
    ),
    ("Result\r# Heading", r"Result\u{000d}# Heading", false),
];

#[test]
fn committed_headings_share_consumers_counts_source_copy_find_and_reflow() {
    for &(source, expected, heading) in HEADING_CASES {
        for width in [16, 40, 50, 80, 120] {
            let mut view = session();
            view.runs.push(run("objective", source));
            let palette = Palette { color: false };
            let copied = display_text(source);
            for agent in [false, true] {
                let mut history = History {
                    agent,
                    ..History::default()
                };
                history.sync(&view, width, 100);
                let rows = history.visible(&view);
                assert_eq!(rows.len(), history.total_rows);
                if width >= 40 {
                    assert_eq!(rows[4].text, format!("  {expected}"));
                }
                let area = Rect::new(0, 0, width as u16, 100);
                let mut terminal =
                    Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
                terminal
                    .draw(|frame| draw_history_rows(frame, area, &rows, palette))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let first = buffer.cell((2, 4)).unwrap();
                assert_eq!(first.modifier.contains(Modifier::BOLD), heading);
                for (line, row) in buffer.content.chunks(width).zip(&rows) {
                    let text: String = line.iter().map(|cell| cell.symbol()).collect();
                    let native = native_line(row, area.width, palette).to_string();
                    assert_eq!(text.trim_end(), native.trim_end());
                }
                history.top = history.messages[1].start;
                history.select_at(&view, 1, 2, 1);
                history.extend_selection(&view, history.messages[1].rows - 1, width);
                assert_eq!(history.selection_text(&view).unwrap().unwrap(), copied);
                history.sync(&view, 24, 100);
                assert_eq!(history.selection_text(&view).unwrap().unwrap(), copied);
                assert!(history.search(&view, "Result"));
                history.sync(&view, 40, 100);
                assert!(
                    history
                        .visible(&view)
                        .iter()
                        .any(|row| row.matched && row.text.contains("Result"))
                );
                history.top = history.messages[1].start;
                history.select_at(&view, 1, expected.find("Result").unwrap() + 2, 2);
                assert_eq!(history.selection_text(&view).unwrap().unwrap(), "Result");
            }
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            let (bytes, count) = markdown_cost(MessageKind::Assistant, source, width);
            assert_eq!(bytes, rows.iter().map(|row| row.text.len()).sum::<usize>());
            assert_eq!(count, rows.len());
            assert_eq!(
                classic_run_cost(&view.runs[0], width).1,
                message_rows(MessageKind::User, "objective", width).len() + rows.len() + 2
            );
            assert_eq!(
                render_exec(&view.runs[0], &[], Output::Text, false),
                format!("Answer:\n  {}\n", copied.replace('\n', "\n  "))
            );
        }
    }
}

#[test]
fn unsupported_syntax_controls_limits_and_nonretaining_counts_stay_literal() {
    let over_bytes = "a".repeat(markdown::MAX_MARKDOWN_PARSE_BYTES + 1);
    let over_rows = "line\n".repeat(markdown::MAX_MARKDOWN_ROWS);
    let over_depth = format!("{}answer", "> ".repeat(markdown::MAX_MARKDOWN_DEPTH));
    for source in [
        "```\r\nraw\r\n```\r\n\r\nnext",
        "    x\r\n\r\nnext",
        "**word** `code` [site](https://e.test/a&amp;b)",
        "# &#27; \u{1b}]52;c;text\u{7}\u{202e}",
        "<script>image</script>\n\n![image](file:///image.png)",
        "#\n",
        "Result\n======\n\nnext",
        "  ## 中 e\u{301} 👩‍💻\n\nnext",
        &over_bytes[..markdown::MAX_MARKDOWN_PARSE_BYTES],
        over_bytes.as_str(),
        over_rows.as_str(),
        &over_depth["> ".len()..],
        over_depth.as_str(),
    ] {
        for width in [0, 1, 2, 3, 16, 40, 80, 120, MAX_ROW_CELLS] {
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            let (bytes, count) = markdown_cost(MessageKind::Assistant, source, width);
            assert_eq!(bytes, rows.iter().map(|row| row.text.len()).sum::<usize>());
            assert_eq!(count, rows.len());
            assert!(rows.iter().all(|row| row.text.width() <= width
                && !row.text.contains(['\r', '\u{1b}', '\u{7}', '\u{202e}'])));
            if width == 80 {
                let limited = [over_bytes.as_str(), over_rows.as_str(), over_depth.as_str()]
                    .contains(&source);
                assert_eq!(
                    rows.iter()
                        .any(|row| row.text.contains(markdown::LIMIT_CUE)),
                    limited
                );
                let short = source.len() < width;
                if !limited && short && !source.trim_start().starts_with('#') {
                    let expected: Vec<_> = source
                        .split('\n')
                        .map(|line| format!("  {}", escape_terminal(line)))
                        .collect();
                    assert_eq!(
                        rows[1..rows.len() - 1]
                            .iter()
                            .map(|row| &row.text)
                            .collect::<Vec<_>>(),
                        expected.iter().collect::<Vec<_>>()
                    );
                }
            }
        }
    }
    for kind in [
        MessageKind::Preview,
        MessageKind::Incomplete,
        MessageKind::User,
    ] {
        assert!(
            display_rows(kind, "# Result", 80, None)
                .iter()
                .any(|row| row.text.contains("# Result"))
        );
    }
}
