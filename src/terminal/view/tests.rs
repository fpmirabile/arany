use super::*;
use crate::provider::Effort;
use crate::terminal::{ComposerEdit, TerminalInput};
use ratatui::backend::{Backend, TestBackend};

#[test]
fn help_keeps_every_registered_command_reachable_at_narrow_widths() {
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        for (selected, command, description) in [
            (0, "/help", "List commands"),
            (help_len() - 1, "/exit", "Exit Session"),
        ] {
            terminal
                .draw(|frame| draw_help_frame(frame, selected, Palette { color: false }))
                .expect("help frame");
            let buffer = terminal.backend().buffer();
            let rows = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            assert!(
                rows.iter().any(|row| row.contains(command)),
                "{width}: {rows:?}"
            );
            assert!(
                rows.iter().any(|row| row.contains(description)),
                "{width}: {rows:?}"
            );
            assert!(rows.last().is_some_and(|row| {
                row.contains(if width < 24 { "Enter Esc" } else { "Enter/Esc" })
            }));
            assert!(
                buffer
                    .content()
                    .iter()
                    .all(|cell| { cell.fg == Color::Reset && cell.bg == Color::Reset })
            );
        }
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    for index in 0..help_len() {
        let entry = help_entry(index).expect("registered command");
        terminal
            .draw(|frame| draw_help_frame(frame, index, Palette { color: false }))
            .expect("help frame");
        let buffer = terminal.backend().buffer();
        let visible = (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(visible.contains(&format!("/{}", entry.name)), "{visible}");
    }
}

#[test]
fn setup_step_stays_centered_and_borderless_without_echoing_input() {
    for (width, height, top, left) in [
        (16, 8, 0u16, 0),
        (40, 12, 2, 0),
        (80, 24, 8, 8),
        (120, 24, 8, 28),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_setup_frame(
                    frame,
                    3,
                    "Enter API key",
                    "Hidden input; up to 512 ASCII characters",
                    12,
                    Some("Invalid key; try again"),
                    Palette { color: false },
                );
            })
            .expect("setup frame");
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows[usize::from(top)].find("Arany setup"),
            Some(left),
            "{width}: {rows:?}"
        );
        assert_eq!(
            rows[usize::from(top + 1)].find("Enter API key"),
            Some(left),
            "{width}: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("12 chars") || row.contains("12 characters"))
        );
        assert!(rows.iter().any(|row| row.contains("Invalid")));
        assert!(!rows.join("").contains(['┌', '┐', '└', '┘', '│']));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| { cell.fg == Color::Reset && cell.bg == Color::Reset })
        );

        for color in [false, true] {
            let mut draft = Composer::default();
            for text in ["", "wrkspc_Test123", &format!("wrkspc_{}", "x".repeat(121))] {
                draft.clear();
                for character in text.chars() {
                    draft.apply(TerminalInput::Character(character));
                }
                for edit in [None, Some(TerminalInput::Home), Some(TerminalInput::End)] {
                    if let Some(edit) = edit {
                        draft.apply(edit);
                    }
                    let caret = draft.cursor_byte_offset();
                    terminal
                        .draw(|frame| {
                            draw_setup_text_frame(
                                frame,
                                "API workspace ID",
                                "From Claude Console",
                                &draft,
                                Some("Error: retry"),
                                Palette { color },
                            );
                        })
                        .expect("visible workspace field");
                    let buffer = terminal.backend().buffer();
                    let visible = (0..height)
                        .map(|y| {
                            (0..width)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(visible.contains("API workspace ID"));
                    assert!(visible.contains("Enter") && visible.contains("Esc"));
                    assert!(visible.contains("Error: retry"));
                    assert!(!visible.contains("bytes") && !visible.contains(" B"));
                    if text.is_empty() {
                        assert!(visible.contains("wrkspc_..."));
                    } else if text == "wrkspc_Test123" {
                        assert!(visible.contains(text));
                    } else if caret == 0 {
                        assert!(visible.contains("wrkspc_"));
                    } else {
                        assert!(visible.contains("xxxxxxxx"));
                    }
                    let Position {
                        x: cursor_x,
                        y: cursor_y,
                    } = terminal.backend_mut().get_cursor_position().expect("caret");
                    assert!(cursor_x < width && cursor_y < height);
                    let area = setup_area(Rect::new(0, 0, width, height));
                    let line_width = usize::from(area.width - 2);
                    assert_eq!(cursor_x, area.x + 2 + (caret % line_width) as u16);
                    assert_eq!(
                        cursor_y,
                        area.y + 3 + (caret / line_width).min(usize::from(area.height - 6)) as u16
                    );
                    assert_eq!(draft.text(), text);
                    assert_eq!(draft.cursor_byte_offset(), caret);
                    if !color {
                        assert!(
                            terminal
                                .backend()
                                .buffer()
                                .content()
                                .iter()
                                .all(|cell| { cell.fg == Color::Reset && cell.bg == Color::Reset })
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn setup_choices_remain_visible_at_narrow_widths_without_color() {
    for (width, height) in [(16, 8), (40, 7), (40, 8), (50, 7), (50, 8), (80, 24)] {
        for (title, instruction, choices, selected, expected) in [
            (
                "Use private file?",
                "NOT encrypted; same-user apps read key",
                &[(b'2', "Cancel"), (b'1', "Use private file")][..],
                0,
                "> Cancel",
            ),
            (
                "Choose access method",
                "ChatGPT plan uses your subscription",
                &[(b'1', "API key"), (b'2', "ChatGPT plan")][..],
                0,
                "> API key",
            ),
            (
                "Choose saved access",
                "Both saved; choose billing route",
                &[(b'0', "Cancel"), (b'1', "API key"), (b'2', "ChatGPT plan")][..],
                2,
                "> ChatGPT plan",
            ),
            (
                "Saved ChatGPT",
                "* is current; choose account",
                &[(b'1', "1* abcdef12"), (b'2', "2  34567890")][..],
                0,
                "> 1* abcdef12",
            ),
            (
                "ChatGPT account",
                "Reuse, reconnect, or add",
                &[
                    (b'1', "Use saved"),
                    (b'2', "Reconnect"),
                    (b'3', "Connect new"),
                ][..],
                0,
                "> Use saved",
            ),
            (
                "Resume sign-in?",
                "Reuse saved registration",
                &[(b'0', "Back"), (b'1', "Resume")][..],
                0,
                "> Back",
            ),
            (
                "Resume sign-in?",
                "Reuse saved registration",
                &[(b'0', "Back"), (b'1', "Resume")][..],
                1,
                "> Resume",
            ),
        ] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            terminal
                .draw(|frame| {
                    draw_setup_choices_frame(
                        frame,
                        title,
                        instruction,
                        choices,
                        selected,
                        None,
                        Palette { color: false },
                    )
                })
                .expect("setup choices");
            let buffer = terminal.backend().buffer();
            let visible = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" ");
            assert!(visible.contains(expected), "{width}: {visible}");
            match title {
                "Use private file?" => {
                    assert!(visible.contains("NOT encrypted"));
                    assert!(visible.contains("read key"));
                }
                "Choose access method" => assert!(visible.contains("subscription")),
                "Choose saved access" => {
                    assert!(visible.contains("Both saved"));
                    assert!(visible.contains("billing"));
                }
                "Saved ChatGPT" => assert!(visible.contains("2  34567890")),
                "ChatGPT account" => {
                    assert!(visible.contains("Reconnect"));
                    assert!(visible.contains("Connect new"));
                }
                "Resume sign-in?" => {
                    assert!(visible.contains("Reuse saved"));
                    assert!(visible.contains("registration"));
                }
                _ => unreachable!(),
            }
            assert!(!visible.contains("1B Enter"));
            assert!(
                buffer
                    .content()
                    .iter()
                    .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
            );
        }
    }
    let mut terminal = Terminal::new(TestBackend::new(16, 8)).expect("test terminal");
    terminal
        .draw(|frame| {
            draw_setup_frame(
                frame,
                3,
                "Enter API key",
                "Hidden input; up to 512 ASCII characters",
                512,
                Some("Invalid or overlong API key"),
                Palette { color: false },
            )
        })
        .expect("secret error frame");
    let visible = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(visible.contains("512 chars"));
    assert!(visible.contains("Enter · Esc"));
    assert!(visible.contains("Invalid"));
}

#[test]
fn setup_warning_pages_every_line_before_showing_acceptance_actions() {
    let warning = "abcdefghijklmnop".repeat(20);
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        let body_height = height.min(16) - 5;
        let body_top = (height - height.min(16)) / 2 + 1;
        let body_left = (width - width.min(72)) / 2;
        let mut rendered = String::new();
        let mut final_page = false;
        for page in 0..20 {
            let mut has_more = false;
            terminal
                .draw(|frame| {
                    has_more = draw_setup_warning_frame(
                        frame,
                        &warning,
                        page,
                        false,
                        None,
                        Palette { color: false },
                    );
                })
                .expect("warning page");
            let buffer = terminal.backend().buffer();
            for y in body_top..body_top + body_height {
                for x in body_left..body_left + width.min(72) {
                    rendered.push_str(buffer[(x, y)].symbol());
                }
            }
            let visible = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(visible.contains("› Back"), !has_more);
            assert_eq!(visible.contains("  Accept"), !has_more);
            assert!(!visible.contains("I ACCEPT"));
            assert!(visible.contains("Esc cancels"));
            if width == 16 && page == 0 {
                assert!(visible.contains("Page 1/7 Enter"));
            }
            if !has_more {
                assert!(visible.contains("Tab/Enter"));
            }
            assert!(
                buffer
                    .content()
                    .iter()
                    .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
            );
            if !has_more {
                final_page = true;
                break;
            }
        }
        assert!(final_page);
        assert_eq!(
            rendered
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>(),
            warning,
            "width {width}"
        );
        for color in [false, true] {
            for accept in [false, true] {
                terminal
                    .draw(|frame| {
                        assert!(!draw_setup_warning_frame(
                            frame,
                            &warning,
                            usize::MAX,
                            accept,
                            None,
                            Palette { color }
                        ));
                    })
                    .expect("consent actions");
                let buffer = terminal.backend().buffer();
                let visible = buffer
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                let top = (height - height.min(16)) / 2;
                let left = (width - width.min(72)) / 2;
                for (index, label) in ["Back", "Accept"].iter().enumerate() {
                    let row = (left..left + width.min(72))
                        .map(|x| buffer[(x, top + height.min(16) - 4 + index as u16)].symbol())
                        .collect::<String>();
                    assert!(row.starts_with(&format!(
                        "{} {label}",
                        if accept == (index == 1) { "›" } else { " " }
                    )));
                }
                assert!(visible.contains("Tab/Enter") && visible.contains("Esc cancels"));
                if !color {
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
fn empty_session_has_a_quiet_welcome_without_obscuring_the_composer() {
    let model = PresentationModel {
        status_line: "idle · model".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
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
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
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
        assert!(rows[usize::from(height - 2)].contains("Ctrl+O"));
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
                    &model,
                    &Composer::default(),
                    None,
                    Palette { color: false },
                    &[HistoryRow {
                        text: "You:".into(),
                        speaker: MessageKind::User,
                        heading: true,
                        matched: false,
                    }],
                    None,
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
        };
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("frame");
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
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
        let row = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(2).contains("hello\\u{202e}[31m"));
        assert!(row(4).contains("Error: retry"));
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 5)).expect("test terminal");
    let model = PresentationModel {
        status_line: "working · openai/model".into(),
        activity_lines: vec!["primary · working".into()],
        setup_required: false,
        draft_action: DraftAction::InspectRun,
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
                        let footer = (0..width)
                            .map(|x| buffer[(x, 6)].symbol())
                            .collect::<String>();
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
                            "Enter · Ctrl+O"
                        };
                        assert!(
                            footer.contains(action),
                            "{width}, {draft_action:?}: {footer}"
                        );
                        assert!(!keeps_draft || !footer.contains("sends"));
                        if width >= 40 || (!command && (width >= 24 || !keeps_draft)) {
                            assert!(footer.contains("Ctrl+O"));
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
                        let status = (0..width)
                            .map(|x| buffer[(x, 7)].symbol())
                            .collect::<String>();
                        assert!(
                            status.contains("state"),
                            "busy draft never advertises command submission"
                        );
                        assert_eq!(composer.text(), text);
                        assert_eq!(composer.cursor_byte_offset(), text.len());
                        composer.clear();
                        for character in "/s".chars() {
                            composer.apply(TerminalInput::Character(character));
                        }
                        terminal
                            .draw(|frame| {
                                draw_frame(frame, &model, &composer, None, Palette { color })
                            })
                            .expect("busy completion frame");
                        let buffer = terminal.backend().buffer();
                        assert!(
                            (0..8).any(|y| (0..width)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                                .contains("Enter keeps")),
                            "busy completion does not imply execution"
                        );
                        assert_eq!(composer.text(), "/s");
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
    };
    let rows = vec![
        HistoryRow {
            text: "You:".into(),
            speaker: MessageKind::User,
            heading: true,
            matched: false,
        },
        HistoryRow {
            text: "  committed objective".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
        },
        HistoryRow {
            text: "Arany:".into(),
            speaker: MessageKind::Assistant,
            heading: true,
            matched: false,
        },
    ];
    terminal
        .draw(|frame| {
            draw_frame_with_history(
                frame,
                &model,
                &Composer::default(),
                None,
                Palette { color: false },
                &rows,
                Some("History · Ctrl+L live"),
            );
        })
        .expect("history frame");
    let buffer = terminal.backend().buffer();
    let row = |y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>();
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
    };
    for color in [false, true] {
        let mut wide = Terminal::new(TestBackend::new(80, 12)).expect("wide terminal");
        let mut feedback_rows = rows.clone();
        feedback_rows.push(HistoryRow {
            text: "Arany · error:".into(),
            speaker: MessageKind::Error,
            heading: true,
            matched: false,
        });
        feedback_rows.push(HistoryRow {
            text: "  Retry /setup; your draft is retained".into(),
            speaker: MessageKind::Error,
            heading: false,
            matched: false,
        });
        wide.draw(|frame| {
            draw_frame_with_history(
                frame,
                &wide_model,
                &Composer::default(),
                None,
                Palette { color },
                &rows,
                Some("History · Ctrl+L live"),
            );
        })
        .expect("wide history frame");
        let status = (0..80)
            .map(|x| wide.backend().buffer()[(x, 11)].symbol())
            .collect::<String>();
        assert!(
            status.contains("History · Ctrl+L live · working · openai/model · Safe conversation"),
            "{status}"
        );

        wide.draw(|frame| {
            draw_frame_with_history(
                frame,
                &wide_model,
                &Composer::default(),
                None,
                Palette { color },
                &feedback_rows,
                Some("History · Ctrl+L live"),
            );
        })
        .expect("wide error frame");
        let status = (0..80)
            .map(|x| wide.backend().buffer()[(x, 11)].symbol())
            .collect::<String>();
        assert!(
            status.starts_with("History · Ctrl+L live · working"),
            "{status}"
        );
        let buffer = wide.backend().buffer();
        let chat_error = (0..80).map(|x| buffer[(x, 4)].symbol()).collect::<String>();
        let chat_body = (0..80).map(|x| buffer[(x, 5)].symbol()).collect::<String>();
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
                &model,
                &Composer::default(),
                None,
                Palette { color: true },
                &rows,
                None,
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
                        &model,
                        &Composer::default(),
                        None,
                        Palette { color },
                        &rows,
                        None,
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
            text: "  continued".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: true,
        },
        HistoryRow {
            text: String::new(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
        },
        rows[2].clone(),
        HistoryRow {
            text: "  You: is quoted".into(),
            speaker: MessageKind::Assistant,
            heading: false,
            matched: false,
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
                        &model,
                        &Composer::default(),
                        None,
                        Palette { color },
                        &conversation,
                        None,
                    );
                })
                .expect("conversation frame");
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
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
        };
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("multiline frame");
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(0).contains("primary · working"));
        assert!(row(1).contains("Ask Arany"));
        assert!(row(2).contains("top"));
        assert_eq!(buffer[(2, 3)].symbol(), "中");
        assert_eq!(buffer[(4, 3)].symbol(), "e\u{301}");
        assert!(row(3).contains("\\u{202e}"));
        assert!(row(4).contains("last"));
        assert!(row(5).contains("Enter keeps draft"));
        assert!(row(5).contains("Ctrl+O newline"));
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
    };
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("short multiline frame");
    let buffer = terminal.backend().buffer();
    let row = |y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>();
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
        let heading = (0..40).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
        assert!(heading.contains(expected_range), "{heading}");
        assert_eq!(composer.cursor_byte_offset(), cursor);
        assert_eq!(composer.text(), "line0\nline1\nline2\nline3\nline4\nline5");
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
            };
            terminal
                .draw(|frame| draw_frame(frame, &preparing, &composer, None, Palette { color }))
                .expect("preparing frame");
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
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
fn slash_argument_placeholder_is_visual_only_until_value_is_typed() {
    let model = PresentationModel {
        status_line: "idle".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
    };
    let mut composer = Composer::default();
    for character in "/provider ".chars() {
        composer.apply(TerminalInput::Character(character));
    }
    for width in [40, 50, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 4)).expect("test terminal");
        for _ in 0..2 {
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
                .expect("frame");
            let buffer = terminal.backend().buffer();
            let row = (0..width)
                .map(|x| buffer[(x, 1)].symbol())
                .collect::<String>();
            assert!(row.contains("/provider <provider>"));
            assert!(!buffer[(1, 1)].modifier.contains(Modifier::DIM));
            assert!(!buffer[(12, 1)].modifier.contains(Modifier::DIM));
            assert_eq!(composer.text(), "/provider ");
        }
    }
    let mut colored = Terminal::new(TestBackend::new(40, 4)).expect("colored test terminal");
    colored
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: true }))
        .expect("colored frame");
    assert!(
        colored.backend().buffer()[(12, 1)]
            .modifier
            .contains(Modifier::DIM)
    );
    composer.apply(TerminalInput::Character('o'));
    let mut terminal = Terminal::new(TestBackend::new(80, 4)).expect("test terminal");
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/provider o[penai]"));
    assert!(!row.contains("<provider>"));
    assert_eq!(composer.text(), "/provider o");
    for (width, expected) in [(18, "/provider o[pe…]"), (20, "/provider o[penai]")] {
        let mut narrow = Terminal::new(TestBackend::new(width, 4)).expect("narrow test terminal");
        narrow
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("narrow frame");
        let buffer = narrow.backend().buffer();
        let row = (0..width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(row.contains(expected), "width {width}: {row}");
    }
    let mut too_narrow = Terminal::new(TestBackend::new(14, 4)).expect("short test terminal");
    too_narrow
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("short frame");
    assert_eq!(too_narrow.backend().buffer()[(13, 1)].symbol(), " ");
    colored
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: true }))
        .expect("colored completion frame");
    let buffer = colored.backend().buffer();
    let row = (0..40).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/provider openai"));
    assert!(buffer[(13, 1)].modifier.contains(Modifier::DIM));

    composer.apply(TerminalInput::Left);
    let cursor = composer.cursor_byte_offset();
    assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
    assert_eq!(composer.cursor_byte_offset(), cursor);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("mid-draft frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/provider o"));
    assert!(!row.contains("penai"));
    let status = (0..80).map(|x| buffer[(x, 3)].symbol()).collect::<String>();
    assert!(status.starts_with("idle"));
    composer.apply(TerminalInput::End);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("end-of-draft frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/provider o[penai]"));

    composer.apply(TerminalInput::Backspace);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/provider <provider>"));
    assert_eq!(composer.text(), "/provider ");

    composer.clear();
    composer.set_completion_selection(Some("openai"), Some("gpt-5.4"), None);
    composer.set_model_catalog(
        "openai",
        &[ModelEntry {
            id: "gpt-5.4".into(),
            runnable: true,
            efforts: vec![Effort::Medium],
        }],
        false,
    );
    for character in "/model gpt-5".chars() {
        composer.apply(TerminalInput::Character(character));
    }
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/model gpt-5[.4]"));
    assert!(!buffer[(13, 1)].modifier.contains(Modifier::DIM));
    assert_eq!(composer.text(), "/model gpt-5");

    composer.clear();
    composer.set_model_catalog(
        "openai",
        &[ModelEntry {
            id: "gpt-[alpha]".into(),
            runnable: true,
            efforts: vec![Effort::Medium],
        }],
        false,
    );
    for character in "/model gpt-".chars() {
        composer.apply(TerminalInput::Character(character));
    }
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("model bracket frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/model gpt-[[alpha]]"));
    assert_eq!(composer.text(), "/model gpt-");

    composer.clear();
    for character in "/prov".chars() {
        composer.apply(TerminalInput::Character(character));
    }
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("command preview frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/prov"));
    assert!(!row.contains("[ider"));
    assert_eq!(composer.text(), "/prov");
    composer.apply(TerminalInput::Submit);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("completed command argument frame");
    let buffer = terminal.backend().buffer();
    let row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
    assert!(row.contains("/provider <provider>"));
    assert_eq!(composer.text(), "/provider ");

    composer.clear();
    for character in "/s".chars() {
        composer.apply(TerminalInput::Character(character));
    }
    composer.apply(TerminalInput::Down);
    let cursor = composer.cursor_byte_offset();
    composer.set_completion_layout(super::super::CompletionLayout::Combined);
    composer.set_runtime_skills(&["review".into(), "help".into()]);
    for (prefix, focused, category) in [
        ("/rev", "review", "Skill"),
        ("/he", "help", "Skill"),
        ("/resum", "resume", "Cmd"),
        ("/permissio", "permissions", "Cmd"),
        ("/s", "status", "Cmd"),
    ] {
        composer.clear();
        composer.insert_paste(prefix).expect("command prefix");
        composer.apply(TerminalInput::Down);
        let cursor = composer.cursor_byte_offset();
        for width in [16, 20, 40, 50, 80] {
            for height in [8, 24] {
                for color in [false, true] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height))
                        .expect("completion choices terminal");
                    terminal
                        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                        .expect("completion choices frame");
                    let buffer = terminal.backend().buffer();
                    let row = |y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    };
                    let menu_height = completion_height(&composer, height.saturating_sub(4));
                    let input_y = height - 3 - menu_height;
                    let focused_y = (0..height)
                        .find(|y| {
                            row(*y).contains(&safe_truncate(
                                &format!("> {category} /{focused}"),
                                usize::from(width),
                            ))
                        })
                        .expect("focused completion visible");
                    assert!(focused_y > input_y, "completion stays below input");
                    assert!(row(input_y).contains(&format!("> {prefix}")));
                    assert!(row(height - 1).starts_with(if category == "Skill" {
                        "Tab/Enter: $"
                    } else {
                        "Tab/Enter: /"
                    }));
                    assert!(row(input_y + 1).contains("Tab/Enter"));
                    assert!(buffer[(0, focused_y)].modifier.contains(Modifier::BOLD));
                    assert_eq!(
                        buffer[(0, focused_y)].fg,
                        if color { Color::Cyan } else { Color::Reset }
                    );
                    assert_eq!(composer.text(), prefix);
                    assert_eq!(composer.cursor_byte_offset(), cursor);
                    assert_eq!(
                        terminal.get_cursor_position().expect("completion cursor").y,
                        input_y
                    );
                }
            }
        }
    }
    for (layout, skill_count, skills_tab, last) in [
        (super::super::CompletionLayout::Tabs, 64, false, false),
        (super::super::CompletionLayout::Tabs, 64, true, true),
        (super::super::CompletionLayout::Tabs, 0, true, false),
        (super::super::CompletionLayout::Combined, 64, false, true),
    ] {
        for width in [16, 40, 50, 79, 80, 120] {
            for height in [8, 24] {
                for color in [false, true] {
                    let mut draft = Composer::default();
                    draft.set_completion_layout(layout);
                    draft.set_runtime_skills(
                        &(0..skill_count)
                            .map(|i| format!("skill-{i:02}"))
                            .collect::<Vec<_>>(),
                    );
                    draft.insert_paste("/").unwrap();
                    if skills_tab {
                        draft.apply(TerminalInput::Right);
                    }
                    if last {
                        draft.apply(TerminalInput::Up);
                    }
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| draw_frame(frame, &model, &draft, None, Palette { color }))
                        .unwrap();
                    let caret = terminal.get_cursor_position().unwrap();
                    let buffer = terminal.backend().buffer();
                    let row = |y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    };
                    assert!(row(caret.y).starts_with("> /"));
                    let labels = (caret.y + 2..height - 1).map(row).collect::<Vec<_>>();
                    assert!(!labels.is_empty());
                    assert!(labels[0].contains(
                        if layout == super::super::CompletionLayout::Combined {
                            if width < 20 {
                                "Commands +"
                            } else {
                                "Commands + Skills"
                            }
                        } else if skills_tab {
                            "[Skills]"
                        } else if width < 40 {
                            "[Cmd]"
                        } else {
                            "[Commands]"
                        }
                    ));
                    if layout == super::super::CompletionLayout::Tabs {
                        let selected_x = labels[0].find('[').unwrap() as u16;
                        assert!(
                            buffer[(selected_x, caret.y + 2)]
                                .modifier
                                .contains(Modifier::REVERSED)
                        );
                        assert!(
                            buffer[(selected_x, caret.y + 2)]
                                .modifier
                                .contains(Modifier::BOLD)
                        );
                        let inactive_x = if skills_tab {
                            0
                        } else {
                            labels[0].find("Skills").unwrap() as u16
                        };
                        assert!(
                            !buffer[(inactive_x, caret.y + 2)]
                                .modifier
                                .contains(Modifier::REVERSED)
                        );
                    }
                    assert!(labels.len() <= 21);
                    if skill_count == 0 {
                        assert!(labels.iter().any(|row| row.starts_with("No Skills")));
                    }
                    if last {
                        assert!(labels.iter().any(|row| row.starts_with("> Skill /skill-")));
                    }
                    if width >= 79 && height == 24 && !skills_tab && !last {
                        assert_eq!(
                            labels.len(),
                            18,
                            "large Commands list includes the entire registry"
                        );
                        assert!(
                            labels
                                .iter()
                                .any(|row| row.contains("Choose slash menu display"))
                        );
                    }
                    if !color {
                        assert!(buffer.content().iter().all(|cell| cell.fg == Color::Reset));
                    }
                    assert_eq!(draft.text(), "/");
                    assert_eq!(draft.cursor_byte_offset(), 1);
                }
            }
        }
    }
    composer.apply(TerminalInput::Escape);
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("closed completion terminal");
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("closed completion frame");
    let buffer = terminal.backend().buffer();
    assert!(!(0..8).any(|y| {
        (0..40)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .contains("> Cmd /status")
    }));
    assert_eq!(
        terminal.get_cursor_position().expect("retained cursor").y,
        5
    );
    assert_eq!(composer.text(), "/s");
    assert_eq!(composer.cursor_byte_offset(), cursor);

    for profile in ["chatgpt", "openai", "anthropic"] {
        composer.clear();
        composer.set_completion_selection(Some(profile), Some("pending-model"), None);
        composer.set_model_catalog(
            profile,
            &[ModelEntry {
                id: "pending-model".into(),
                runnable: false,
                efforts: vec![],
            }],
            false,
        );
        for character in "/model x ".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        for width in [16, 20, 40, 80] {
            for color in [false, true] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, 8)).expect("pending effort terminal");
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .expect("pending effort frame");
                let buffer = terminal.backend().buffer();
                let row = (0..width)
                    .map(|x| buffer[(x, 5)].symbol())
                    .collect::<String>();
                assert!(row.contains("/model x "));
                assert!(!row.contains("default"));
                if width >= 40 {
                    assert!(row.contains("<effort>"));
                }
                assert_eq!(composer.text(), "/model x ");
                assert_eq!(composer.cursor_byte_offset(), "/model x ".len());
            }
        }
        composer.apply(TerminalInput::Character('h'));
        let mut terminal =
            Terminal::new(TestBackend::new(40, 8)).expect("explicit effort terminal");
        terminal
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("explicit effort frame");
        let buffer = terminal.backend().buffer();
        let row = (0..40).map(|x| buffer[(x, 5)].symbol()).collect::<String>();
        assert!(row.contains("/model x h[igh]"));
        assert!(!row.contains("<effort>"));
        assert_eq!(composer.text(), "/model x h");
    }

    let first_account = uuid::Uuid::now_v7();
    let second_account = uuid::Uuid::now_v7();
    for width in [16, 20, 40, 80] {
        for color in [false, true] {
            composer.clear();
            composer.set_completion_selection(Some("chatgpt"), Some("xold"), Some(first_account));
            composer.set_model_catalog(
                "chatgpt",
                &[ModelEntry::exact_custom("xold".into())],
                false,
            );
            for character in "/model x".chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let mut terminal =
                Terminal::new(TestBackend::new(width, 8)).expect("account-scoped preview terminal");
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                .expect("previous account preview");
            let row = (0..width)
                .map(|x| terminal.backend().buffer()[(x, 5)].symbol())
                .collect::<String>();
            assert!(row.contains(if color {
                "/model xold"
            } else {
                "/model x[old]"
            }));
            let cursor = terminal
                .get_cursor_position()
                .expect("previous preview cursor");
            composer.set_completion_selection(Some("chatgpt"), Some("xold"), Some(second_account));
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                .expect("new account preview");
            let row = (0..width)
                .map(|x| terminal.backend().buffer()[(x, 5)].symbol())
                .collect::<String>();
            assert!(row.contains("/model x"));
            assert!(!row.contains("old"));
            assert_eq!(composer.text(), "/model x");
            assert_eq!(composer.cursor_byte_offset(), "/model x".len());
            assert_eq!(
                terminal.get_cursor_position().expect("new preview cursor"),
                cursor
            );
        }
    }
}

#[test]
fn composer_frame_wraps_draft_and_keeps_grapheme_cursor_visible() {
    let model = PresentationModel {
        status_line: "idle".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
    };
    for width in [16, 40, 80] {
        for color in [true, false] {
            for action in [DraftAction::Submit, DraftAction::Retain] {
                let mut model = model.clone();
                model.draft_action = action;
                let mut composer = Composer::default();
                composer.insert_paste("alpha\n中").unwrap();
                composer.apply(TerminalInput::Left);
                let text = composer.text().to_owned();
                let caret = composer.cursor_byte_offset();
                let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .unwrap();
                let cursor_before = terminal.backend_mut().get_cursor_position().unwrap();
                composer
                    .attach_image(crate::provider::test_image())
                    .unwrap();
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .unwrap();
                let cursor_after = terminal.backend_mut().get_cursor_position().unwrap();
                assert_eq!(
                    cursor_before, cursor_after,
                    "image attachment moved the caret at {width}"
                );
                assert_eq!(composer.text(), text);
                assert_eq!(composer.cursor_byte_offset(), caret);
                let buffer = terminal.backend().buffer();
                let rows = (0..8)
                    .map(|y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>();
                assert!(
                    rows[3].contains(if width < 40 { "1img" } else { "1 image" }),
                    "{width}: {rows:?}"
                );
                assert!(rows[4].contains("alpha") && rows[5].contains('中'));
                assert!(!rows.iter().any(|row| row.contains("iVBOR")));
                if !color {
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
    for (width, text, expected, cursor) in [
        (
            16,
            "abcdefghijklm中e\u{301}XYZ".to_owned(),
            vec!["abcdefghijklm", "中e\u{301}XYZ"],
            (8, 5),
        ),
        (
            16,
            "0123456789abcd".to_owned(),
            vec!["0123456789abcd", ""],
            (2, 5),
        ),
        (
            16,
            "abcdefghij\u{202e}Z".to_owned(),
            vec!["abcdefghij", "\\u{202e}Z"],
            (11, 5),
        ),
        (
            40,
            format!("{}tail", "x".repeat(38)),
            vec!["xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "tail"],
            (6, 5),
        ),
        (
            16,
            "abcdefghijklmnABCDEFGHIJKLMN0123456789abcdEFGHIJKLMNOPQRstuvwxyzABCDEF".to_owned(),
            vec!["0123456789abcd", "EFGHIJKLMNOPQR", "stuvwxyzABCDEF", ""],
            (2, 5),
        ),
    ] {
        let mut composer = Composer::default();
        for character in text.chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).expect("wrapped terminal");
        terminal
            .draw(|frame| {
                draw_frame(frame, &model, &composer, None, Palette { color: false });
            })
            .expect("wrapped frame");
        let buffer = terminal.backend().buffer();
        let first = 6 - expected.len() as u16;
        if text.len() > 60 && width == 16 {
            let heading = (0..width)
                .map(|x| buffer[(x, first - 1)].symbol())
                .collect::<String>();
            assert!(heading.contains("6/6"), "{heading}");
        }
        for (offset, expected) in expected.iter().enumerate() {
            let y = first + offset as u16;
            let mut x = 2;
            for grapheme in expected.graphemes(true) {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    grapheme,
                    "{width}: visual draft row {offset}, cell {x}"
                );
                x += UnicodeWidthStr::width(grapheme) as u16;
            }
            assert!(
                (x..width).all(|x| buffer[(x, y)].symbol() == " "),
                "{width}: unexpected suffix in draft row {offset}"
            );
        }
        assert_eq!(
            composer.text(),
            text,
            "visual wrapping cannot edit draft bytes"
        );
        assert!(
            !buffer
                .content()
                .iter()
                .any(|cell| cell.symbol().contains(['\u{1b}', '\u{202e}']))
        );
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
        );
        let observed = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("wrapped cursor");
        assert_eq!((observed.x, observed.y), cursor, "{width}: wrapped caret");
        composer.apply(TerminalInput::Backspace);
        assert_eq!(
            composer.text(),
            &text[..text
                .grapheme_indices(true)
                .next_back()
                .expect("last grapheme")
                .0]
        );
    }

    for width in [16, 24, 40, 80] {
        for (text, joined) in [
            ("a\n\u{301}b", "a\u{301}"),
            ("👩\n\u{200d}💻b", "👩\u{200d}💻"),
        ] {
            let joined_width = UnicodeWidthStr::width(joined) as u16;
            let prefix = "x".repeat(usize::from(width - 2 - joined_width));
            let mut composer = Composer::default();
            for character in prefix.chars().chain(text.chars()) {
                composer.apply(if character == '\n' {
                    TerminalInput::Newline
                } else {
                    TerminalInput::Character(character)
                });
            }
            composer.apply(TerminalInput::Home);
            for _ in 0..=prefix.len() {
                composer.apply(TerminalInput::Right);
            }
            composer.apply(TerminalInput::Delete);
            for color in [false, true] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, 8)).expect("joined terminal");
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .expect("joined frame");
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(width - joined_width, 4)].symbol(), joined);
                assert_eq!(buffer[(2, 5)].symbol(), "b");
                let cursor = terminal
                    .backend_mut()
                    .get_cursor_position()
                    .expect("joined cursor");
                assert_eq!(
                    (cursor.x, cursor.y),
                    (2, 5),
                    "{width}, color={color}: {text:?}"
                );
            }
        }
    }

    let mut composer = Composer::default();
    for width in [16, 24, 40, 80] {
        for color in [false, true] {
            let mut composer = Composer::default();
            for character in "alpha e\u{301} 中 tail".chars() {
                composer.apply(TerminalInput::Character(character));
            }
            composer.apply(TerminalInput::Home);
            for (input, offset, x, removed) in [
                (TerminalInput::WordRight, 6, 8, false),
                (TerminalInput::WordRight, 10, 10, false),
                (TerminalInput::WordLeft, 6, 8, false),
                (TerminalInput::WordRight, 10, 10, false),
                (TerminalInput::BackspaceWord, 6, 8, true),
            ] {
                composer.apply(input);
                let mut terminal =
                    Terminal::new(TestBackend::new(width, 8)).expect("word terminal");
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .expect("word editing frame");
                let cursor = terminal
                    .backend_mut()
                    .get_cursor_position()
                    .expect("word caret");
                let y = if width == 16 && !removed { 4 } else { 5 };
                assert_eq!(
                    (cursor.x, cursor.y),
                    (x, y),
                    "{width}, color={color}: {input:?}"
                );
                assert_eq!(composer.cursor_byte_offset(), offset);
                if width == 80 {
                    let footer = (0..width)
                        .map(|x| terminal.backend().buffer()[(x, 6)].symbol())
                        .collect::<String>();
                    assert!(footer.contains("Shift+Enter newline"));
                }
                assert_eq!(
                    composer.text(),
                    if removed {
                        "alpha 中 tail"
                    } else {
                        "alpha e\u{301} 中 tail"
                    }
                );
                assert_eq!(
                    terminal.backend().buffer()[(if removed { 8 } else { 10 }, y)].symbol(),
                    "中"
                );
            }
        }
    }

    for width in [16, 24, 40, 80] {
        for color in [false, true] {
            let mut pasted = Composer::default();
            pasted.insert_paste("a\tb\r\n中").expect("safe paste");
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).expect("paste terminal");
            terminal
                .draw(|frame| draw_frame(frame, &model, &pasted, None, Palette { color }))
                .expect("pasted multiline frame");
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(2, 4)].symbol(), "a");
            assert!((3..7).all(|x| buffer[(x, 4)].symbol() == " "));
            assert_eq!(buffer[(7, 4)].symbol(), "b");
            assert_eq!(buffer[(2, 5)].symbol(), "中");
            assert_eq!(pasted.text(), "a\tb\n中", "drawing expanded canonical tabs");
            assert!(
                !buffer
                    .content()
                    .iter()
                    .any(|cell| cell.symbol().contains(['\r', '\t', '\x1b']))
            );
            let cursor = terminal
                .backend_mut()
                .get_cursor_position()
                .expect("paste caret");
            assert_eq!((cursor.x, cursor.y), (4, 5));
        }
    }

    let text = format!("{}中CENTER{}", "left-".repeat(12), "-right".repeat(12));
    for character in text.chars() {
        composer.apply(TerminalInput::Character(character));
    }
    composer.apply(TerminalInput::Home);
    for _ in 0.."left-".repeat(12).chars().count() {
        composer.apply(TerminalInput::Right);
    }
    let backend = TestBackend::new(40, 4);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let model = PresentationModel {
        status_line: "idle".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
    };
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("frame");
    let row = (0..40)
        .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
        .collect::<String>();
    assert!(row.contains('中') && row.contains("CENTER"), "{row}");
    let wide_x = (0..40)
        .find(|x| terminal.backend().buffer()[(*x, 1)].symbol() == "中")
        .expect("wide grapheme");
    let cursor = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor");
    assert_eq!(cursor.x, wide_x);
    assert_eq!(cursor.y, 1);

    composer.apply(TerminalInput::Right);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("frame after Right");
    let wide_x = (0..40)
        .find(|x| terminal.backend().buffer()[(*x, 1)].symbol() == "中")
        .expect("wide grapheme after Right");
    let cursor = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor after Right");
    assert_eq!(cursor.x, wide_x + 2);
}

#[test]
fn session_picker_keeps_selection_visible_and_titles_inert() {
    let items = (0..15)
        .map(|index| SessionListItem {
            id: crate::session::SessionId::new(),
            title: if index == 14 {
                "Hostile\u{1b}[31m\u{202e} title".into()
            } else {
                format!("Session {index}")
            },
            last_sequence: index,
            created_at: "2026-10-06 12:00:00".into(),
            last_activity_at: "2026-10-06 12:05:00".into(),
            defaults: crate::SessionDefaults {
                provider: Some("chatgpt".into()),
                model: Some("gpt-6.1-sol".into()),
                ..crate::SessionDefaults::default()
            },
        })
        .collect::<Vec<_>>();
    for width in [16, 20, 24, 40, 50, 80] {
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_session_picker_frame(frame, &items, 14, Palette { color: false }))
            .expect("picker frame");
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(0).contains("Resume · 15"));
        assert!(row(6).starts_with("> Hostile"));
        if width == 80 {
            assert!(row(6).contains("\\u{202e}"));
        }
        assert!(!row(6).contains('\u{1b}'));
        assert!(!row(6).contains('\u{202e}'));
        assert!(row(7).contains("2026-10-06"));
        assert!(row(8).contains("2026-10-06"));
        assert!(row(9).contains("ChatGPT plan"));
        assert!(row(10).contains("gpt-6.1-sol"));
        assert!(row(11).contains(if width < 20 {
            "Up/Dn Enter Esc"
        } else if width < 24 {
            "Up/Dn Enter:go Esc"
        } else if width < 40 {
            "Up/Dn Enter:resume Esc"
        } else {
            "Enter resume"
        }));
        let area = Rect::new(0, 0, width, 12);
        assert_eq!(session_picker_index(area, 0, 1, 14, 15), Some(9));
        assert_eq!(session_picker_index(area, 0, 6, 14, 15), Some(14));
        assert_eq!(session_picker_index(area, 0, 0, 14, 15), None);
        for row in 7..12 {
            assert_eq!(session_picker_index(area, 0, row, 14, 15), None);
        }
        assert_eq!(session_picker_index(area, width, 10, 14, 15), None);
        assert_eq!(session_picker_index(area, 0, 2, 0, 1), None);
        assert_eq!(buffer[(0, 6)].fg, Color::Reset);
        assert!(buffer[(0, 6)].modifier.contains(Modifier::BOLD));
        terminal
            .draw(|frame| draw_session_picker_frame(frame, &items, 14, Palette { color: true }))
            .expect("colored picker frame");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 6)].fg, Color::Cyan);
        assert_eq!(buffer[(0, 9)].fg, Color::Reset);
    }
}

#[test]
fn quick_actions_keep_focus_and_draft_guard_visible_without_color() {
    for width in [16, 20, 24, 40, 50, 80] {
        for height in [4, 6, 7, 8] {
            for color in [false, true] {
                for (selected, name) in [(0, "Models"), (1, "Agents"), (2, "Resume"), (3, "Setup")]
                {
                    for has_draft in [false, true] {
                        let mut terminal =
                            Terminal::new(TestBackend::new(width, height)).expect("quick selector");
                        terminal
                            .draw(|frame| {
                                draw_quick_actions_frame(
                                    frame,
                                    selected,
                                    has_draft,
                                    Palette { color },
                                )
                            })
                            .expect("quick frame");
                        let buffer = terminal.backend().buffer();
                        let rows = (0..height)
                            .map(|y| {
                                (0..width)
                                    .map(|x| buffer[(x, y)].symbol())
                                    .collect::<String>()
                            })
                            .collect::<Vec<_>>();
                        let row = rows
                            .iter()
                            .position(|row| row.starts_with(&format!("> {name}")))
                            .expect("focused action visible");
                        let footer = rows.last().expect("footer");
                        assert!(footer.contains("Esc"));
                        if selected == 2 && has_draft {
                            assert!(footer.contains("draft") || footer.contains("Draft"));
                        } else {
                            assert!(footer.contains("Enter"));
                        }
                        assert!(buffer[(0, row as u16)].modifier.contains(Modifier::BOLD));
                        assert!(!rows.iter().any(|row| row.contains("Effort")));
                        if !color {
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
                                &crate::terminal::ModelPicker {
                                    items: &items,
                                    efforts: &efforts,
                                    selected: 4095,
                                    exact_custom: false,
                                    filter,
                                    invalid: false,
                                    catalog_state,
                                },
                                &composer,
                                &model,
                                Palette { color },
                            );
                        })
                        .expect("chat model panel");
                    let buffer = terminal.backend().buffer();
                    let rows = (0..height)
                        .map(|y| {
                            (0..width)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>();
                    let focused = rows
                        .iter()
                        .find(|row| row.starts_with("> 4096 "))
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
                        crate::terminal::ModelCatalogState::Updated => Some("Refreshed"),
                        crate::terminal::ModelCatalogState::RefreshFailed => Some("Refresh failed"),
                    };
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

#[test]
fn inline_custom_model_catalog_shows_declared_efforts() {
    let items = vec![ModelEntry {
        id: "same-long-prefix-model-alpha".into(),
        runnable: true,
        efforts: vec![Effort::Low, Effort::High],
    }];
    let model = PresentationModel {
        status_line: "idle".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
    };
    for width in [16, 24, 40, 50, 80] {
        for (filter, empty) in [("", false), ("MODEL", false), ("missing", true)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).expect("test terminal");
            let composer = Composer::default();
            terminal
                .draw(|frame| {
                    draw_frame(frame, &model, &composer, None, Palette { color: false });
                    draw_model_catalog_frame(
                        frame,
                        &crate::terminal::ModelPicker {
                            items: if empty { &[] } else { &items },
                            efforts: if empty { &[] } else { &[Some(Effort::High)] },
                            selected: 0,
                            exact_custom: true,
                            filter,
                            invalid: false,
                            catalog_state: crate::terminal::ModelCatalogState::Current,
                        },
                        &composer,
                        &model,
                        Palette { color: false },
                    );
                })
                .expect("custom model panel");
            let buffer = terminal.backend().buffer();
            let rows = (0..8)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            if empty {
                assert!(rows.iter().any(|row| row.contains("No matches")));
            } else {
                assert!(rows[1].contains("<high>"));
                assert!(
                    rows[1].contains(if width == 16 { "ha" } else { "alpha" }),
                    "{width}: {rows:?}"
                );
            }
            if filter.is_empty() {
                assert!(rows[0].contains("Models · exact"));
            } else {
                assert!(rows[0].contains(&format!("Find {filter}")));
            }
            assert!(rows.iter().any(|row| row.contains("Esc")));
        }
    }
}

#[test]
fn agent_inspector_keeps_details_and_keyboard_help_in_viewport() {
    for width in [40, 50, 79, 80, 120] {
        let backend = TestBackend::new(width, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let model = AgentInspectorModel {
            selected: 1,
            choices: vec![
                "Run 1: primary; Finished".into(),
                "Run 1: child 1; Failed".into(),
            ],
            lines: vec![
                "Last Run: 2 agents; children: 1".into(),
                "History: 2 agents in 1 recent Runs".into(),
                "Next Run: Single".into(),
                "Agent 2/2: child 1; Failed".into(),
                "Objective: hostile\\u{001b}[31m".into(),
                "Summary: bounded details".into(),
            ],
        };
        terminal
            .draw(|frame| draw_agent_inspector_frame(frame, &model, 0, Palette { color: false }))
            .expect("agent frame");
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(0).contains("Last Run: 2 agents; children: 1"));
        assert!(row(1).contains("primary; Finished"));
        assert!(row(2).starts_with("> Run 1: child 1; Failed"));
        assert!(row(5).contains("child 1; Failed"));
        assert!(row(6).contains("hostile\\u{001b}[31m"));
        assert!(row(13).contains("Esc close"));
        let area = Rect::new(0, 0, width, 14);
        assert_eq!(agent_picker_index(area, 0, 1, 1, 2), Some(0));
        assert_eq!(agent_picker_index(area, 0, 2, 1, 2), Some(1));
        assert_eq!(agent_picker_index(area, 0, 0, 1, 2), None);
        assert_eq!(agent_picker_index(area, 0, 3, 1, 2), None);
        assert_eq!(agent_picker_index(area, width, 1, 1, 2), None);
        assert_eq!(agent_picker_index(area, 0, 1, 0, 0), None);
        assert_eq!(agent_picker_index(area, 0, 1, 7, 8), Some(4));
        assert_eq!(agent_picker_index(area, 0, 4, 7, 8), Some(7));
        assert_eq!(agent_picker_index(area, 0, 5, 7, 8), None);
        for y in 0..14 {
            assert!(!row(y).contains('\u{1b}'));
        }
    }

    let mut terminal = Terminal::new(TestBackend::new(16, 14)).expect("narrow terminal");
    let model = AgentInspectorModel {
        selected: 0,
        choices: vec!["Run 1: primary; Finished".into()],
        lines: vec![
            "Last Run: 1 agent; children: 0".into(),
            "Next Run: Single".into(),
        ],
    };
    terminal
        .draw(|frame| draw_agent_inspector_frame(frame, &model, 0, Palette { color: false }))
        .expect("narrow agent frame");
    let footer = (0..16)
        .map(|x| terminal.backend().buffer()[(x, 13)].symbol())
        .collect::<String>();
    assert!(footer.contains("Esc Up/Dn Pg"));
}

#[test]
fn permission_choices_are_vertical_and_complete_before_selection() {
    let lines =
        crate::presentation::workspace_permission_lines(std::path::Path::new("/projects/example"));
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut rendered = String::new();
        for page in 0..100 {
            let mut more = true;
            terminal
                .draw(|frame| {
                    more = draw_permission_frame(
                        frame,
                        &lines,
                        ("Read only", "Trust folder"),
                        page,
                        false,
                        "↑↓ Enter ^C exit",
                        Palette { color: false },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let panel_width = width
                .saturating_sub(if width >= 40 { 4 } else { 0 })
                .min(76);
            let left = (width - panel_width) / 2;
            let rows = (0..height)
                .map(|y| {
                    (left..left + panel_width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            if more {
                assert!(
                    !rows
                        .iter()
                        .any(|row| row.contains("Read only") || row.contains("Trust folder"))
                );
            } else {
                let deny = rows
                    .iter()
                    .position(|row| row.starts_with("› Read only"))
                    .unwrap();
                assert!(rows[deny + 1].starts_with("  Trust folder"));
                assert!(rows.iter().any(|row| row.contains("^C exit")));
            }
            for row in &rows {
                if row.contains("Page ")
                    || row.contains("Read only")
                    || row.contains("Trust folder")
                    || row.contains("^C exit")
                {
                    continue;
                }
                rendered.push_str(row);
            }
            if !more {
                if width == 80 {
                    println!("{}", rows.join("\n"));
                }
                break;
            }
        }
        assert_eq!(
            rendered
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>(),
            lines
                .join("")
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        );
    }
}
