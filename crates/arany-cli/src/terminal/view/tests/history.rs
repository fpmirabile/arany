use super::*;

#[test]
fn container_projection_matches_fullscreen_and_pager_frames_without_color() {
    let source = "> - alpha beta gamma delta\n>\n>   ~~~rust\n>   code\n>   ~~~\n\n-";
    for width in [16, 40, 50, 80, 120] {
        for kind in [
            MessageKind::Assistant,
            MessageKind::Preview,
            MessageKind::Incomplete,
        ] {
            let rows = crate::terminal::history::display_rows(kind, source, width, None);
            for pager in [false, true] {
                let mut terminal = Terminal::new(TestBackend::new(width as u16, 24)).unwrap();
                terminal
                    .draw(|frame| {
                        let palette = Palette { color: false };
                        if pager {
                            draw_pager_frame(frame, &rows, "History", palette);
                        } else {
                            draw_history_rows(frame, frame.area(), &rows, palette);
                        }
                    })
                    .unwrap();
                for (index, row) in rows.iter().enumerate() {
                    let expected = if row.heading && kind == MessageKind::Assistant {
                        format!("● {}", row.text)
                    } else {
                        row.text.clone()
                    };
                    assert_eq!(
                        buffer_row(terminal.backend().buffer(), index as u16).trim_end(),
                        expected.trim_end()
                    );
                }
                let screen = (0..23)
                    .map(|row| buffer_row(terminal.backend().buffer(), row))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(screen.contains("│ • alpha"));
                assert!(!screen.contains("~~~"));
            }
        }
    }
}

#[test]
fn narrow_classic_history_cue_keeps_export_action_visible() {
    let width = 20;
    let status = crate::terminal::classic_history_status(width, true, false);
    assert_eq!(status, "arany show");
    let model = PresentationModel {
        status_line: "ready · custom:local/model-1".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Retain,
        empty_chat: false,
        agent_switch: false,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, 8)).expect("narrow terminal");
    terminal
        .draw(|frame| {
            draw_frame_with_history(
                frame,
                FrameContent {
                    model: &model,
                    composer: &Composer::default(),
                    notice: None,
                    hints: ComposerHints::new(Palette { color: false }, "\\ then Enter"),
                    history_rows: &[],
                    history_status: Some(status),
                },
            );
        })
        .expect("narrow history status");
    assert!(buffer_row(terminal.backend().buffer(), 7).contains(status));
}

#[test]
fn fullscreen_history_cue_stays_visible_behind_a_long_state() {
    let status_line = "running · openai gpt-5.1-codex-max-high-reasoning · approval: ask before writes · goal active · model catalog refreshed";
    for width in [80, 120] {
        let mut terminal = Terminal::new(TestBackend::new(width, 6)).expect("test terminal");
        let model = PresentationModel {
            status_line: status_line.into(),
            activity_lines: Vec::new(),
            setup_required: false,
            draft_action: DraftAction::Retain,
            empty_chat: false,
            agent_switch: false,
        };
        terminal
            .draw(|frame| {
                draw_frame_with_history(
                    frame,
                    FrameContent {
                        model: &model,
                        composer: &Composer::default(),
                        notice: None,
                        hints: ComposerHints::new(Palette { color: false }, "\\ then Enter"),
                        history_rows: &[],
                        history_status: Some("History · Ctrl+L live"),
                    },
                );
            })
            .expect("status frame");
        let status = buffer_row(terminal.backend().buffer(), 5);
        let status = status.trim_end();
        assert!(
            status.starts_with("running · openai"),
            "{width}: {status:?}"
        );
        assert!(
            status.ends_with("· History · Ctrl+L live"),
            "{width}: {status:?}"
        );
    }
}

#[test]
fn pausing_status_names_the_step_boundary_and_the_cancel_key() {
    let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("test terminal");
    let model = PresentationModel {
        status_line: "working · model".into(),
        activity_lines: vec!["primary · working".into()],
        setup_required: false,
        draft_action: DraftAction::InspectRun,
        empty_chat: true,
        agent_switch: false,
    };
    let composer = Composer::default();
    terminal
        .draw(|frame| {
            draw_frame(
                frame,
                &model,
                &composer,
                Some("Pausing at the next step... Ctrl+C cancels now"),
                Palette { color: false },
            )
        })
        .expect("pausing frame");
    let buffer = terminal.backend().buffer();
    assert!(
        (0..6)
            .any(|y| buffer_row(buffer, y)
                .contains("Pausing at the next step... Ctrl+C cancels now"))
    );
    assert!((0..6).all(|y| !buffer_row(buffer, y).contains("Run in progress")));
}

#[test]
fn peer_tool_row_frames_at_80_and_120_columns() {
    let tool = crate::presentation::tool_fixture(
        arany::ToolCall::Write {
            path: "test.txt".into(),
            expected_digest: None,
            content: "hello".into(),
        },
        "{}",
    );
    let model = PresentationModel {
        status_line: "ready".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: false,
        agent_switch: false,
    };
    for width in [80, 120] {
        let rows = crate::terminal::history::display_rows(
            MessageKind::Tool,
            &crate::presentation::tool_summary(&tool),
            usize::from(width),
            None,
        );
        for color in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
                .draw(|frame| {
                    draw_frame_with_history(
                        frame,
                        FrameContent {
                            model: &model,
                            composer: &Composer::default(),
                            notice: None,
                            hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                            history_rows: &rows,
                            history_status: None,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer_row(buffer, 1).trim_end(), "⏺ Write(test.txt)");
            assert_eq!(buffer_row(buffer, 2).trim_end(), "  ⎿ applied");
            assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
            let mut limited = crate::presentation::tool_fixture(
                arany::ToolCall::Command {
                    program: "sh".into(),
                    args: vec![],
                    cwd: String::new(),
                    resource_class: None,
                    timeout_ms: None,
                    additional_reads: None,
                },
                "untrusted output",
            );
            let observation = limited.observation.as_mut().unwrap();
            observation.intent.limits.runtime_ms = 120_000;
            observation.disposition = arany::ToolDisposition::Limit;
            observation.limit_cause = Some(arany::LimitCause::Timeout);
            observation.cache_update = arany::CacheUpdate::NotAttempted;
            let rows = crate::terminal::history::display_rows(
                MessageKind::Tool,
                &crate::presentation::tool_summary(&limited),
                usize::from(width),
                None,
            );
            terminal
                .draw(|frame| {
                    draw_frame_with_history(
                        frame,
                        FrameContent {
                            model: &model,
                            composer: &Composer::default(),
                            notice: None,
                            hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                            history_rows: &rows,
                            history_status: None,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer_row(buffer, 1).trim_end(), "⏺ Command(sh )");
            assert_eq!(
                buffer_row(buffer, 2).trim_end(),
                "  ⎿ limit: timed out after 2 min; /settings raises it"
            );
            let output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,
                "stdout":"head\n…[123398 bytes, 12338 lines omitted; 123408 bytes, 12340 lines total]…\ntail\n",
                "stderr":"", "stdout_total":{"bytes":123408,"lines":12340},
                "stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string();
            let command = crate::presentation::tool_fixture(limited.intent.call.clone(), &output);
            let rows = crate::terminal::history::display_rows(
                MessageKind::Tool,
                &crate::presentation::tool_summary(&command),
                usize::from(width),
                None,
            );
            terminal
                .draw(|frame| {
                    draw_frame_with_history(
                        frame,
                        FrameContent {
                            model: &model,
                            composer: &Composer::default(),
                            notice: None,
                            hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                            history_rows: &rows,
                            history_status: None,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer_row(buffer, 1).trim_end(), "⏺ Command(sh )");
            assert_eq!(
                buffer_row(buffer, 2).trim_end(),
                "  ⎿ exit 0 · 12,340 lines (truncated)"
            );
            let long = crate::presentation::tool_fixture(
                arany::ToolCall::Read {
                    path: "界".repeat(70),
                    offset: None,
                    limit: Some(100),
                },
                r#"{"text":"one\ntwo\n"}"#,
            );
            let rows = crate::terminal::history::display_rows(
                MessageKind::Tool,
                &crate::presentation::tool_summary(&long),
                usize::from(width),
                None,
            );
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
                .draw(|frame| {
                    draw_frame_with_history(
                        frame,
                        FrameContent {
                            model: &model,
                            composer: &Composer::default(),
                            notice: None,
                            hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                            history_rows: &rows,
                            history_status: None,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let first = if width == 80 { 36 } else { 56 };
            assert_eq!(
                buffer_row(buffer, 1).replace("界 ", "界").trim_end(),
                format!("⏺ Read({}", "界".repeat(first))
            );
            assert_eq!(
                buffer_row(buffer, 2).replace("界 ", "界").trim_end(),
                format!("{})", "界".repeat(70 - first))
            );
            assert_eq!(buffer_row(buffer, 3).trim_end(), "  ⎿ 2 lines");
        }
    }
}

#[test]
fn command_row_frames_show_first_script_line_and_nonzero_counters() {
    let model = PresentationModel {
        status_line: "ready".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: false,
        agent_switch: false,
    };
    let output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":2,"oversized_inputs":0,"discarded_outputs":1},"exit_code":0,
        "stdout":"x\n","stderr":"","stdout_total":{"bytes":2,"lines":1},
        "stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}})
    .to_string();
    let args = vec!["-c".to_owned(), "\n echo \u{1b}[2J\rhi\nls".to_owned()];
    let calls = [
        (
            arany::ToolCall::Command {
                program: "sh".into(),
                args: args.clone(),
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
                additional_reads: None,
            },
            r"⏺ Command(sh -c  echo \u{001b}[2J\u{000d}hi …)",
        ),
        (
            arany::ToolCall::CommandUnguarded {
                program: "sh".into(),
                args,
                cwd: String::new(),
                justification: "check".into(),
                prefix_rule: None,
            },
            r"⏺ Unguarded command(sh -c  echo \u{001b}[2J\u{000d}hi …)",
        ),
    ];
    for width in [80, 120] {
        for color in [false, true] {
            for (call, expected) in &calls {
                let tool = crate::presentation::tool_fixture(call.clone(), &output);
                let rows = crate::terminal::history::display_rows(
                    MessageKind::Tool,
                    &crate::presentation::tool_summary(&tool),
                    usize::from(width),
                    None,
                );
                let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
                terminal
                    .draw(|frame| {
                        draw_frame_with_history(
                            frame,
                            FrameContent {
                                model: &model,
                                composer: &Composer::default(),
                                notice: None,
                                hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                                history_rows: &rows,
                                history_status: None,
                            },
                        );
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert!(buffer_row(buffer, 1).starts_with(expected));
                assert!(
                    buffer_row(buffer, 2)
                        .trim_end()
                        .starts_with("  ⎿ exit 0 · 1 line · 2 pruned roots, 1 discarded output")
                );
                assert!(!buffer_row(buffer, 2).contains("oversized"));
                assert!(!buffer_row(buffer, 2).contains("ignored"));
                assert!((0..12).all(|y| !buffer_row(buffer, y).contains('\u{1b}')));
            }
        }
    }
}

#[test]
fn selection_highlight_is_inverse_video_on_text_cells_without_color() {
    let rows = [
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "  selected text".into(),
            speaker: MessageKind::Assistant,
            heading: false,
            matched: false,
            selected: Some((2, 10)),
        },
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "  body".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
            selected: Some((2, 6)),
        },
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "Arany:".into(),
            speaker: MessageKind::Assistant,
            heading: true,
            matched: false,
            selected: None,
        },
    ];
    let mut terminal = Terminal::new(TestBackend::new(20, 3)).expect("selection terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_history_rows(frame, area, &rows, Palette { color: false });
        })
        .expect("selection frame");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer_row(buffer, 1), "│ body              ");
    for (y, selected) in [(0, 2..10), (1, 2..6), (2, 0..0)] {
        for x in 0..20 {
            let cell = &buffer[(x, y)];
            assert_eq!(
                cell.modifier.contains(Modifier::REVERSED),
                selected.contains(&x),
                "row {y} column {x}"
            );
            assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset));
        }
    }
}

#[test]
fn pager_frame_gives_history_every_row_above_one_inert_footer() {
    let footer = "Transcript · / find · n/N · j/k · g/G · Ctrl+U/D · [ scrollback · q close";
    for (width, height, color) in [(80, 24, true), (50, 12, false), (16, 8, false)] {
        let mut rows = crate::terminal::history::display_rows(
            MessageKind::Commentary,
            "hostile \u{1b}]52;c;eA==\u{7} tail",
            usize::from(width),
            None,
        );
        rows.extend(crate::terminal::history::display_rows(
            MessageKind::Assistant,
            &"line\n".repeat(usize::from(height)),
            usize::from(width),
            None,
        ));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("pager terminal");
        let mut history_area = Rect::default();
        terminal
            .draw(|frame| {
                history_area = draw_pager_frame(frame, &rows, footer, Palette { color });
            })
            .expect("pager frame");
        assert_eq!(history_area, Rect::new(0, 0, width, height - 1));
        let buffer = terminal.backend().buffer();
        let screen = (0..height)
            .map(|y| buffer_row(buffer, y))
            .collect::<Vec<_>>();
        assert_eq!(
            screen[usize::from(height) - 1].trim_end(),
            safe_truncate(footer, usize::from(width)).trim_end(),
            "{width}x{height}"
        );
        // History fills every row above the footer; the composer and status line are gone.
        for (y, row) in screen[..usize::from(height) - 1].iter().enumerate() {
            let expected = rows[y].text.as_str();
            assert!(
                row.trim_end().ends_with(expected.trim()),
                "{width}x{height} row {y}: {row:?} vs {expected:?}"
            );
        }
        assert!(
            screen.iter().all(|row| !row.contains('\u{1b}')),
            "{screen:#?}"
        );
        if width >= 50 {
            assert!(
                screen
                    .iter()
                    .any(|row| row.contains("hostile \\u{001b}]52"))
            );
        }
    }
}
