use super::tests::{run, session};
use super::*;
use crate::presentation::{Output, render_exec};
use crate::terminal::markdown;
use crate::terminal::view::{Palette, draw_history_rows, native_line};
use pulldown_cmark::{Event, Options, Parser, Tag};
use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Modifier};

const MARKDOWN_CASES: &[(&str, &str, bool, &str)] = &[
    (
        "# Result ###\r\n\r\n```\r\nraw\r\n```\r\n\r\nnext",
        "Result",
        true,
        "Result",
    ),
    (r"  ## \*Result", r"\*Result", true, "Result"),
    ("```Result\n  中 e\u{301} 👩‍💻\n```", "Result", true, "Result"),
    ("~~~Result&#27;\nbody\n~~~", "Result&#27;", true, "Result"),
    ("~~~Result\nbody\n~~~", "Result", true, "Result"),
    ("  ~~~~Result\nbody\n  ~~~~", "Result", true, "Result"),
    ("~~~\nResult\n~~~", "Result", false, "Result"),
    ("~~~\n~Result~\n~~~", "~Result~", false, "~Result~"),
    ("    Result\n      value", "Result", false, "Result"),
    (
        "# Result\rretained text",
        r"# Result\u{000d}retained text",
        false,
        "Result",
    ),
    (
        "Result\r# Heading",
        r"Result\u{000d}# Heading",
        false,
        "Result",
    ),
    (
        "~~~Result\rraw",
        r"~~~Result\u{000d}raw",
        false,
        "~~~Result",
    ),
];

#[test]
fn committed_markdown_shares_consumers_counts_source_copy_find_and_reflow() {
    for &(source, expected, heading, selected) in MARKDOWN_CASES {
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
                assert_eq!(
                    history.selection_text(&view).unwrap().unwrap(),
                    selected,
                    "source {source:?}, width {width}, agent {agent}"
                );
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
fn syntax_controls_limits_and_nonretaining_counts_agree() {
    let over_bytes = "a".repeat(markdown::MAX_MARKDOWN_PARSE_BYTES + 1);
    let over_rows = "line\n".repeat(markdown::MAX_MARKDOWN_ROWS);
    let over_depth = format!("{}answer", "> ".repeat(markdown::MAX_MARKDOWN_DEPTH));
    for (case, source) in [
        "```\r\nraw\r\n```\r\n\r\nnext",
        "    x\r\n\r\nnext",
        "- ~~~\n  code\n  ~~~",
        "7. ~~~\n   code\n   ~~~",
        "- - ~~~\n    code\n    ~~~",
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
    ]
    .into_iter()
    .enumerate()
    {
        for width in [0, 1, 2, 3, 16, 40, 80, 120, MAX_ROW_CELLS] {
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            let (bytes, count) = markdown_cost(MessageKind::Assistant, source, width);
            assert_eq!(
                bytes,
                rows.iter().map(|row| row.text.len()).sum::<usize>(),
                "case {case}, width {width}: rendered byte count"
            );
            assert_eq!(count, rows.len(), "case {case}, width {width}: row count");
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
                if !limited
                    && short
                    && !source.trim_start().starts_with('#')
                    && !source.starts_with("```")
                    && !source.starts_with("    ")
                {
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
    for kind in [MessageKind::Preview, MessageKind::Incomplete] {
        for source in [&over_bytes, &over_rows, &over_depth] {
            let rows = display_rows(kind, source, 80, None);
            assert!(
                rows.iter()
                    .any(|row| row.text.contains(markdown::LIMIT_CUE))
            );
            assert_eq!(
                markdown_cost(kind, source, 80),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
        }
    }
    for kind in [
        MessageKind::User,
        MessageKind::Commentary,
        MessageKind::Tool,
    ] {
        assert!(
            display_rows(kind, "# Result", 80, None)
                .iter()
                .any(|row| row.text.contains("# Result"))
        );
    }
}

#[test]
fn code_whitespace_info_strings_and_narrow_geometry() {
    for (source, expected) in [
        (
            "```rust\n  Result 中 e\u{301} 👩‍💻\n\n    value\n```\nnext",
            vec!["rust", "  Result 中 e\u{301} 👩‍💻", "", "    value", "next"],
        ),
        (
            "    Result\n      value\n\nnext",
            vec!["Result", "  value", "", "next"],
        ),
        ("```\n\tResult\n```", vec!["    Result"]),
        (
            "~~~&#27;]52;c;unsafe&#7;\nResult\n~~~",
            vec!["&#27;]52;c;unsafe&#7;", "Result"],
        ),
        (
            "~~~rust\u{1b}\nResult\n~~~",
            vec![r"rust\u{001b}", "Result"],
        ),
    ] {
        for width in [0, 1, 2, 3, 16, 40, 50, 80, 120] {
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            assert_eq!(
                markdown_cost(MessageKind::Assistant, source, width),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
            assert!(
                rows.iter().all(|row| row.text.width() <= width
                    && !row.text.contains(['\r', '\u{1b}', '\u{7}']))
            );
            if width == 80 {
                assert_eq!(
                    rows[1..rows.len() - 1]
                        .iter()
                        .map(|row| row.text.strip_prefix("  ").unwrap())
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }
}

const TABLE: &str = "| Name | Description |\n| --- | --- |\n| **Result** | escaped \\| pipe and *value* |\n| `code` | https://example.test/a/long/path/to/result |\n| `left\\|right` | extra |\n| empty |";

#[test]
fn tables_fit_or_use_records_with_source_copy_find_and_exact_classic_costs() {
    for source in [
        TABLE,
        "| Result | Value |\n| --- | --- |\n| 中 e\u{301} 👩‍💻 | &#27;]52;c;text&#7; |",
        "Result | Description\n--- | ---\nx aaaaaa&#27; | bbbbbbbbbbbbbbbb",
    ] {
        for width in [0, 1, 2, 3, 16, 21, 40, 50, 80, 120] {
            let mut view = session();
            view.runs.push(run("objective", source));
            let rows = display_rows(MessageKind::Assistant, source, width, None);
            assert_eq!(
                markdown_cost(MessageKind::Assistant, source, width),
                (rows.iter().map(|row| row.text.len()).sum(), rows.len())
            );
            assert!(rows.iter().all(|row| row.text.width() <= width && !row.text.contains(['\u{1b}', '\u{7}'])));
            if width < 16 {
                continue;
            }
            let user = display_rows(MessageKind::User, "objective", width, None);
            assert_eq!(
                classic_run_cost(&view.runs[0], width),
                (
                    user.iter()
                        .chain(&rows)
                        .map(
                            |row| native_line(row, width as u16, Palette { color: false })
                                .to_string()
                                .len()
                        )
                        .sum(),
                    user.len() + rows.len() + 2,
                )
            );
            for kind in [
                MessageKind::Assistant,
                MessageKind::AgentResult,
                MessageKind::Preview,
                MessageKind::Incomplete,
            ] {
                let rows = display_rows(kind, source, width, None);
                let palette = Palette { color: false };
                let area = Rect::new(0, 0, width as u16, rows.len() as u16);
                let mut terminal =
                    Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
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
                    assert_eq!(
                        text.trim_end(),
                        native_line(row, area.width, palette).to_string().trim_end()
                    );
                }
            }
            for agent in [false, true] {
                let mut history = History {
                    agent,
                    details: true,
                    ..History::default()
                };
                history.sync(&view, width, 100);
                assert_eq!(history.total_rows, history.visible(&view).len());
                history.top = history.messages[1].start;
                history.select_at(&view, 1, 0, 1);
                history.extend_selection(&view, history.messages[1].rows - 1, width);
                assert_eq!(
                    history.selection_text(&view).unwrap().unwrap(),
                    display_text(source)
                );
                history.sync(&view, 24, 100);
                assert_eq!(
                    history.selection_text(&view).unwrap().unwrap(),
                    display_text(source)
                );
                assert!(history.search(&view, "Result"));
                history.sync(&view, 80, 100);
                assert!(
                    history
                        .visible(&view)
                        .iter()
                        .any(|row| row.matched && row.text.contains("Result"))
                );
                if source == TABLE {
                    history.sync(&view, width, 100);
                    history.top = history.messages[1].start;
                    let first = rows
                        .iter()
                        .position(|row| row.text.contains("Result"))
                        .unwrap();
                    let last = rows
                        .iter()
                        .position(|row| row.text.contains("code"))
                        .unwrap()
                        - 1;
                    history.select_at(&view, first, 2, 1);
                    history.extend_selection(&view, last, rows[last].text.width() - 1);
                    assert_eq!(
                        history.selection_text(&view).unwrap().unwrap(),
                        TABLE.split_inclusive('\n').nth(2).unwrap()
                    );
                    history.sync(&view, 80, 100);
                    let body = rows
                        .iter()
                        .map(|row| row.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if width == 16 {
                        assert!(body.contains("Name: Result"));
                        let records: String = rows
                            .iter()
                            .map(|row| row.text.strip_prefix("  ").unwrap_or(&row.text))
                            .collect();
                        assert!(records.contains("Description: escaped | pipe and value"));
                    }
                    if width >= 80 {
                        history.top = history.messages[1].start;
                        history.select_at(&view, 2, 2, 2);
                        assert_eq!(history.selection_text(&view).unwrap().unwrap(), "Result");
                        history.select_at(&view, 3, 2, 2);
                        assert_eq!(history.selection_text(&view).unwrap().unwrap(), "code");
                        assert!(history.search(&view, "long/path"));
                        assert!(history.search(&view, "---"));
                        assert!(body.contains("escaped | pipe and value"));
                        assert!(body.contains("left|right"));
                        assert!(body.contains("│"));
                        assert!(!body.contains("**"));
                        let value = rows
                            .iter()
                            .flat_map(|row| &row.spans)
                            .find(|span| span.content == "R")
                            .unwrap();
                        assert!(value.style.add_modifier.contains(Modifier::BOLD));
                    }
                }
            }
        }
    }
}

#[test]
fn table_matrix_padding_and_output_limits_fall_back_as_whole_messages() {
    const MAX_AUTOCOMPLETED_CELLS: usize = 1 << 18;
    let columns = markdown::MAX_MARKDOWN_TABLE_COLUMNS;
    let rows = markdown::MAX_MARKDOWN_TABLE_ROWS;
    let table = |columns, rows| {
        format!(
            "{}\n{}\n{}",
            "Name |".repeat(columns),
            "--- |".repeat(columns),
            "value\n".repeat(rows)
        )
    };
    let at_limit = table(columns, rows);
    let too_wide = table(columns + 1, 1);
    let too_tall = table(1, rows + 1);
    let padding = table(columns * rows, rows + 1);
    assert!(padding.len() <= markdown::MAX_MARKDOWN_PARSE_BYTES);
    assert!((columns * rows - 1) * (rows + 1) > MAX_AUTOCOMPLETED_CELLS);
    assert_eq!(
        Parser::new_ext(&padding, Options::ENABLE_TABLES)
            .into_offset_iter()
            .filter(
                |(event, range)| matches!(event, Event::Start(Tag::TableCell)) && range.is_empty()
            )
            .count(),
        MAX_AUTOCOMPLETED_CELLS
    );
    let long_header = format!(
        "{} | Value\n--- | ---\n{}",
        "heading ".repeat(160),
        "value | text\n".repeat(rows)
    );
    let output = format!(
        "{} | Value\n--- | ---\n{}",
        "heading".repeat(600),
        "value | text\n".repeat(rows)
    );
    for (source, width, limited) in [
        (&at_limit, 120, false),
        (&too_wide, 80, true),
        (&too_tall, 80, true),
        (&padding, 80, true),
        (&long_header, 16, true),
        (&output, MAX_ROW_CELLS, true),
    ] {
        assert!(source.len() <= markdown::MAX_MARKDOWN_PARSE_BYTES);
        let rows = display_rows(MessageKind::Assistant, source, width, None);
        assert_eq!(
            markdown_cost(MessageKind::Assistant, source, width),
            (rows.iter().map(|row| row.text.len()).sum(), rows.len())
        );
        let literal = literal_rows(MessageKind::Assistant, source, width);
        let mut expected: Vec<_> = literal.iter().map(|row| &row.text).collect();
        let cue_rows = literal_rows(MessageKind::Notice, markdown::LIMIT_CUE, width);
        let cue: Vec<_> = cue_rows.iter().skip(1).map(|row| &row.text).collect();
        let actual: Vec<_> = rows.iter().map(|row| &row.text).collect();
        assert_eq!(actual.ends_with(&cue), limited, "width {width}");
        if limited {
            expected.extend(cue);
            assert_eq!(actual, expected, "width {width}");
        }
    }
}

#[test]
fn classic_rejects_the_whole_run_when_generated_records_exceed_its_budget() {
    let source = format!(
        "{} | {}\n{}\n{}",
        "heading".repeat(155),
        "Name |".repeat(markdown::MAX_MARKDOWN_TABLE_COLUMNS - 1),
        "--- |".repeat(markdown::MAX_MARKDOWN_TABLE_COLUMNS),
        "value\n".repeat(markdown::MAX_MARKDOWN_TABLE_ROWS / 2)
    );
    let mut view = session();
    view.runs.push(run("objective", &source));
    let rows = display_rows(MessageKind::Assistant, &source, 40, None);
    assert!(
        !rows
            .iter()
            .any(|row| row.text.contains(markdown::LIMIT_CUE))
    );
    assert!(rows.iter().map(|row| row.text.len()).sum::<usize>() > MAX_CLASSIC_HISTORY_BYTES);
    assert!(classic_projection(&view, 40).first_run_index.is_none());
    view.runs[0].assistant_message = None;
    view.runs[0].status = arany::RunStatus::Interrupted;
    view.runs[0].display.push(arany::DisplayChunk {
        call_index: 0,
        phase: arany::AgentPhase::RootPlan,
        offset: 0,
        category: arany::DisplayCategory::Answer,
        item: 0,
        text: source,
    });
    assert!(classic_projection(&view, 40).first_run_index.is_none());
}

#[test]
fn streaming_snapshots_replace_partial_markdown_and_keep_source_on_resize() {
    for source in [
        "# Result\n\n*unfinished emphasis*",
        "# Result\n\n[label](https://example.test)",
        "# Result\n\n```rust\n  中 e\u{301} value\n```",
        "# Result\n\n| Name | Value |\n| --- | --- |\n| **first** | second |",
    ] {
        for width in [16, 40, 50, 80, 120] {
            for agent in [false, true] {
                let mut view = session();
                let mut active = run("objective", "");
                active.status = arany::RunStatus::Active;
                active.assistant_message = None;
                view.runs.push(active);
                let mut history = History {
                    agent,
                    ..History::default()
                };
                history.sync(&view, width, 100);
                let actor = arany::AgentRunId::new();
                replace_prefixes(&mut history, &view, source, actor, width);
                assert!(
                    history
                        .visible(&view)
                        .iter()
                        .any(|row| row.text == "  Result")
                );
                assert!(history.search(&view, "Result"));
                history.top = history.messages[1].start;
                history.select_at(&view, 1, 0, 1);
                history.extend_selection(&view, history.messages[1].rows - 1, width);
                history.follow_tail = false;
                history.sync(&view, 24, 100);
                assert_eq!(history.selection_text(&view).unwrap().unwrap(), source);
                assert!(
                    history
                        .visible(&view)
                        .iter()
                        .any(|row| row.matched && row.text.contains("Result"))
                );
                view.runs[0].assistant_message = Some(source.into());
                view.runs[0].status = arany::RunStatus::Finished;
                view.last_sequence += 1;
                history.set_preview(None);
                history.sync(&view, 24, 100);
                assert_eq!(history.selection_text(&view).unwrap().unwrap(), source);
                assert_eq!(history.messages.len(), 2);
                assert_eq!(
                    history.messages[1].kind,
                    if agent {
                        MessageKind::AgentResult
                    } else {
                        MessageKind::Assistant
                    }
                );
                assert!(history.live_rows(&view).is_empty());
            }
        }
    }
}

fn replace_prefixes(
    history: &mut History,
    view: &SessionView,
    source: &str,
    actor: arany::AgentRunId,
    width: usize,
) {
    for end in source.char_indices().map(|(byte, ch)| byte + ch.len_utf8()) {
        let snapshot = arany::DisplayUpdate {
            run_id: view.runs[0].id,
            agent_run_id: actor,
            call_index: 0,
            text: std::sync::Arc::new(arany::PreviewText {
                answer: source[..end].into(),
                ..Default::default()
            }),
        };
        assert!(history.set_preview(Some(snapshot.clone())));
        history.sync(view, width, 100);
        assert!(!history.set_preview(Some(snapshot)));
        assert!(!history.preview_dirty);
        history.sync(view, width, 100);
        assert_eq!(history.messages.len(), 2);
        let expected = display_rows(MessageKind::Preview, &source[..end], width, None);
        assert_eq!(history.messages[1].rows, expected.len());
        assert_eq!(history.total_rows, history.visible(view).len());
        assert_eq!(
            history
                .live_rows(view)
                .iter()
                .map(|row| (&row.text, &row.spans))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|row| (&row.text, &row.spans))
                .collect::<Vec<_>>()
        );
        let committed = display_rows(MessageKind::Assistant, &source[..end], width, None);
        assert_eq!(
            expected
                .iter()
                .skip(1)
                .map(|row| (&row.text, &row.spans))
                .collect::<Vec<_>>(),
            committed
                .iter()
                .skip(1)
                .map(|row| (&row.text, &row.spans))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn incomplete_checkpoints_format_with_exact_classic_costs_and_withdraw_on_rejection() {
    use arany::ProviderCallDisposition;
    let source = "# Result\n\n```rust\n  retained body";
    let view = checkpoint_view(source);
    for width in [16, 40, 50, 80, 120] {
        let mut history = History::default();
        history.sync(&view, width, 100);
        let preview = arany::DisplayUpdate {
            run_id: view.runs[0].id,
            agent_run_id: view.runs[0].agents[0].id,
            call_index: 0,
            text: std::sync::Arc::new(arany::PreviewText {
                answer: format!("{source}\nunacknowledged suffix"),
                ..Default::default()
            }),
        };
        history.set_preview(Some(preview.clone()));
        history.sync(&view, width, 100);
        for status in [arany::RunStatus::Cancelled, arany::RunStatus::Interrupted] {
            let mut reopened = view.clone();
            reopened.runs[0].status = status;
            reopened.last_sequence += 1;
            let mut recovered = History::default();
            recovered.sync(&reopened, width, 100);
            assert_eq!(recovered.messages[1].kind, MessageKind::Incomplete);
            assert!(
                recovered
                    .visible(&reopened)
                    .iter()
                    .all(|row| !row.text.contains("unacknowledged"))
            );
            assert!(
                recovered
                    .visible(&reopened)
                    .iter()
                    .any(|row| row.text == "  Result")
            );
            let linear = super::super::linear::transcript_details(&reopened.runs[0]);
            assert_eq!(linear, [format!("Arany · incomplete:\n{source}")]);
            let rows = display_rows(MessageKind::User, "objective", width, None)
                .into_iter()
                .chain(display_rows(MessageKind::Incomplete, source, width, None))
                .collect::<Vec<_>>();
            assert_eq!(
                classic_run_cost(&reopened.runs[0], width),
                (
                    rows.iter()
                        .map(
                            |row| native_line(row, width as u16, Palette { color: false })
                                .to_string()
                                .len()
                        )
                        .sum(),
                    rows.len() + 2
                )
            );
        }
        assert!(history.set_preview(None));
        history.sync(&view, width, 100);
        assert!(
            history
                .visible(&view)
                .iter()
                .all(|row| !row.text.contains("unacknowledged"))
        );
        history.set_preview(Some(preview));
        let mut rejected = view.clone();
        rejected.runs[0].agents[0].provider_calls[0].disposition =
            ProviderCallDisposition::InvalidResponse;
        rejected.last_sequence += 1;
        history.sync(&rejected, width, 100);
        assert_eq!(history.messages.len(), 1);
        assert!(history.live_rows(&rejected).is_empty());
        assert_eq!(rejected.runs[0].display[0].text, source);
        history.set_preview(None);
        rejected.runs[0].display.clear();
        rejected.last_sequence += 1;
        history.sync(&rejected, width, 100);
        assert_eq!(history.messages.len(), 1);
    }
}

fn checkpoint_view(source: &str) -> SessionView {
    use arany::{DisplayCategory, DisplayChunk, ProviderCallDisposition};
    let mut view = crate::presentation::team_view();
    let mut active = view.runs.remove(0);
    active.objective = "objective".into();
    active.tools.clear();
    active.supervisor = Default::default();
    active.assistant_message = None;
    active.status = arany::RunStatus::Active;
    active.agents.truncate(1);
    active.agents[0].provider_calls = vec![arany::ProviderCallRecord {
        phase: arany::AgentPhase::RootPlan,
        disposition: ProviderCallDisposition::Cancelled,
        rate_limits: None,
        delegation: None,
        requested_tools: 0,
        response_id: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        wire_provenance: None,
        failure_reason: None,
        refusal: None,
    }];
    active.display = vec![DisplayChunk {
        call_index: 0,
        phase: arany::AgentPhase::RootPlan,
        offset: 0,
        category: DisplayCategory::Answer,
        item: 0,
        text: source.into(),
    }];
    view.runs.push(active);
    view
}
