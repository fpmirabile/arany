use super::*;
use crate::terminal::{ComposerEdit, TerminalInput};
use arany::Effort;
use ratatui::backend::{Backend, TestBackend};

mod agents;
mod composer;
mod history;
mod permissions;
mod pickers;
mod rewind;
mod setup;
mod usage;

fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

#[test]
fn recognized_slash_tokens_are_accented_and_bold_without_changing_text() {
    let model = PresentationModel {
        status_line: "ready".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: false,
        agent_switch: false,
    };
    for (text, token_len, skill) in [
        ("/ask-matt task", 9, Some("ask-matt")),
        ("/ask-matt literal", 9, None),
        ("/help task", 5, None),
        ("/unknown task", 0, None),
    ] {
        for color in [false, true] {
            let mut composer = Composer::default();
            composer.set_runtime_skills(&["ask-matt".into(), "help".into()]);
            composer.insert_paste(text).unwrap();
            let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                .unwrap();
            let y = terminal.get_cursor_position().unwrap().y;
            let buffer = terminal.backend().buffer();
            assert!(buffer_row(buffer, y).starts_with(&format!("> {text}")));
            for x in 0..text.len() {
                let cell = &buffer[(2 + x as u16, y)];
                assert_eq!(cell.modifier.contains(Modifier::BOLD), x < token_len);
                assert_eq!(
                    cell.fg,
                    if color && x < token_len {
                        Color::Cyan
                    } else {
                        Color::Reset
                    }
                );
            }
            let rows = super::super::history::display_rows(MessageKind::User, text, 40, skill);
            assert_eq!(
                rows[0].text,
                if skill.is_some() {
                    "You (skill):"
                } else {
                    "You:"
                }
            );
            let token_len = if skill.is_some() { token_len } else { 0 };
            for native in [false, true] {
                terminal
                    .draw(|frame| {
                        if native {
                            frame.render_widget(
                                Paragraph::new(
                                    rows.iter()
                                        .map(|row| native_line(row, 40, Palette { color }))
                                        .collect::<Vec<_>>(),
                                ),
                                frame.area(),
                            );
                        } else {
                            draw_history_rows(frame, frame.area(), &rows, Palette { color });
                        }
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                for x in 0..text.len() {
                    let cell = &buffer[(2 + x as u16, 1)];
                    assert_eq!(cell.modifier.contains(Modifier::BOLD), x < token_len);
                    assert_eq!(
                        cell.fg,
                        if color && x < token_len {
                            Color::Cyan
                        } else {
                            Color::Reset
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn classic_dialogs_fill_their_content_sized_viewport() {
    for screen_height in [8, 24, 50] {
        for (panel, height) in [
            ("permission", 12),
            ("secret", 7),
            ("text", 7),
            ("choices", 8),
            ("warning", 8),
            ("rewind", 12),
            ("rewind review", 12),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(80, screen_height)).unwrap();
            // As in Codex, the viewport is min(screen, the dialog's content height).
            let rows = height.min(screen_height);
            let full = Rect::new(0, screen_height - rows, 80, rows);
            let area = crate::terminal::dialog_area(full, height, true);
            let palette = Palette { color: false };
            let lines = vec!["Review this change".to_owned()];
            terminal
                .draw(|frame| match panel {
                    "permission" => {
                        draw_permission_frame(
                            frame,
                            area,
                            &lines,
                            PermissionFrameState {
                                choices: &["Read only", "Trust folder"],
                                page: 0,
                                focus: 0,
                                profile_start: None,
                                profile_count: 0,
                                focused_profile: None,
                                profile_selection_enabled: None,
                                footer: Some("Enter confirms"),
                                palette,
                                boxed: false,
                            },
                        );
                    }
                    "secret" => draw_setup_frame(
                        frame,
                        area,
                        SetupFrame {
                            step: 3,
                            title: "API key",
                            instruction: "Hidden input",
                            input_chars: 0,
                            notice: Some("Enter confirms"),
                            palette,
                        },
                    ),
                    "text" => draw_setup_text_frame(
                        frame,
                        area,
                        SetupTextFrame {
                            title: "Workspace",
                            instruction: "Enter ID",
                            draft: &Composer::default(),
                            notice: None,
                            palette,
                        },
                    ),
                    "choices" => draw_setup_choices_frame(
                        frame,
                        area,
                        SetupChoicesFrame {
                            title: "Account",
                            instruction: "Choose one",
                            choices: &[(1, "Saved"), (2, "New")],
                            selected: 0,
                            notice: None,
                            palette,
                        },
                    ),
                    "warning" => {
                        draw_setup_warning_frame(
                            frame,
                            area,
                            SetupWarningFrame {
                                title: "Consent",
                                warning: "Review this change",
                                page: 0,
                                accept_selected: false,
                                notice: None,
                                palette,
                            },
                        );
                    }
                    "rewind" | "rewind review" => {
                        crate::terminal::rewind::draw_rewind_frame(
                            frame,
                            area,
                            crate::terminal::rewind::RewindFrame {
                                title: "Checkpoints",
                                lines: if panel == "rewind" { &[] } else { &lines },
                                choices: &["Cancel".into(), "Restore".into()],
                                selected: 0,
                                page: 0,
                                palette,
                            },
                        );
                    }
                    _ => unreachable!(),
                })
                .unwrap();
            let rows = (0..screen_height)
                .map(|y| buffer_row(terminal.backend().buffer(), y))
                .collect::<Vec<_>>();
            assert_eq!(
                rows.iter().position(|row| !row.trim().is_empty()),
                Some(usize::from(area.y)),
                "{panel}: {rows:?}"
            );
            assert!(
                !rows[usize::from(area.bottom() - 1)].trim().is_empty(),
                "{panel}: footer stays with content: {rows:?}"
            );
            assert_eq!(area, full, "{panel}: no spare rows around the dialog");
        }
    }
}

#[test]
fn empty_session_has_a_quiet_welcome_without_obscuring_the_composer() {
    let model = PresentationModel {
        status_line: "idle · model".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: true,
        agent_switch: false,
    };
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_frame(
                    frame,
                    &model,
                    &Composer::default(),
                    None,
                    Palette { color: false },
                )
            })
            .expect("idle frame");
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| buffer_row(buffer, y))
            .collect::<Vec<_>>();
        assert!(
            rows.iter().any(|row| row.starts_with("Arany")),
            "{width}: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("/help")),
            "{width}: {rows:?}"
        );
        assert!(rows[usize::from(height - 4)].starts_with("─ Ask Arany"));
        assert!(rows[usize::from(height - 3)].starts_with("> "));
        assert!(rows[usize::from(height - 2)].contains("\\"));
        if width >= 40 {
            assert!(rows[usize::from(height - 2)].contains("Enter sends"));
        }
        assert!(rows[usize::from(height - 1)].starts_with("idle"));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| { cell.fg == Color::Reset && cell.bg == Color::Reset })
        );

        terminal
            .draw(|frame| {
                draw_frame_with_history(
                    frame,
                    FrameContent {
                        model: &model,
                        composer: &Composer::default(),
                        notice: None,
                        hints: ComposerHints::new(Palette { color: false }, "\\ then Enter"),
                        history_rows: &[HistoryRow {
                            spans: Vec::new(),
                            slash_cells: 0,
                            text: "You:".into(),
                            speaker: MessageKind::User,
                            heading: true,
                            matched: false,
                            selected: None,
                        }],
                        history_status: None,
                    },
                );
            })
            .expect("history frame");
        let buffer = terminal.backend().buffer();
        let screen = (0..height)
            .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        assert!(screen.contains("You:"));
        assert!(screen.contains("› You:"));
        assert!(!screen.contains("Describe a task"));
        assert!(!screen.contains("Describe a task"));
    }
}

#[test]
fn unconfigured_session_draws_typed_draft_in_the_composer() {
    let model = PresentationModel {
        status_line: "setup · /setup".into(),
        activity_lines: Vec::new(),
        setup_required: true,
        draft_action: DraftAction::Submit,
        empty_chat: true,
        agent_switch: false,
    };
    let mut composer = Composer::default();
    for character in "visible draft".chars() {
        composer.apply(TerminalInput::Character(character));
    }
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("composer frame");
        let buffer = terminal.backend().buffer();
        let line = (0..width)
            .map(|x| buffer[(x, height - 3)].symbol())
            .collect::<String>();
        assert!(line.starts_with("> visible draft"), "{width}: {line}");
        let screen = (0..height)
            .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        assert!(screen.contains("Type /setup"), "{width}: {screen}");
        assert!(!screen.contains("Describe a task"), "{width}: {screen}");
        let status = (0..width)
            .map(|x| buffer[(x, height - 1)].symbol())
            .collect::<String>();
        assert!(status.starts_with("setup"), "{width}: {status}");
    }
}

#[test]
fn classic_content_height_keeps_bottom_controls_and_completion_visible() {
    for height in [8, 12, 24, 50] {
        for width in [16, 50, 80, 120] {
            let screen = ratatui::layout::Rect::new(0, 0, width, height);
            let model = PresentationModel {
                status_line: "idle".into(),
                activity_lines: Vec::new(),
                setup_required: false,
                draft_action: DraftAction::Submit,
                empty_chat: false,
                agent_switch: false,
            };
            let mut composer = Composer::default();
            for (draft, expected) in [
                ("", 4),
                ("/", height.min(26)),
                ("e\u{301}\n界\n👩‍💻\nlast\nclipped", 7),
                ("", 4),
            ] {
                composer = Composer::default();
                composer.insert_paste(draft).unwrap();
                let needed = classic_height(&model, &composer, screen, 0);
                assert_eq!(needed, expected, "{width}x{height}: {draft:?}");
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::with_options(
                    backend,
                    ratatui::TerminalOptions {
                        viewport: ratatui::Viewport::Fixed(bottom_area(screen, needed)),
                    },
                )
                .unwrap();
                terminal
                    .draw(|frame| {
                        draw_frame(frame, &model, &composer, None, Palette { color: false })
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert!(!buffer_row(buffer, height - 1).trim().is_empty());
                let rows = (height - needed..height)
                    .map(|y| buffer_row(buffer, y))
                    .collect::<Vec<_>>();
                assert!(rows.iter().any(|row| row.contains("> ")));
                if draft == "/" {
                    assert!(
                        rows.iter()
                            .any(|row| row.contains("[Cmd]") || row.contains("[Commands]"))
                    );
                }
            }
            let mut active = model;
            active.activity_lines = vec!["primary · working".into()];
            assert_eq!(classic_height(&active, &composer, screen, 0), 5);
            assert_eq!(classic_height(&active, &composer, screen, 100), height);
        }
    }
}

#[test]
fn classic_height_sizes_the_completion_menu_from_the_rows_the_draft_leaves() {
    let screen = ratatui::layout::Rect::new(0, 0, 80, 5);
    let model = PresentationModel {
        status_line: "idle".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: false,
        agent_switch: false,
    };
    let mut composer = Composer::default();
    composer.insert_paste("/").unwrap();
    let draft = composer_height(&composer, screen);
    assert_eq!(completion_height(&composer, screen.height - 1 - draft), 0);

    let height = classic_height(&model, &composer, screen, 0);

    assert_eq!(
        height,
        draft + 1,
        "no row is reserved for a menu the draw cannot show"
    );
}

#[test]
fn inline_frame_keeps_composer_and_status_in_the_viewport() {
    for width in [40, 50, 79, 80, 120] {
        let backend = TestBackend::new(width, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut composer = Composer::default();
        for character in "hello\u{202e}[31m".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let model = PresentationModel {
            status_line: "idle · safe Session".into(),
            activity_lines: vec!["primary · working".into()],
            setup_required: false,
            draft_action: DraftAction::InspectRun,
            empty_chat: true,
            agent_switch: false,
        };
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("frame");
        let buffer = terminal.backend().buffer();
        let row = |y| buffer_row(buffer, y);
        assert!(row(0).contains("primary · working"));
        assert!(row(1).contains("Ask Arany"));
        assert!(row(2).contains("hello\\u{202e}[31m"));
        assert!(row(4).contains("idle · safe Session"));
        for y in 0..5 {
            assert!(!row(y).contains('\u{1b}'));
        }
        assert!(buffer.content().iter().all(|cell| {
            cell.fg == ratatui::style::Color::Reset && cell.bg == ratatui::style::Color::Reset
        }));
        terminal
            .draw(|frame| {
                draw_frame(
                    frame,
                    &model,
                    &composer,
                    Some("Error: retry"),
                    Palette { color: false },
                )
            })
            .expect("error frame");
        let buffer = terminal.backend().buffer();
        let row = |y| buffer_row(buffer, y);
        assert!(row(2).contains("hello\\u{202e}[31m"));
        assert!(row(4).contains("Error: retry"));
        terminal
            .draw(|frame| {
                draw_frame(
                    frame,
                    &model,
                    &composer,
                    Some("Tool queued · waiting for capacity · Ctrl+C cancels"),
                    Palette { color: false },
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer_row(buffer, 4).contains("Tool queued"));
        assert!(buffer_row(buffer, 4).contains("waiting for capacity"));
        assert!(buffer_row(buffer, 2).contains("hello\\u{202e}[31m"));
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 5)).expect("test terminal");
    let model = PresentationModel {
        status_line: "working · openai/model".into(),
        activity_lines: vec!["primary · working".into()],
        setup_required: false,
        draft_action: DraftAction::InspectRun,
        empty_chat: true,
        agent_switch: false,
    };
    terminal
        .draw(|frame| {
            draw_frame(
                frame,
                &model,
                &Composer::default(),
                None,
                Palette { color: true },
            )
        })
        .expect("colored frame");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
    assert_eq!(buffer[(0, 4)].fg, Color::Cyan);
    assert_eq!(buffer[(0, 2)].symbol(), ">");
    assert_eq!(buffer[(0, 2)].fg, Color::Cyan);
    assert_eq!(buffer[(2, 2)].fg, Color::Reset);
    terminal
        .draw(|frame| {
            draw_frame(
                frame,
                &model,
                &Composer::default(),
                Some("Error: retry"),
                Palette { color: true },
            )
        })
        .expect("colored error frame");
    assert_eq!(terminal.backend().buffer()[(0, 4)].fg, Color::Red);

    for (text, command) in [("typed draft", false), ("/status", true)] {
        for width in [16, 24, 40, 80] {
            for draft_action in [
                DraftAction::Submit,
                DraftAction::InspectRun,
                DraftAction::Retain,
            ] {
                for color in [false, true] {
                    let model = PresentationModel {
                        status_line: "state".into(),
                        activity_lines: Vec::new(),
                        setup_required: false,
                        draft_action,
                        empty_chat: true,
                        agent_switch: false,
                    };
                    let mut composer = Composer::default();
                    for character in text.chars() {
                        composer.apply(TerminalInput::Character(character));
                    }
                    composer.apply(TerminalInput::Left);
                    let cursor = composer.cursor_byte_offset();
                    let mut terminal =
                        Terminal::new(TestBackend::new(width, 8)).expect("hint terminal");
                    for notice in [None, Some("Error: correct the draft")] {
                        terminal
                            .draw(|frame| {
                                draw_frame(frame, &model, &composer, notice, Palette { color })
                            })
                            .expect("input action frame");
                        let buffer = terminal.backend().buffer();
                        let footer = buffer_row(buffer, 6);
                        let keeps_draft = draft_action != DraftAction::Submit;
                        let command = command && draft_action != DraftAction::Retain;
                        let action = if command {
                            "Enter command"
                        } else if keeps_draft && width >= 40 {
                            "Enter keeps draft"
                        } else if keeps_draft {
                            "Enter keeps"
                        } else if width >= 40 {
                            "Enter sends"
                        } else {
                            "Enter · \\ Enter"
                        };
                        assert!(
                            footer.contains(action),
                            "{width}, {draft_action:?}: {footer}"
                        );
                        assert!(!keeps_draft || !footer.contains("sends"));
                        if width >= 40 || (!command && (width >= 24 || !keeps_draft)) {
                            assert!(footer.contains("\\"));
                        }
                        assert_eq!(composer.text(), text);
                        assert_eq!(composer.cursor_byte_offset(), cursor);
                        if !color {
                            assert!(buffer.content().iter().all(|cell| cell.fg == Color::Reset));
                        }
                    }
                    if draft_action == DraftAction::Retain {
                        composer.apply(TerminalInput::End);
                        terminal
                            .draw(|frame| {
                                draw_frame(frame, &model, &composer, None, Palette { color })
                            })
                            .expect("busy draft-end frame");
                        let buffer = terminal.backend().buffer();
                        let status = buffer_row(buffer, 7);
                        assert!(
                            status.contains("state"),
                            "busy draft never advertises command submission"
                        );
                        assert_eq!(composer.text(), text);
                        assert_eq!(composer.cursor_byte_offset(), text.len());
                        composer.set_runtime_skills(&["ask-matt".into()]);
                        for draft in ["/s", "/ask", "/ask-matt", "/ask-matt task"] {
                            composer.clear();
                            composer.insert_paste(draft).unwrap();
                            terminal
                                .draw(|frame| {
                                    draw_frame(frame, &model, &composer, None, Palette { color })
                                })
                                .expect("busy completion frame");
                            let buffer = terminal.backend().buffer();
                            assert!(
                                (0..8).any(|y| buffer_row(buffer, y).contains("Enter keeps")),
                                "busy completion does not imply execution: {draft}"
                            );
                            assert!(buffer_row(buffer, 7).contains("state"));
                            for y in 0..8 {
                                let row = buffer_row(buffer, y);
                                assert!(!row.contains("Enter invokes"), "{draft}: {row}");
                                assert!(!row.contains("Enter submits"), "{draft}: {row}");
                            }
                            assert_eq!(composer.text(), draft);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn full_height_history_keeps_chat_above_activity_and_the_bottom_composer() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test terminal");
    let model = PresentationModel {
        status_line: "working · model".into(),
        activity_lines: vec!["primary · working".into()],
        setup_required: false,
        draft_action: DraftAction::InspectRun,
        empty_chat: true,
        agent_switch: false,
    };
    let rows = vec![
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "You:".into(),
            speaker: MessageKind::User,
            heading: true,
            matched: false,
            selected: None,
        },
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "  committed objective".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
            selected: None,
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
    terminal
        .draw(|frame| {
            draw_frame_with_history(
                frame,
                FrameContent {
                    model: &model,
                    composer: &Composer::default(),
                    notice: None,
                    hints: ComposerHints::new(Palette { color: false }, "\\ then Enter"),
                    history_rows: &rows,
                    history_status: Some("History · Ctrl+L live"),
                },
            );
        })
        .expect("history frame");
    let buffer = terminal.backend().buffer();
    let row = |y| buffer_row(buffer, y);
    assert!(
        row(0).trim().is_empty(),
        "top edge stays clear of speaker labels"
    );
    assert!(row(1).contains("You:"));
    assert!(row(3).contains("Arany:"));
    assert_eq!(row(1), format!("› You:{}", " ".repeat(34)));
    assert_eq!(row(3), format!("● Arany:{}", " ".repeat(32)));
    assert!(row(2).starts_with("│ committed objective"));
    assert!(row(7).contains("primary · working"));
    assert!(row(8).contains("Ask Arany"));
    assert!(row(11).contains("History · Ctrl+L live"));
    assert!(!row(11).contains("working · model"));
    assert!(buffer[(2, 1)].modifier.contains(Modifier::BOLD));
    assert!(buffer[(2, 3)].modifier.contains(Modifier::BOLD));
    assert!(!buffer[(2, 2)].modifier.contains(Modifier::BOLD));
    assert_eq!(buffer[(2, 1)].fg, Color::Reset);
    assert_eq!(buffer[(0, 3)].fg, Color::Reset);

    let wide_model = PresentationModel {
        status_line: "working · openai/model · Safe conversation".into(),
        activity_lines: vec!["primary · working".into()],
        setup_required: false,
        draft_action: DraftAction::InspectRun,
        empty_chat: true,
        agent_switch: false,
    };
    for color in [false, true] {
        let mut wide = Terminal::new(TestBackend::new(80, 12)).expect("wide terminal");
        let mut feedback_rows = rows.clone();
        feedback_rows.push(HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "Arany · error:".into(),
            speaker: MessageKind::Error,
            heading: true,
            matched: false,
            selected: None,
        });
        feedback_rows.push(HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "  Retry /setup; your draft is retained".into(),
            speaker: MessageKind::Error,
            heading: false,
            matched: false,
            selected: None,
        });
        wide.draw(|frame| {
            draw_frame_with_history(
                frame,
                FrameContent {
                    model: &wide_model,
                    composer: &Composer::default(),
                    notice: None,
                    hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                    history_rows: &rows,
                    history_status: Some("History · Ctrl+L live"),
                },
            );
        })
        .expect("wide history frame");
        let status = (0..80)
            .map(|x| wide.backend().buffer()[(x, 11)].symbol())
            .collect::<String>();
        assert!(
            status.contains("working · openai/model · Safe conversation · History · Ctrl+L live"),
            "{status}"
        );

        wide.draw(|frame| {
            draw_frame_with_history(
                frame,
                FrameContent {
                    model: &wide_model,
                    composer: &Composer::default(),
                    notice: None,
                    hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                    history_rows: &feedback_rows,
                    history_status: Some("History · Ctrl+L live"),
                },
            );
        })
        .expect("wide error frame");
        let status = (0..80)
            .map(|x| wide.backend().buffer()[(x, 11)].symbol())
            .collect::<String>();
        assert!(
            status
                .starts_with("working · openai/model · Safe conversation · History · Ctrl+L live"),
            "{status}"
        );
        let buffer = wide.backend().buffer();
        let chat_error = buffer_row(buffer, 4);
        let chat_body = buffer_row(buffer, 5);
        assert!(chat_error.starts_with("Arany · error:"));
        assert!(chat_body.contains("Retry /setup; your draft is retained"));
        assert_eq!(
            buffer[(0, 4)].fg,
            if color { Color::Red } else { Color::Reset }
        );
    }

    terminal
        .draw(|frame| {
            draw_frame_with_history(
                frame,
                FrameContent {
                    model: &model,
                    composer: &Composer::default(),
                    notice: None,
                    hints: ComposerHints::new(Palette { color: true }, "\\ then Enter"),
                    history_rows: &rows,
                    history_status: None,
                },
            );
        })
        .expect("colored history frame");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(2, 1)].fg, Color::Reset);
    assert_eq!(buffer[(0, 3)].fg, Color::Cyan);
    assert!(buffer[(2, 1)].modifier.contains(Modifier::BOLD));
    assert!(buffer[(2, 3)].modifier.contains(Modifier::BOLD));

    for (height, top) in [(8, 0), (9, 0), (10, 1)] {
        for color in [false, true] {
            let mut narrow = Terminal::new(TestBackend::new(16, height)).expect("narrow terminal");
            narrow
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
                .expect("narrow history frame");
            let buffer = narrow.backend().buffer();
            assert_eq!(buffer[(0, top)].symbol(), "›");
            assert_eq!(buffer[(0, top + 2)].symbol(), "●");
            assert_eq!(buffer[(5, top)].symbol(), ":");
            assert_eq!(buffer[(7, top + 2)].symbol(), ":");
            assert_eq!(buffer[(15, top)].symbol(), " ");
            assert_eq!(buffer[(15, top + 2)].symbol(), " ");
            assert!(buffer[(2, top)].modifier.contains(Modifier::BOLD));
            assert!(buffer[(2, top + 2)].modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[(2, top)].fg, Color::Reset);
            assert_eq!(
                buffer[(0, top + 2)].fg,
                if color { Color::Cyan } else { Color::Reset }
            );
        }
    }

    let mut conversation = rows[..2].to_vec();
    conversation[1].text = "  e\u{301}👩‍💻".into();
    conversation.extend([
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "  continued".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: true,
            selected: None,
        },
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: String::new(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
            selected: None,
        },
        rows[2].clone(),
        HistoryRow {
            spans: Vec::new(),
            slash_cells: 0,
            text: "  You: is quoted".into(),
            speaker: MessageKind::Assistant,
            heading: false,
            matched: false,
            selected: None,
        },
    ]);
    for width in [16, 40, 80] {
        for color in [false, true] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, 12)).expect("conversation terminal");
            terminal
                .draw(|frame| {
                    draw_frame_with_history(
                        frame,
                        FrameContent {
                            model: &model,
                            composer: &Composer::default(),
                            notice: None,
                            hints: ComposerHints::new(Palette { color }, "\\ then Enter"),
                            history_rows: &conversation,
                            history_status: None,
                        },
                    );
                })
                .expect("conversation frame");
            let buffer = terminal.backend().buffer();
            let row = |y| buffer_row(buffer, y);
            assert!(row(0).trim().is_empty());
            assert!(row(1).starts_with("› You:"));
            assert!(row(2).starts_with("│ e\u{301}👩‍💻"));
            assert!(row(3).starts_with("│ continued"));
            assert!(row(4).trim().is_empty());
            assert!(row(5).starts_with("● Arany:"));
            assert!(row(6).starts_with("  You:"));
            assert_eq!(
                buffer[(0, 2)].fg,
                if color { Color::Cyan } else { Color::Reset }
            );
            assert_eq!(buffer[(2, 2)].fg, Color::Reset);
            assert!(buffer[(2, 2)].modifier.is_empty());
            assert!(buffer[(2, 3)].modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[(2, 6)].fg, Color::Reset);
            assert!(buffer[(2, 6)].modifier.is_empty());
            assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
        }
    }
}

#[test]
fn multiline_frame_keeps_activity_draft_caret_and_status_in_order() {
    for width in [40, 50, 79, 80, 120] {
        let mut terminal = Terminal::new(TestBackend::new(width, 7)).expect("test terminal");
        let mut composer = Composer::default();
        for character in "top".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        composer.apply(TerminalInput::Newline);
        for character in "中e\u{301}\u{202e}".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        composer.apply(TerminalInput::Newline);
        for character in "last".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let model = PresentationModel {
            status_line: "working · model".into(),
            activity_lines: vec!["primary · working".into()],
            setup_required: false,
            draft_action: DraftAction::InspectRun,
            empty_chat: true,
            agent_switch: false,
        };
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("multiline frame");
        let buffer = terminal.backend().buffer();
        let row = |y| buffer_row(buffer, y);
        assert!(row(0).contains("primary · working"));
        assert!(row(1).contains("Ask Arany"));
        assert!(row(2).contains("top"));
        assert_eq!(buffer[(2, 3)].symbol(), "中");
        assert_eq!(buffer[(4, 3)].symbol(), "e\u{301}");
        assert!(row(3).contains("\\u{202e}"));
        assert!(row(4).contains("last"));
        assert!(row(5).contains("Enter keeps draft"));
        assert!(row(5).contains("\\ then Enter newline"));
        assert!(row(6).contains("working · model"));
        assert!(buffer.content().iter().all(|cell| {
            cell.fg == ratatui::style::Color::Reset && cell.bg == ratatui::style::Color::Reset
        }));
        let cursor = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("cursor");
        assert_eq!((cursor.x, cursor.y), (6, 4));
    }

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("short test terminal");
    let mut composer = Composer::default();
    for index in 0..6 {
        if index > 0 {
            composer.apply(TerminalInput::Newline);
        }
        for character in format!("line{index}").chars() {
            composer.apply(TerminalInput::Character(character));
        }
    }
    let model = PresentationModel {
        status_line: "working".into(),
        activity_lines: vec!["primary · working".into(), "child 1 · waiting".into()],
        setup_required: false,
        draft_action: DraftAction::InspectRun,
        empty_chat: true,
        agent_switch: false,
    };
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("short multiline frame");
    let buffer = terminal.backend().buffer();
    let row = |y| buffer_row(buffer, y);
    assert!(row(0).contains("primary · working"));
    assert!(row(1).contains("Ask Arany"));
    assert!(row(1).contains("rows 3-6/6"));
    assert!(row(2).contains("line2"));
    assert!(row(5).contains("line5"));
    assert!(row(7).contains("working"));
    let cursor = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor");
    assert_eq!((cursor.x, cursor.y), (7, 5));

    for (moves, expected_range) in [(0, "rows 1-4/6"), (3, "rows 2-5/6")] {
        composer.apply(TerminalInput::Home);
        for _ in 0..moves {
            composer.apply(TerminalInput::Down);
        }
        let cursor = composer.cursor_byte_offset();
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("draft window position");
        let buffer = terminal.backend().buffer();
        let heading = buffer_row(buffer, 1);
        assert!(heading.contains(expected_range), "{heading}");
        assert_eq!(composer.cursor_byte_offset(), cursor);
        assert_eq!(composer.text(), "line0\nline1\nline2\nline3\nline4\nline5");
    }

    composer.apply(TerminalInput::End);
    for _ in 0..arany::MAX_MESSAGE_IMAGES {
        composer
            .attach_image(arany::test_support::test_image())
            .unwrap();
    }
    for width in [16, 28, 32, 40, 80] {
        for color in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                .unwrap();
            let heading = buffer_row(terminal.backend().buffer(), 1);
            assert!(heading.contains(">[Image #4]"), "{width}: {heading}");
            assert!(
                heading.contains(if width >= 40 { "rows 3-6/6" } else { "6/6" }),
                "{width}: {heading}"
            );
        }
    }

    composer.clear();
    for _ in 0..8 * 1024 {
        assert_eq!(
            composer.apply(TerminalInput::Newline),
            ComposerEdit::Changed
        );
    }
    let mut narrow = Terminal::new(TestBackend::new(16, 8)).expect("bounded draft terminal");
    narrow
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("maximum row-count frame");
    let heading = (0..16)
        .map(|x| narrow.backend().buffer()[(x, 1)].symbol())
        .collect::<String>();
    assert!(heading.contains("Draft 8193/8193"), "{heading}");
    assert_eq!(composer.text().len(), 8 * 1024);
    assert_eq!(composer.cursor_byte_offset(), 8 * 1024);

    composer.clear();
    composer.insert_paste("draft中").expect("retained draft");
    let caret = composer.cursor_byte_offset();
    for width in [16, 40, 80] {
        for color in [false, true] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, 8)).expect("preparing terminal");
            let preparing = PresentationModel {
                status_line: safe_truncate("preparing · model", width as usize),
                activity_lines: vec![safe_truncate(
                    "Preparing message · Ctrl+C cancels",
                    width as usize,
                )],
                setup_required: false,
                draft_action: DraftAction::Retain,
                empty_chat: true,
                agent_switch: false,
            };
            terminal
                .draw(|frame| draw_frame(frame, &preparing, &composer, None, Palette { color }))
                .expect("preparing frame");
            let buffer = terminal.backend().buffer();
            let row = |y| buffer_row(buffer, y);
            assert!(row(3).starts_with("Preparing"));
            assert!(row(4).contains("Ask Arany"));
            assert!(row(5).contains("draft中"));
            assert!(row(6).contains("Enter keeps"));
            assert!(row(7).starts_with("preparing"));
            if !color {
                assert!(
                    buffer
                        .content()
                        .iter()
                        .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
                );
            }
            let cursor = terminal
                .backend_mut()
                .get_cursor_position()
                .expect("preparing caret");
            assert_eq!((cursor.x, cursor.y), (9, 5));
            assert_eq!(composer.cursor_byte_offset(), caret);
            assert_eq!(composer.text(), "draft中");
        }
    }
}

#[test]
fn inline_dialogs_number_rows_at_the_bottom_and_focus_only_once_armed() {
    let items = ["First", "Second", "Third"]
        .into_iter()
        .map(|title| SessionListItem {
            id: arany::SessionId::new(),
            title: title.into(),
            last_sequence: 1,
            created_at: "2026-10-06 12:00:00".into(),
            last_activity_at: "2026-10-06 12:05:00".into(),
            defaults: arany::SessionDefaults::default(),
        })
        .collect::<Vec<_>>();
    let settings = vec![
        "Slash menu: tabs".to_owned(),
        "Chat view: classic".to_owned(),
        "Tokens per task: unlimited".to_owned(),
        "Build cache: 8.0 GB".to_owned(),
    ];
    let agents = inspector_model(
        Some(1),
        vec![
            "Run 1: primary; finished".into(),
            "Run 1: child 1; Failed".into(),
        ],
        vec![
            "Last Run: 2 agents; children: 1".into(),
            "Agent 2/2: child 1; Failed".into(),
        ],
    );
    for width in [80, 50, 16] {
        for height in [8, 24] {
            for armed in [false, true] {
                let cases: [(&str, u16, &str); 5] = [
                    ("quick actions", 6, "› 2. "),
                    ("resume", 9, "› 1. "),
                    ("help", 20, "› 1. /help"),
                    (
                        "settings",
                        settings_height(&settings, width, SELECTOR_ROWS),
                        "› 4. ",
                    ),
                    ("agents", 8, "› 2. "),
                ];
                for (name, dialog_height, focused) in cases {
                    let mut terminal =
                        Terminal::new(TestBackend::new(width, height)).expect("dialog terminal");
                    let mut area = Rect::default();
                    terminal
                        .draw(|frame| {
                            frame.render_widget(
                                Paragraph::new("history ".repeat(400)).wrap(Wrap { trim: false }),
                                frame.area(),
                            );
                            area = bottom_area(frame.area(), dialog_height);
                            let palette = Palette { color: false };
                            match name {
                                "quick actions" => {
                                    draw_quick_actions_frame(
                                        frame,
                                        area,
                                        QuickActionsFrame {
                                            selected: 1,
                                            armed,
                                            has_draft: false,
                                            palette,
                                        },
                                    );
                                }
                                "resume" => {
                                    draw_session_picker_frame(
                                        frame,
                                        area,
                                        SessionPickerFrame {
                                            items: &items,
                                            selected: 0,
                                            armed,
                                            palette,
                                        },
                                    );
                                }
                                "help" => draw_help_frame(frame, area, 0, armed, palette),
                                "settings" => draw_settings_frame(
                                    frame,
                                    area,
                                    SettingsFrame {
                                        title: "Settings",
                                        rows: &settings,
                                        selected: 3,
                                        armed,
                                        notice: None,
                                        palette,
                                    },
                                ),
                                _ => draw_agent_inspector_frame(
                                    frame,
                                    area,
                                    AgentInspectorFrame {
                                        model: &agents,
                                        scroll: 0,
                                        armed,
                                        notice: None,
                                        palette,
                                    },
                                ),
                            }
                            if name == "settings" && width == 80 && height == 24 && armed {
                                crate::terminal::PanelSelection {
                                    area: Some(area),
                                    anchor: Some((4, 2)),
                                    head: Some((4, 3)),
                                    ..crate::terminal::PanelSelection::default()
                                }
                                .render(frame, area);
                                crate::terminal::draw_panel_feedback(
                                    frame,
                                    area,
                                    "Copy unavailable: clipboard unavailable",
                                    palette,
                                );
                            }
                        })
                        .expect("dialog frame");
                    let buffer = terminal.backend().buffer();
                    let rows = (0..height)
                        .map(|y| buffer_row(buffer, y))
                        .collect::<Vec<_>>();
                    let context = format!("{name} armed {armed} at {width}x{height}: {rows:?}");
                    assert_eq!(area.bottom(), height, "{context}");
                    if name == "settings" && width == 80 && height == 24 && armed {
                        let buffer = terminal.backend().buffer();
                        let focused_selection = &buffer[(area.x + 2, area.y + 4)];
                        assert!(focused_selection.modifier.contains(Modifier::REVERSED));
                        assert!(focused_selection.modifier.contains(Modifier::UNDERLINED));
                        assert_eq!(
                            rows[usize::from(height - 1)].trim_end(),
                            "Copy unavailable: clipboard unavailable"
                        );
                    }
                    // The key hint is the arming cue.
                    assert_eq!(
                        rows[usize::from(height - 1)].contains("Esc"),
                        armed && !(name == "settings" && width == 80 && height == 24),
                        "{context}"
                    );
                    let dialog = &rows[usize::from(area.y)..];
                    assert!(
                        dialog.iter().all(|row| !row.contains("history")),
                        "the dialog clears its own rows: {context}"
                    );
                    let numbered = (area.y..height)
                        .filter(|y| {
                            rows[usize::from(*y)]
                                .chars()
                                .skip(2)
                                .collect::<String>()
                                .split_once(". ")
                                .is_some_and(|(number, _)| {
                                    !number.is_empty()
                                        && number.bytes().all(|byte| byte.is_ascii_digit())
                                })
                        })
                        .collect::<Vec<_>>();
                    assert!(numbered.len() >= 2, "numbered rows in {context}");
                    if armed {
                        let focus = (area.y..height)
                            .find(|y| rows[usize::from(*y)].starts_with(focused))
                            .unwrap_or_else(|| panic!("focused row missing in {context}"));
                        assert!(
                            buffer[(0, focus)].modifier.contains(Modifier::REVERSED),
                            "{context}"
                        );
                    } else {
                        assert!(
                            dialog.iter().all(|row| !row.starts_with('›')),
                            "an unarmed selector shows no focus: {context}"
                        );
                        for y in numbered {
                            assert!(buffer[(2, y)].modifier.contains(Modifier::DIM), "{context}");
                        }
                    }
                    if name == "settings" {
                        // A visible value is whole, on its own row when the label is wide.
                        for value in ["tabs", "classic", "unlimited", "8.0 GB"] {
                            if dialog.iter().any(|row| row.contains(&value[..3])) {
                                assert!(
                                    dialog.iter().any(|row| row.contains(value)),
                                    "{value} clipped in {context}"
                                );
                            }
                        }
                        assert!(
                            dialog.iter().any(|row| row.contains("8.0 GB")),
                            "focused value visible in {context}"
                        );
                    }
                    assert!(
                        buffer
                            .content()
                            .iter()
                            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
                    );
                }
            }
        }
    }
}

#[test]
fn model_catalog_keeps_selected_row_visible_and_hostile_ids_inert() {
    let items = (0..4096)
        .map(|index| ModelEntry {
            id: if index == 4095 {
                "bad\u{1b}[31m\u{202e}".into()
            } else {
                format!("same-long-prefix-model-{index}")
            },
            context_window_tokens: None,
            runnable: index == 1,
            efforts: Vec::new(),
        })
        .collect::<Vec<_>>();
    let efforts = vec![Some(Effort::Low); items.len()];
    let model = PresentationModel {
        status_line: "idle · current".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: true,
        agent_switch: false,
    };
    for width in [16, 20, 24, 40, 50, 79, 80, 120] {
        for height in [8, 12, 24] {
            for (color, catalog_state) in [
                (false, crate::terminal::ModelCatalogState::Current),
                (true, crate::terminal::ModelCatalogState::Current),
                (false, crate::terminal::ModelCatalogState::Refreshing),
                (true, crate::terminal::ModelCatalogState::Refreshing),
                (false, crate::terminal::ModelCatalogState::Updated),
                (true, crate::terminal::ModelCatalogState::Updated),
                (false, crate::terminal::ModelCatalogState::RefreshFailed),
                (true, crate::terminal::ModelCatalogState::RefreshFailed),
            ] {
                for (text, filter) in [
                    ("draft é好", ""),
                    ("one\ntwo\nthree\nfour", ""),
                    ("draft é好", "model"),
                ] {
                    let mut composer = Composer::default();
                    for character in text.chars() {
                        composer.apply(if character == '\n' {
                            TerminalInput::Newline
                        } else {
                            TerminalInput::Character(character)
                        });
                    }
                    let cursor = composer.cursor_byte_offset();
                    let mut terminal =
                        Terminal::new(TestBackend::new(width, height)).expect("test terminal");
                    terminal
                        .draw(|frame| {
                            draw_frame(frame, &model, &composer, None, Palette { color });
                            frame.render_widget(
                                Paragraph::new("Earlier chat"),
                                Rect::new(0, 0, width, 1),
                            );
                            draw_model_catalog_frame(
                                frame,
                                ModelCatalogFrame {
                                    picker: &crate::terminal::ModelPicker {
                                        items: &items,
                                        efforts: &efforts,
                                        selected: 4095,
                                        exact_custom: false,
                                        filter,
                                        invalid: false,
                                        catalog_state,
                                        refresh_error: Some("Provider unavailable"),
                                    },
                                    armed: true,
                                    composer: &composer,
                                    model: &model,
                                    hints: ComposerHints::new(
                                        Palette { color },
                                        composer.newline_hint(KeyboardSupport::Legacy),
                                    ),
                                },
                            );
                        })
                        .expect("chat model panel");
                    let buffer = terminal.backend().buffer();
                    let rows = (0..height)
                        .map(|y| buffer_row(buffer, y))
                        .collect::<Vec<_>>();
                    let focused = rows
                        .iter()
                        .find(|row| row.starts_with("› 4096. "))
                        .expect("focused last row");
                    assert!(focused.contains("<low>"), "{width}x{height}: {rows:?}");
                    assert!(!focused.contains('\u{1b}') && !focused.contains('\u{202e}'));
                    assert!(rows.iter().any(|row| row.contains(if filter.is_empty() {
                        "Models · listed"
                    } else {
                        "Find model"
                    })));
                    let status = match catalog_state {
                        crate::terminal::ModelCatalogState::Current => None,
                        crate::terminal::ModelCatalogState::Refreshing => Some("Refreshing"),
                        crate::terminal::ModelCatalogState::Updated if width >= 50 => {
                            Some("Model list refreshed")
                        }
                        crate::terminal::ModelCatalogState::Updated => Some("Refreshed"),
                        crate::terminal::ModelCatalogState::RefreshFailed if width >= 50 => {
                            Some("Refresh failed: Provider unavailable")
                        }
                        crate::terminal::ModelCatalogState::RefreshFailed => Some("Refresh failed"),
                    };
                    assert_eq!(
                        rows.iter().any(|row| row.contains("Ctrl+R refresh")),
                        width >= 60,
                        "{width}x{height}: {rows:?}"
                    );
                    if let Some(status) = status {
                        assert!(
                            rows.last().unwrap().contains(status),
                            "{width}x{height}: {rows:?}"
                        );
                    }
                    assert!(
                        rows.iter().any(|row| row.contains("L/R effort")
                            || row.contains("Left/Right reasoning"))
                    );
                    assert!(
                        rows.iter()
                            .any(|row| row.contains("Enter") && row.contains("Esc"))
                    );
                    assert!(rows.iter().any(|row| row.contains("Draft kept")));
                    assert!(rows.iter().any(|row| row.contains(if text.contains('\n') {
                        "four"
                    } else {
                        "> draft é好"
                    })));
                    if height == 24 {
                        assert!(rows[0].contains("Earlier chat"));
                    }
                    if !color {
                        assert!(
                            buffer
                                .content()
                                .iter()
                                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
                        );
                    }
                    assert_eq!(composer.text(), text);
                    assert_eq!(composer.cursor_byte_offset(), cursor);
                    let position = terminal
                        .backend_mut()
                        .get_cursor_position()
                        .expect("caret position");
                    assert!(position.x < width && position.y < height);
                }
            }
        }
    }
}

fn inspector_model(
    selected: Option<usize>,
    choices: Vec<String>,
    lines: Vec<String>,
) -> AgentInspectorModel {
    AgentInspectorModel {
        selected,
        rows: choices
            .iter()
            .map(|_| {
                crate::presentation::AgentRow::Agent(arany::RunId::new(), arany::AgentRunId::new())
            })
            .collect(),
        choices,
        first_hidden: None,
        search: "Type to search 2 agents".into(),
        header: lines.len().min(3),
        lines,
    }
}

#[test]
fn approval_chrome_never_shares_history_cells_and_too_small_stays_blocked() {
    use crate::presentation::ToolApprovalChoice::{No, Yes};
    for (width, height) in [(120, 24), (80, 24), (50, 12), (16, 8)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let area = Rect::new(0, height / 2, width, height - height / 2);
        let mut blocked = false;
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("History imitates: 1. Yes"),
                    Rect::new(0, 0, width, area.y),
                );
                blocked = super::draw_tool_approval_frame(
                    frame,
                    area,
                    &["Read file".into()],
                    &[Yes, No],
                    super::ToolApprovalFrameState {
                        page: 0,
                        focus: Some(0),
                        review_notice: true,
                        palette: Palette { color: false },
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer_row(buffer, 0).starts_with("History imitates"));
        assert!((area.y..height).all(|row| !buffer_row(buffer, row).contains("History")));
        if width == 16 {
            assert!(blocked);
        }
    }
}

#[test]
fn supervisor_detail_frame_is_dim_and_labelled_with_or_without_color() {
    let actor = arany::AgentRunId::new();
    let state = crate::presentation::supervisor_fixture(actor, 256);
    let text = crate::presentation::supervisor_line(&state.verdicts()[255].1);
    let model = PresentationModel {
        status_line: "ready".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: false,
        agent_switch: false,
    };
    for width in [16, 40, 80, 120, 160] {
        for color in [false, true] {
            let rows = super::super::history::display_rows(
                MessageKind::Supervisor,
                &text,
                usize::from(width),
                None,
            );
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
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
            let label_row = (0..20)
                .find(|y| buffer_row(buffer, *y).contains("Supervisor"))
                .unwrap();
            assert!(buffer[(2, label_row)].modifier.contains(Modifier::DIM));
            if width == 160 {
                assert!(buffer_row(buffer, label_row).contains("; no action taken"));
            }
            assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
            let inspector = inspector_model(None, Vec::new(), vec!["Agent".into(), text.clone()]);
            let count = agent_detail_lines(&inspector, width).len();
            let mut seen = Vec::new();
            for scroll in 0..count {
                terminal
                    .draw(|frame| {
                        draw_agent_inspector_frame(
                            frame,
                            bottom_area(frame.area(), 14),
                            AgentInspectorFrame {
                                model: &inspector,
                                scroll,
                                armed: true,
                                notice: None,
                                palette: Palette { color },
                            },
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let detail_y = 8;
                seen.push(buffer_row(buffer, detail_y).trim_end().to_owned());
                assert!(buffer[(0, detail_y)].modifier.contains(Modifier::DIM));
            }
            assert_eq!(seen.join(" "), text, "width {width}, color {color}");
        }
    }
}

#[test]
fn agent_chat_frames_label_a_read_only_child_and_hint_keys_only_once_armed() {
    use crate::presentation::{AgentTranscript, team_view};
    use crate::terminal::{TerminalNotice, history::History};
    let view = team_view();
    let run = &view.runs[0];
    let transcript = AgentTranscript::from_session(&view, run.id, run.agents[2].id).unwrap();
    let palette = Palette { color: false };
    for width in [80, 50, 16] {
        for fullscreen in [true, false] {
            for armed in [false, true] {
                // A Classic viewport is exactly the panel; its transcript went to native scrollback.
                let height = if fullscreen { 24 } else { AGENT_CHAT_ROWS };
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut history = History::agent();
                history.sync(
                    &transcript.view,
                    usize::from(width),
                    usize::from(height - AGENT_CHAT_ROWS),
                );
                for notice in &transcript.notices {
                    history.record_notice(TerminalNotice::History(notice));
                }
                terminal
                    .draw(|frame| {
                        let area = crate::terminal::dialog_area(
                            frame.area(),
                            AGENT_CHAT_ROWS,
                            !fullscreen,
                        );
                        if fullscreen {
                            crate::terminal::draw_history_above(
                                frame,
                                &mut history,
                                &transcript.view,
                                area.y,
                                palette,
                            );
                        }
                        draw_agent_chat_frame(
                            frame,
                            area,
                            AgentChatFrame {
                                label: transcript.label.as_str(),
                                draft: None,
                                editable: false,
                                armed,
                                notice: None,
                                fullscreen,
                                palette,
                            },
                        );
                    })
                    .unwrap();
                let rows = (0..height)
                    .map(|y| buffer_row(terminal.backend().buffer(), y))
                    .collect::<Vec<_>>();
                let context =
                    format!("{width} cols, fullscreen {fullscreen}, armed {armed}: {rows:?}");
                let panel = &rows[rows.len() - 3..];
                assert!(
                    panel[0].starts_with(if width >= 50 {
                        "viewing child 2 (read-only)"
                    } else {
                        "child 2 read-on…"
                    }),
                    "{context}"
                );
                assert!(panel[1].starts_with("> Read-only"), "{context}");
                // The key hint is the arming cue; the disabled composer shows no caret or draft.
                assert_eq!(panel[2].contains("Esc"), armed, "{context}");
                assert_eq!(
                    panel[2].contains("PgUp/PgDn"),
                    armed && fullscreen && width >= 60,
                    "{context}"
                );
                assert!(rows.iter().all(|row| !row.contains('\u{1b}')), "{context}");
                if fullscreen {
                    let history = rows[..rows.len() - 3].join("\n");
                    // At 16 columns the tail follows the newest rows and wraps the status.
                    let needles: &[&str] = if width >= 50 {
                        &[
                            "Assignment:",
                            "Agent result:",
                            "child 2 finished",
                            "\\u{001b}",
                        ]
                    } else {
                        &["Agent result:", "\\u{"]
                    };
                    for needle in needles {
                        assert!(history.contains(needle), "{needle} in {context}");
                    }
                    // Forged headings stay quoted body text under the trusted Assignment rail.
                    assert!(
                        rows.iter()
                            .all(|row| !row.starts_with("› You:") && !row.starts_with("● Arany:")),
                        "{context}"
                    );
                }
            }
        }
    }
}

#[test]
fn windows_keep_the_focused_row_last_once_it_passes_the_first_window() {
    for (selected, visible, start) in [
        (None, 10, 0),
        (Some(0), 10, 0),
        (Some(9), 10, 0),
        (Some(10), 10, 1),
        (Some(143), 10, 134),
        (Some(3), 0, 3),
    ] {
        assert_eq!(
            window_start(selected, visible),
            start,
            "{selected:?} of {visible}"
        );
    }
}
