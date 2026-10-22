use super::*;
use crate::terminal::{ComposerEdit, TerminalInput};
use arany::Effort;
use ratatui::backend::{Backend, TestBackend};

fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
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
                        3,
                        "API key",
                        "Hidden input",
                        0,
                        Some("Enter confirms"),
                        palette,
                    ),
                    "text" => draw_setup_text_frame(
                        frame,
                        area,
                        "Workspace",
                        "Enter ID",
                        &Composer::default(),
                        None,
                        palette,
                    ),
                    "choices" => draw_setup_choices_frame(
                        frame,
                        area,
                        "Account",
                        "Choose one",
                        &[(1, "Saved"), (2, "New")],
                        0,
                        None,
                        palette,
                    ),
                    "warning" => {
                        draw_setup_warning_frame(
                            frame,
                            area,
                            "Consent",
                            "Review this change",
                            0,
                            false,
                            None,
                            palette,
                        );
                    }
                    "rewind" | "rewind review" => {
                        crate::terminal::rewind::draw_rewind_frame(
                            frame,
                            area,
                            "Checkpoints",
                            if panel == "rewind" { &[] } else { &lines },
                            &["Cancel".into(), "Restore".into()],
                            0,
                            0,
                            palette,
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
fn rewind_frame_corpus_keeps_every_panel_state_inert_and_paged_at_all_widths() {
    use crate::presentation::{
        escape_terminal,
        rewind::{KEEP, OVERWRITE},
    };
    use crate::terminal::rewind::draw_rewind_frame;
    let cases = [
        (
            "Checkpoints",
            vec![],
            vec![
                "Run 1 · prompt · 2 files changed since",
                "Before rewind · no code changes since",
            ],
        ),
        (
            "Restore choices",
            vec!["Before Run 1"],
            vec![
                "Restore code and conversation",
                "Restore conversation",
                "Restore code",
                "Never mind",
            ],
        ),
        (
            "Rewind preview",
            vec![
                "restore +1 -2",
                "delete +0 -3",
                "recreate +2 -0",
                "drifted +1 -1",
                "unavailable",
                "unsafe",
                "outside grant",
                "kept",
            ],
            vec!["Cancel", "Compare", "Restore"],
        ),
        (
            "Rewind blocked",
            vec![
                "Code restore blocked: unknown or open outcome; inspect files",
                "Restore conversation remains available",
                "Not rewindable: MCP external effect",
            ],
            vec!["Cancel", "Compare", "Resolve listed files"],
        ),
        (
            "Resolve listed files",
            vec!["Rewind refused: 2 files changed outside Arany"],
            vec!["Cancel", KEEP, OVERWRITE],
        ),
        (
            "Confirm overwrite",
            vec!["Their current content is saved first"],
            vec!["Cancel", "Overwrite after saving"],
        ),
        ("Compare", vec!["- old", "+ new"], vec!["Back", "Next"]),
        (
            "Checkpoint storage unavailable",
            vec!["Saved pre-images unavailable"],
            vec!["Retry", "Reset checkpoint storage", "Cancel"],
        ),
        (
            "Reset checkpoint storage?",
            vec!["Conversations and current files remain"],
            vec!["Cancel", "Reset checkpoint storage"],
        ),
        (
            "Rewind receipt",
            vec![
                "Restored 2 files to before Run 1",
                "Code restored; conversation unchanged",
                "Rewind outcome unknown: inspect files before continuing",
                "Older checkpoints pruned",
            ],
            vec!["Back"],
        ),
    ];
    for (width, height) in [(16, 8), (40, 12), (60, 16), (80, 24), (120, 30)] {
        for (title, body, options) in &cases {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut lines: Vec<_> = body.iter().map(|s| s.to_string()).collect();
            if !lines.is_empty() {
                lines.insert(0, title.to_string());
                lines.push(escape_terminal("src/\u{1b}]52;bad\u{7}\r\u{202e}path"));
            }
            let choices: Vec<_> = options.iter().map(|s| s.to_string()).collect();
            let mut page = 0;
            loop {
                let mut more = false;
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        more = draw_rewind_frame(
                            frame,
                            area,
                            title,
                            &lines,
                            &choices,
                            0,
                            page,
                            Palette { color: false },
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert!(buffer.content().iter().all(|cell| {
                    !cell
                        .symbol()
                        .chars()
                        .any(|c| c.is_control() || c == '\u{202e}')
                }));
                let text = (0..height)
                    .map(|y| buffer_row(buffer, y))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("Esc"), "{title} {width}: {text:?}");
                assert!(
                    buffer
                        .content()
                        .iter()
                        .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
                );
                if !more {
                    assert!(
                        text.contains(if lines.is_empty() {
                            "Run 1"
                        } else if *title == "Restore choices" && width < 50 {
                            "Code + chat"
                        } else {
                            options[0]
                        }),
                        "{title} {width}: {text:?}"
                    );
                    break;
                }
                assert!(
                    !text.contains("› Restore"),
                    "confirmation before final page"
                );
                page += 1;
                assert!(page < 200, "bounded panel pages");
            }
        }
    }
}

#[test]
fn help_keeps_every_registered_command_reachable_at_narrow_widths() {
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        for (selected, command, description) in [
            (0, "/help", "List commands"),
            (help_len() - 1, "/exit", "Exit Session"),
        ] {
            terminal
                .draw(|frame| {
                    draw_help_frame(
                        frame,
                        bottom_area(frame.area(), 20),
                        selected,
                        true,
                        Palette { color: false },
                    )
                })
                .expect("help frame");
            let buffer = terminal.backend().buffer();
            let rows = (0..height)
                .map(|y| buffer_row(buffer, y))
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
            .draw(|frame| {
                draw_help_frame(
                    frame,
                    bottom_area(frame.area(), 20),
                    index,
                    true,
                    Palette { color: false },
                )
            })
            .expect("help frame");
        let buffer = terminal.backend().buffer();
        let visible = (0..24)
            .map(|y| buffer_row(buffer, y))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(visible.contains(&format!("/{}", entry.name)), "{visible}");
    }
}

#[test]
fn narrow_classic_history_cue_keeps_export_action_visible() {
    let width = 20;
    let status = super::super::classic_history_status(width, true, false);
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
                &model,
                &Composer::default(),
                None,
                Palette { color: false },
                &[],
                Some(status),
            );
        })
        .expect("narrow history status");
    assert!(buffer_row(terminal.backend().buffer(), 7).contains(status));
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
                let area = frame.area();
                draw_setup_frame(
                    frame,
                    area,
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
            .map(|y| buffer_row(buffer, y))
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
                            let area = frame.area();
                            draw_setup_text_frame(
                                frame,
                                area,
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
                        .map(|y| buffer_row(buffer, y))
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
                    let area = frame.area();
                    draw_setup_choices_frame(
                        frame,
                        area,
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
                .map(|y| buffer_row(buffer, y))
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
            let area = frame.area();
            draw_setup_frame(
                frame,
                area,
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
fn dense_settings_choices_keep_the_change_keys_visible() {
    let choices = [
        (1, "Slash menu"),
        (2, "Chat view"),
        (3, "Goal provider calls"),
        (4, "Goal tokens"),
        (5, "Goal active time"),
        (6, "Default command timeout"),
        (7, "Maximum command timeout"),
        (8, "Build memory"),
        (9, "Build processes/threads"),
        (10, "Build writable project"),
        (11, "Build cache"),
    ];
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("settings terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_setup_choices_frame(
                frame,
                area,
                "Settings",
                "↑↓ selects a setting; ←→ changes it; Enter saves",
                &choices,
                0,
                None,
                Palette { color: false },
            )
        })
        .expect("dense Settings frame");
    let rows = (0..12)
        .map(|y| buffer_row(terminal.backend().buffer(), y))
        .collect::<Vec<_>>();
    assert!(rows.iter().any(|row| row.contains("↑↓ ←→ Enter Esc")));
    for width in [16, 40, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_settings_frame(
                    frame,
                    frame.area(),
                    &["Newline shortcut: Shift+Enter".into()],
                    0,
                    true,
                    Some(crate::terminal::NEWLINE_SHORTCUT_GUIDANCE),
                    Palette { color: false },
                );
            })
            .unwrap();
        let rows: Vec<_> = (0..8)
            .map(|y| buffer_row(terminal.backend().buffer(), y))
            .collect();
        assert!(rows.iter().any(|row| row.contains("Shift+Enter")));
        assert!(rows[7].contains("↑↓ ←→ Enter") || rows[7].contains("Enter saves"));
        let guidance = rows
            .iter()
            .map(|row| row.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            guidance.contains("\\ then Enter adds a newline."),
            "{width}: {guidance}"
        );
        assert!(guidance.contains("Widen for setup."), "{width}: {guidance}");
        if width == 80 {
            assert!(guidance.contains("Manual setup: send CSI-u ESC [ 13 ; M u"));
        }
    }
}

#[test]
fn newline_hint_recommends_a_received_preference_with_the_portable_fallback() {
    let model = PresentationModel {
        status_line: "ready".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: true,
        agent_switch: false,
    };
    for width in [40, 80] {
        for shortcut in crate::terminal::NewlineShortcut::ALL {
            let mut composer = Composer::default();
            composer.set_newline_shortcut(shortcut);
            composer.apply(TerminalInput::Character('a'));
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
                .unwrap();
            let footer = buffer_row(terminal.backend().buffer(), 6);
            assert!(footer.contains("\\ then Enter"));
            assert!(!footer.contains(shortcut.label()));
            composer.apply(TerminalInput::ModifiedEnter(shortcut));
            terminal
                .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
                .unwrap();
            let footer = buffer_row(terminal.backend().buffer(), 6);
            assert!(footer.contains(shortcut.label()), "{width}: {footer}");
            assert!(footer.contains("\\ Enter"), "{width}: {footer}");
        }
    }
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
                    let area = frame.area();
                    has_more = draw_setup_warning_frame(
                        frame,
                        area,
                        "ChatGPT plan consent",
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
                .map(|y| buffer_row(buffer, y))
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
                        let area = frame.area();
                        assert!(!draw_setup_warning_frame(
                            frame,
                            area,
                            "ChatGPT plan consent",
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
                    &model,
                    &Composer::default(),
                    None,
                    Palette { color: false },
                    &[HistoryRow {
                        text: "You:".into(),
                        speaker: MessageKind::User,
                        heading: true,
                        matched: false,
                        selected: None,
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
                            (0..8).any(|y| buffer_row(buffer, y).contains("Enter keeps")),
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
        empty_chat: true,
        agent_switch: false,
    };
    let rows = vec![
        HistoryRow {
            text: "You:".into(),
            speaker: MessageKind::User,
            heading: true,
            matched: false,
            selected: None,
        },
        HistoryRow {
            text: "  committed objective".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
            selected: None,
        },
        HistoryRow {
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
            text: "Arany · error:".into(),
            speaker: MessageKind::Error,
            heading: true,
            matched: false,
            selected: None,
        });
        feedback_rows.push(HistoryRow {
            text: "  Retry /setup; your draft is retained".into(),
            speaker: MessageKind::Error,
            heading: false,
            matched: false,
            selected: None,
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
            status.contains("working · openai/model · Safe conversation · History · Ctrl+L live"),
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
            selected: None,
        },
        HistoryRow {
            text: String::new(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
            selected: None,
        },
        rows[2].clone(),
        HistoryRow {
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
fn slash_argument_placeholder_is_visual_only_until_value_is_typed() {
    let model = PresentationModel {
        status_line: "idle".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: true,
        agent_switch: false,
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
            let row = buffer_row(buffer, 1);
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
    let row = buffer_row(buffer, 1);
    assert!(row.contains("/provider o[penai]"));
    assert!(!row.contains("<provider>"));
    assert_eq!(composer.text(), "/provider o");
    for (width, expected) in [(18, "/provider o[pe…]"), (20, "/provider o[penai]")] {
        let mut narrow = Terminal::new(TestBackend::new(width, 4)).expect("narrow test terminal");
        narrow
            .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
            .expect("narrow frame");
        let buffer = narrow.backend().buffer();
        let row = buffer_row(buffer, 1);
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
    let row = buffer_row(buffer, 1);
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
    let row = buffer_row(buffer, 1);
    assert!(row.contains("/provider o"));
    assert!(!row.contains("penai"));
    let status = buffer_row(buffer, 3);
    assert!(status.starts_with("idle"));
    composer.apply(TerminalInput::End);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("end-of-draft frame");
    let buffer = terminal.backend().buffer();
    let row = buffer_row(buffer, 1);
    assert!(row.contains("/provider o[penai]"));

    composer.apply(TerminalInput::Backspace);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("frame");
    let buffer = terminal.backend().buffer();
    let row = buffer_row(buffer, 1);
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
    let row = buffer_row(buffer, 1);
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
    let row = buffer_row(buffer, 1);
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
    let row = buffer_row(buffer, 1);
    assert!(row.contains("/prov"));
    assert!(!row.contains("[ider"));
    assert_eq!(composer.text(), "/prov");
    composer.apply(TerminalInput::Tab);
    terminal
        .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color: false }))
        .expect("completed command argument frame");
    let buffer = terminal.backend().buffer();
    let row = buffer_row(buffer, 1);
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
        ("/he", "help", "Cmd"),
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
                    let row = |y| buffer_row(buffer, y);
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
                    assert!(row(height - 1).starts_with("Tab completes"));
                    assert!(row(input_y + 1).contains("Enter"));
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
            for height in [8, 24, 26] {
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
                    let row = |y| buffer_row(buffer, y);
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
                        let shown = 18;
                        assert_eq!(
                            labels.len(),
                            shown + 1,
                            "24-row Commands pane clips the registry tail"
                        );
                        assert!(
                            labels[0].contains(&format!("1-{shown}/{}", help_len())),
                            "24-row Commands header omits the visible range: {labels:?}"
                        );
                        let hidden = help_entry(help_len() - 1).expect("registered command");
                        assert!(
                            !labels
                                .iter()
                                .any(|row| row.contains(&format!("/{}", hidden.name))),
                            "24-row Commands pane shows clipped /{}: {labels:?}",
                            hidden.name
                        );
                    }
                    if width >= 79 && height == 26 && !skills_tab && !last {
                        assert_eq!(
                            labels.len(),
                            21,
                            "large Commands pane shows its header and 20 entries"
                        );
                        assert!(
                            labels[0].contains(&format!("1-20/{}", help_len())),
                            "clipped Commands header omits the visible range: {labels:?}"
                        );
                        for index in 0..20 {
                            let entry = help_entry(index).expect("registered command");
                            assert!(
                                labels
                                    .iter()
                                    .any(|row| row.contains(&format!("/{}", entry.name))),
                                "large Commands pane omits /{}: {labels:?}",
                                entry.name
                            );
                        }
                        assert!(labels.iter().any(|row| {
                            row.contains("Slash menu, chat view and newline shortcut")
                        }));
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
    assert!(!(0..8).any(|y| { buffer_row(buffer, y).contains("> Cmd /status") }));
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
                let row = buffer_row(buffer, 5);
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
        let row = buffer_row(buffer, 5);
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
        empty_chat: true,
        agent_switch: false,
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
                    .attach_image(arany::test_support::test_image())
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
                let rows = (0..8).map(|y| buffer_row(buffer, y)).collect::<Vec<_>>();
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
                    assert!(footer.contains("\\ then Enter newline"));
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
        empty_chat: true,
        agent_switch: false,
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
            id: arany::SessionId::new(),
            title: if index == 14 {
                "Hostile\u{1b}[31m\u{202e} title".into()
            } else {
                format!("Session {index}")
            },
            last_sequence: index,
            created_at: "2026-10-06 12:00:00".into(),
            last_activity_at: "2026-10-06 12:05:00".into(),
            defaults: arany::SessionDefaults {
                provider: Some("chatgpt".into()),
                model: Some("gpt-6.1-sol".into()),
                ..arany::SessionDefaults::default()
            },
        })
        .collect::<Vec<_>>();
    for width in [16, 20, 24, 40, 50, 80] {
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_session_picker_frame(
                    frame,
                    bottom_area(frame.area(), 12),
                    &items,
                    14,
                    true,
                    Palette { color: false },
                )
            })
            .expect("picker frame");
        let buffer = terminal.backend().buffer();
        let row = |y| buffer_row(buffer, y);
        assert!(row(0).contains("Resume · 15"));
        assert!(row(6).starts_with("› 15. Hostile"), "{width}: {}", row(6));
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
        assert!(buffer[(0, 6)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(0, 5)].modifier.contains(Modifier::REVERSED));
        assert!(row(5).starts_with("  14. Session 13"));
        terminal
            .draw(|frame| {
                draw_session_picker_frame(
                    frame,
                    bottom_area(frame.area(), 12),
                    &items,
                    14,
                    true,
                    Palette { color: true },
                )
            })
            .expect("colored picker frame");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
        assert!(buffer[(0, 6)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(0, 9)].fg, Color::Reset);
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
        "Goal tokens: unlimited".to_owned(),
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
                                    draw_quick_actions_frame(frame, area, 1, armed, false, palette);
                                }
                                "resume" => {
                                    draw_session_picker_frame(
                                        frame, area, &items, 0, armed, palette,
                                    );
                                }
                                "help" => draw_help_frame(frame, area, 0, armed, palette),
                                "settings" => draw_settings_frame(
                                    frame, area, &settings, 3, armed, None, palette,
                                ),
                                _ => draw_agent_inspector_frame(
                                    frame, area, &agents, 0, armed, None, palette,
                                ),
                            }
                        })
                        .expect("dialog frame");
                    let buffer = terminal.backend().buffer();
                    let rows = (0..height)
                        .map(|y| buffer_row(buffer, y))
                        .collect::<Vec<_>>();
                    let context = format!("{name} armed {armed} at {width}x{height}: {rows:?}");
                    assert_eq!(area.bottom(), height, "{context}");
                    // The key hint is the arming cue.
                    assert_eq!(
                        rows[usize::from(height - 1)].contains("Esc"),
                        armed,
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
                                    bottom_area(frame.area(), 6),
                                    selected,
                                    true,
                                    has_draft,
                                    Palette { color },
                                )
                            })
                            .expect("quick frame");
                        let buffer = terminal.backend().buffer();
                        let rows = (0..height)
                            .map(|y| buffer_row(buffer, y))
                            .collect::<Vec<_>>();
                        let row = rows
                            .iter()
                            .position(|row| row.starts_with(&format!("› {}. {name}", selected + 1)))
                            .expect("focused action visible");
                        let footer = rows.last().expect("footer");
                        assert!(footer.contains("Esc"));
                        if selected == 2 && has_draft {
                            assert!(footer.contains("draft") || footer.contains("Draft"));
                            if (20..40).contains(&width) {
                                assert!(footer.contains("Submit draft"));
                            }
                        } else {
                            assert!(footer.contains("Enter"));
                        }
                        assert!(
                            buffer[(0, row as u16)]
                                .modifier
                                .contains(Modifier::REVERSED)
                        );
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
                                &crate::terminal::ModelPicker {
                                    items: &items,
                                    efforts: &efforts,
                                    selected: 4095,
                                    exact_custom: false,
                                    filter,
                                    invalid: false,
                                    catalog_state,
                                },
                                true,
                                &composer,
                                &model,
                                Palette { color },
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
        empty_chat: true,
        agent_switch: false,
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
                        true,
                        &composer,
                        &model,
                        Palette { color: false },
                    );
                })
                .expect("custom model panel");
            let buffer = terminal.backend().buffer();
            let rows = (0..8).map(|y| buffer_row(buffer, y)).collect::<Vec<_>>();
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
fn agent_picker_windows_rows_and_shows_search_feedback_without_false_focus() {
    let choices = (1..=12)
        .map(|ordinal| format!("Run 1: child {ordinal}; active"))
        .collect::<Vec<_>>();
    let mut model = inspector_model(
        Some(11),
        choices,
        vec![
            "Current Run: 13 agents; children: 12".into(),
            "Agent 12/12: child 12; active".into(),
        ],
    );
    model.search = "Search: child · 12 of 13 agents".into();
    let draw = |model: &AgentInspectorModel, notice: Option<&str>| {
        let mut terminal = Terminal::new(TestBackend::new(80, 23)).expect("picker terminal");
        terminal
            .draw(|frame| {
                draw_agent_inspector_frame(
                    frame,
                    frame.area(),
                    model,
                    0,
                    true,
                    notice,
                    Palette { color: false },
                );
            })
            .expect("picker frame");
        (0..23)
            .map(|y| {
                buffer_row(terminal.backend().buffer(), y)
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    // Ten rows fit; the window ends at the focused twelfth row.
    let rows = draw(&model, None);
    assert_eq!(rows[1], "Search: child · 12 of 13 agents");
    assert_eq!(rows[2], "  3. Run 1: child 3; active");
    assert_eq!(rows[11], "› 12. Run 1: child 12; active");
    assert_eq!(rows[12], "Agent 12/12: child 12; active");
    assert_eq!(
        rows[22],
        "Enter open · Esc close · Up/Down agent · type to search · PgUp/PgDn"
    );
    assert_eq!(rows.iter().filter(|row| row.starts_with('›')).count(), 1);
    // No match shows the search and its feedback, with no focused row to activate.
    let mut none = inspector_model(
        None,
        Vec::new(),
        vec![
            "Current Run: 13 agents; children: 12".into(),
            "No matching agents; edit the search".into(),
        ],
    );
    none.search = "Search: zz · no matching agents".into();
    let rows = draw(&none, Some("Search takes up to 64 ASCII characters"));
    assert_eq!(rows[1], "Search: zz · no matching agents");
    assert_eq!(rows[2], "No matching agents; edit the search");
    assert_eq!(rows[22], "Search takes up to 64 ASCII characters");
    assert!(rows.iter().all(|row| !row.contains('›')));
}

#[test]
fn agent_picker_reserves_all_ten_choice_rows_before_short_detail() {
    let choices = (1..=10)
        .map(|ordinal| format!("Run 1: child {ordinal}; active"))
        .collect::<Vec<_>>();
    let model = inspector_model(
        Some(9),
        choices,
        vec![
            "Current Run: 10 agents; children: 10".into(),
            "Agent 10/10: child 10; active".into(),
            "Summary: short".into(),
        ],
    );
    // Two detail lines plus three chrome rows and ten choice rows.
    let height = 2 + 3 + 10;
    let mut terminal = Terminal::new(TestBackend::new(80, height)).expect("picker terminal");
    terminal
        .draw(|frame| {
            draw_agent_inspector_frame(
                frame,
                frame.area(),
                &model,
                0,
                true,
                None,
                Palette { color: false },
            );
        })
        .expect("picker frame");
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer_row(buffer, 2).trim_end(),
        "  1. Run 1: child 1; active"
    );
    assert_eq!(
        buffer_row(buffer, 11).trim_end(),
        "› 10. Run 1: child 10; active"
    );
    assert_eq!(
        buffer_row(buffer, 12).trim_end(),
        "Agent 10/10: child 10; active"
    );
    assert_eq!(buffer_row(buffer, 13).trim_end(), "Summary: short");
}

#[test]
fn agent_inspector_keeps_details_and_keyboard_help_in_viewport() {
    for width in [40, 50, 79, 80, 120] {
        let backend = TestBackend::new(width, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let model = inspector_model(
            Some(1),
            vec![
                "Run 1: primary; finished".into(),
                "Run 1: child 1; Failed".into(),
            ],
            vec![
                "Last Run: 2 agents; children: 1".into(),
                "History: 2 agents in 1 recent Runs".into(),
                "Next Run: single".into(),
                "Agent 2/2: child 1; Failed".into(),
                "Objective: hostile\\u{001b}[31m".into(),
                "Summary: bounded details".into(),
            ],
        );
        terminal
            .draw(|frame| {
                draw_agent_inspector_frame(
                    frame,
                    bottom_area(frame.area(), 14),
                    &model,
                    0,
                    true,
                    None,
                    Palette { color: false },
                )
            })
            .expect("agent frame");
        let buffer = terminal.backend().buffer();
        let row = |y| buffer_row(buffer, y);
        assert!(row(0).contains("Last Run: 2 agents; children: 1"));
        assert_eq!(row(1).trim_end(), "Type to search 2 agents");
        assert!(row(2).starts_with("  1. Run 1: primary; finished"));
        assert!(row(3).starts_with("› 2. Run 1: child 1; Failed"));
        assert!(row(6).contains("child 1; Failed"));
        assert!(row(7).contains("hostile\\u{001b}[31m"));
        assert!(row(13).contains("Esc close"));
        let area = Rect::new(0, 0, width, 14);
        assert_eq!(agent_picker_index(area, 0, 2, Some(1), 2), Some(0));
        assert_eq!(agent_picker_index(area, 0, 3, Some(1), 2), Some(1));
        assert_eq!(agent_picker_index(area, 0, 1, Some(1), 2), None);
        assert_eq!(agent_picker_index(area, 0, 4, Some(1), 2), None);
        assert_eq!(agent_picker_index(area, width, 2, Some(1), 2), None);
        assert_eq!(agent_picker_index(area, 0, 2, None, 0), None);
        // Eight choices reserve their rows in 14 lines; the detail keeps the rest.
        assert_eq!(agent_picker_index(area, 0, 2, Some(7), 8), Some(0));
        assert_eq!(agent_picker_index(area, 0, 9, Some(7), 8), Some(7));
        assert_eq!(agent_picker_index(area, 0, 10, Some(7), 8), None);
        assert_eq!(agent_picker_index(area, 0, 2, None, 8), Some(0));
        for y in 0..14 {
            assert!(!row(y).contains('\u{1b}'));
        }
    }

    let mut terminal = Terminal::new(TestBackend::new(16, 14)).expect("narrow terminal");
    let model = inspector_model(
        Some(0),
        vec!["Run 1: primary; finished".into()],
        vec![
            "Last Run: 1 agent; children: 0".into(),
            "Next Run: single".into(),
        ],
    );
    terminal
        .draw(|frame| {
            draw_agent_inspector_frame(
                frame,
                bottom_area(frame.area(), 14),
                &model,
                0,
                true,
                None,
                Palette { color: false },
            )
        })
        .expect("narrow agent frame");
    let footer = (0..16)
        .map(|x| terminal.backend().buffer()[(x, 13)].symbol())
        .collect::<String>();
    assert!(footer.contains("Esc Up/Dn Pg"));
}

fn is_choice_row(row: &str) -> bool {
    matches!(
        row.trim_end().trim_start_matches(['›', ' ']),
        "Read only" | "Trust folder"
    )
}

fn profile_frame_lines() -> Vec<String> {
    let mut lines =
        crate::presentation::workspace_permission_lines(std::path::Path::new("/projects/example"));
    lines.extend(crate::presentation::RUNTIME_PROFILE_LINES.map(String::from));
    lines
}

#[test]
fn permission_profile_rows_wrap_at_spaces_with_a_hanging_indent() {
    let lines = vec![
        "Do you trust this folder?".to_owned(),
        "/p".to_owned(),
        String::new(),
        "  [ ] 3. Node v20.19.2 · ~/.local/share/fnm/node-versions/v20.19.2/installation"
            .to_owned(),
    ];
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_permission_frame(
                frame,
                area,
                &lines,
                PermissionFrameState {
                    choices: &["Read only", "Trust folder"],
                    page: 0,
                    focus: 0,
                    profile_start: Some(3),
                    profile_count: 1,
                    focused_profile: None,
                    profile_selection_enabled: Some(true),
                    footer: None,
                    palette: Palette { color: false },
                    boxed: false,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let content = (0..12)
        .map(|y| buffer_row(buffer, y).chars().skip(2).collect::<String>())
        .map(|row| row.trim_end().to_owned())
        .collect::<Vec<_>>();
    let first = content
        .iter()
        .position(|row| row == "  [ ] 3. Node v20.19.2 ·")
        .expect("profile label row");
    assert!(content[first + 1].starts_with("      ~/.local/share/fnm/"));
    let tail = content[first + 1..]
        .iter()
        .take_while(|row| row.starts_with("      "))
        .map(|row| row.trim())
        .collect::<String>();
    assert!(tail.contains("~/.local/share/fnm/node-versions/v20.19.2/installation"));
}

#[test]
fn permission_long_path_hard_breaks_only_at_the_row_edge() {
    let line = "  [ ] 3. Node v20.19.2 · ~/.local/share/fnm/node-versions/v20.19.2/installation";
    let lines = vec![
        "Do you trust this folder?".to_owned(),
        "/p".to_owned(),
        String::new(),
        line.to_owned(),
    ];
    let mut terminal = Terminal::new(TestBackend::new(16, 8)).unwrap();
    let mut rows = Vec::new();
    for page in 0..100 {
        let mut progress = PermissionFrameProgress { more: true, page };
        terminal
            .draw(|frame| {
                let area = frame.area();
                progress = draw_permission_frame(
                    frame,
                    area,
                    &lines,
                    PermissionFrameState {
                        choices: &["Read only", "Trust folder"],
                        page,
                        focus: 0,
                        profile_start: Some(3),
                        profile_count: 1,
                        focused_profile: None,
                        profile_selection_enabled: Some(true),
                        footer: None,
                        palette: Palette { color: false },
                        boxed: false,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        rows.extend((0..3).map(|y| buffer_row(buffer, y).trim_end().to_owned()));
        if !progress.more {
            break;
        }
    }
    let first = rows
        .iter()
        .position(|row| row.contains("[ ] 3."))
        .expect("profile row");
    let profile = rows[first..]
        .iter()
        .take_while(|row| row.starts_with("  [") || row.starts_with("      "))
        .collect::<Vec<_>>();
    assert!(profile.len() > 1);
    assert!(profile[1..].iter().all(|row| row.starts_with("      ")));
    let joined = profile
        .iter()
        .flat_map(|row| row.chars())
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    assert_eq!(
        joined,
        line.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    );
}

#[test]
fn permission_frame_fills_the_terminal_like_chat() {
    let mut lines = profile_frame_lines();
    let profile_start = lines.len();
    lines.extend((3..9).map(|index| {
        format!(
            "    [ ] {index}. Node v20.19.2 · ~/.local/share/fnm/node-versions/v20.19.2/installation"
        )
    }));
    for (width, height, expect_more) in [(100, 30, false), (60, 20, true)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut progress = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                progress = Some(draw_permission_frame(
                    frame,
                    area,
                    &lines,
                    PermissionFrameState {
                        choices: &["Read only", "Trust this folder"],
                        page: 0,
                        focus: 0,
                        profile_start: Some(profile_start),
                        profile_count: 6,
                        focused_profile: None,
                        profile_selection_enabled: Some(true),
                        footer: None,
                        palette: Palette { color: false },
                        boxed: false,
                    },
                ));
            })
            .unwrap();
        let progress = progress.unwrap();
        assert_eq!(progress.more, expect_more);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 0)].symbol(), "D");
        for y in 0..height {
            assert_eq!(buffer[(0, y)].symbol(), " ");
            if width >= 40 {
                assert_eq!(buffer[(1, y)].symbol(), " ");
            }
        }
        if expect_more {
            let rows = (0..height)
                .map(|y| buffer_row(buffer, y))
                .collect::<Vec<_>>();
            let page = rows.iter().position(|row| row.contains("Page 1/")).unwrap();
            assert_eq!(page, usize::from(height) - 4);
        } else {
            assert!(buffer_row(buffer, 26).contains("› Read only"));
            assert!(buffer_row(buffer, 29).contains("Ctrl+C exit"));
        }
    }
}

#[test]
fn trust_dialog_takes_its_content_height_and_pages_beyond_the_screen() {
    let state = |profile_start| PermissionFrameState {
        choices: &["Continue read only", "Trust this folder"],
        page: 0,
        focus: 0,
        profile_start,
        profile_count: 0,
        focused_profile: None,
        profile_selection_enabled: None,
        footer: None,
        palette: Palette { color: false },
        boxed: false,
    };
    let fitting =
        crate::presentation::workspace_permission_lines(std::path::Path::new("/home/user/project"));
    let mut long = profile_frame_lines();
    let profile_start = long.len();
    long.extend((3..20).map(|index| {
        format!(
            "    [ ] {index}. Node v20.19.2 · ~/.local/share/fnm/node-versions/v20.19.2/installation"
        )
    }));
    for (lines, profile_start, pages) in
        [(&fitting, None, false), (&long, Some(profile_start), true)]
    {
        // As in Codex, the Classic viewport is min(screen, the dialog's content height).
        let content = permission_height(lines, 80, false, profile_start);
        let height = content.min(24);
        assert_eq!(content > 24, pages, "content of {} rows", content);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let area = Rect::new(0, 24 - height, 80, height);
        let mut progress = None;
        terminal
            .draw(|frame| {
                progress = Some(draw_permission_frame(
                    frame,
                    area,
                    lines,
                    state(profile_start),
                ));
            })
            .unwrap();
        let rows = (0..24)
            .map(|y| buffer_row(terminal.backend().buffer(), y))
            .collect::<Vec<_>>();
        assert_eq!(progress.unwrap().more, pages, "{rows:?}");
        assert!(
            rows[usize::from(area.y)].contains("Do you trust this folder?"),
            "{rows:?}"
        );
        if pages {
            assert!(rows.iter().any(|row| row.contains("Page 1/")), "{rows:?}");
        } else {
            // Every content row and both choices fit on the one page.
            assert!(
                rows.iter()
                    .any(|row| row.contains("Every edit and command"))
            );
            assert!(rows.iter().any(|row| row.contains("› Continue read only")));
            assert!(rows.iter().any(|row| row.contains("Trust this folder")));
        }
    }
}

#[test]
fn wrap_cells_keeps_words_and_indents_continuations() {
    assert_eq!(wrap_cells("aaa bbb ccc", 7, 2), ["aaa bbb", "  ccc"]);
    assert_eq!(wrap_cells("abcdefghij", 4, 1), ["abcd", " efg", " hij"]);
    assert_eq!(wrap_cells("", 5, 0), [""]);
    assert_eq!(wrap_cells("日本語", 5, 0), ["日本", "語"]);
}

#[test]
fn permission_choices_are_vertical_and_complete_before_selection() {
    let lines =
        crate::presentation::workspace_permission_lines(std::path::Path::new("/projects/example"));
    for (width, height) in [(16, 8), (40, 12), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut rendered = String::new();
        for page in 0..100 {
            let mut progress = PermissionFrameProgress { more: true, page };
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    progress = draw_permission_frame(
                        frame,
                        area,
                        &lines,
                        PermissionFrameState {
                            choices: &["Read only", "Trust folder"],
                            page,
                            focus: 0,
                            profile_start: None,
                            profile_count: 0,
                            focused_profile: None,
                            profile_selection_enabled: None,
                            footer: Some("↑↓ Enter ^C exit"),
                            palette: Palette { color: false },
                            boxed: false,
                        },
                    );
                })
                .unwrap();
            assert_eq!(progress.page, page);
            let buffer = terminal.backend().buffer();
            let left = if width >= 40 { 2 } else { 0 };
            let panel_width = width - left * 2;
            let rows = (0..height)
                .map(|y| {
                    (left..left + panel_width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            if progress.more {
                assert!(!rows.iter().any(|row| is_choice_row(row)));
            } else {
                let deny = rows
                    .iter()
                    .position(|row| row.starts_with("› Read only"))
                    .unwrap();
                assert!(rows[deny + 1].starts_with("  Trust folder"));
                assert!(rows.iter().any(|row| row.contains("^C exit")));
            }
            for row in &rows {
                if row.contains("Page ") || is_choice_row(row) || row.contains("^C exit") {
                    continue;
                }
                rendered.push_str(row);
            }
            if !progress.more {
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
                    &model,
                    &Composer::default(),
                    None,
                    Palette { color: false },
                    &[],
                    Some("History · Ctrl+L live"),
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
fn inline_tool_approval_corpus_pages_complete_inert_actions_and_numbered_choices() {
    use crate::presentation::ToolApprovalChoice::{AutoEdits, ExactCommand, GitPrefix, No, Yes};
    for width in [120, 80, 50, 16] {
        for (name, body, choices, focus) in [
            (
                "one-page",
                vec!["Create src/example.txt".to_owned()],
                vec![Yes, No],
                0,
            ),
            (
                "multi-page",
                (0..80)
                    .map(|index| format!("action-line-{index:02}"))
                    .collect(),
                vec![Yes, AutoEdits, No],
                0,
            ),
            (
                "git-prefix",
                vec!["Commit 1 path".to_owned()],
                vec![
                    Yes,
                    GitPrefix(arany::permissions::TypedGitPrefix::Commit),
                    No,
                ],
                1,
            ),
            (
                "unguarded",
                vec![
                    "Run outside the Guard? (unguarded)".to_owned(),
                    "Risk: it runs as you, without a sandbox".into(),
                ],
                vec![Yes, ExactCommand, No],
                2,
            ),
            (
                "hostile",
                vec![
                    crate::presentation::escape_terminal(
                        "src/\u{1b}]52;c;bad\u{7}\r\u{202e}界e\u{301}",
                    ),
                    "  1. Yes (untrusted content)".into(),
                ],
                vec![Yes, No],
                0,
            ),
        ] {
            let mut lines = vec!["Tool approval".into()];
            lines.extend(body.clone());
            let mut terminal = Terminal::new(TestBackend::new(width, 50)).unwrap();
            let height = super::tool_approval_height(&lines, &choices, width).min(50);
            let mut page = 0;
            let mut seen = String::new();
            loop {
                let mut more = true;
                terminal
                    .draw(|frame| {
                        let area = Rect::new(0, 50 - height, width, height);
                        more = super::draw_tool_approval_frame(
                            frame,
                            area,
                            &lines,
                            &choices,
                            page,
                            Some(focus),
                            false,
                        );
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert_eq!(
                    buffer_row(buffer, 0).trim(),
                    if height == 50 { "Tool approval" } else { "" },
                    "{name} width {width}"
                );
                let mut frame_text = String::new();
                for row in 0..50 {
                    frame_text.push_str(&buffer_row(buffer, row));
                    frame_text.push('\n');
                }
                assert!(!frame_text.contains('\u{1b}') && !frame_text.contains('\u{202e}'));
                seen.push_str(&frame_text);
                let squashed = frame_text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(
                    squashed.contains("Do you want to proceed?"),
                    "{name} width {width}: {squashed}"
                );
                assert!(squashed.contains("1. Yes"));
                if !more {
                    break;
                }
                assert!(page < 100, "bounded approval pages");
                page += 1;
            }
            let squashed = seen.split_whitespace().collect::<Vec<_>>().join(" ");
            if name == "multi-page" {
                for line in body {
                    assert!(seen.contains(&line), "missing {line} at {width}");
                }
                assert!(page > 0);
            }
            if name == "unguarded" {
                assert!(squashed.contains("this exact command"));
            }
            if name == "git-prefix" {
                assert!(
                    squashed.contains("allow git commit in this folder from the next Run"),
                    "{width}: {squashed}"
                );
            }
            let focused_rows = (0..50)
                .map(|row| buffer_row(terminal.backend().buffer(), row))
                .collect::<Vec<_>>();
            assert!(
                focused_rows
                    .iter()
                    .any(|row| row.contains(&format!("› {}.", focus + 1)))
            );
            terminal
                .draw(|frame| {
                    let area = Rect::new(0, 50 - height, width, height);
                    super::draw_tool_approval_frame(frame, area, &lines, &choices, 0, None, false);
                })
                .unwrap();
            let unarmed = (0..50)
                .map(|row| buffer_row(terminal.backend().buffer(), row))
                .collect::<String>();
            assert!(
                unarmed.contains("1. Yes") && !unarmed.contains('›'),
                "{name} width {width}: unarmed choices show no focus"
            );
        }
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
                    0,
                    Some(0),
                    true,
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
            );
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            terminal
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
                            &inspector,
                            scroll,
                            true,
                            None,
                            Palette { color },
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
        let rows = super::super::history::display_rows(
            MessageKind::Tool,
            &crate::presentation::tool_summary(&tool),
            usize::from(width),
        );
        for color in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
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
                },
                "untrusted output",
            );
            let observation = limited.observation.as_mut().unwrap();
            observation.intent.limits.runtime_ms = 120_000;
            observation.disposition = arany::ToolDisposition::Limit;
            observation.limit_cause = Some(arany::LimitCause::Timeout);
            observation.cache_update = arany::CacheUpdate::NotAttempted;
            let rows = super::super::history::display_rows(
                MessageKind::Tool,
                &crate::presentation::tool_summary(&limited),
                usize::from(width),
            );
            terminal
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
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer_row(buffer, 1).trim_end(), "⏺ Command(sh )");
            assert_eq!(
                buffer_row(buffer, 2).trim_end(),
                "  ⎿ limit: timed out after 2 min; /settings raises it"
            );
            let output = serde_json::json!({"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,
                "stdout":"head\n…[123398 bytes, 12338 lines omitted; 123408 bytes, 12340 lines total]…\ntail\n",
                "stderr":"", "stdout_total":{"bytes":123408,"lines":12340},
                "stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string();
            let command = crate::presentation::tool_fixture(limited.intent.call.clone(), &output);
            let rows = super::super::history::display_rows(
                MessageKind::Tool,
                &crate::presentation::tool_summary(&command),
                usize::from(width),
            );
            terminal
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
            let rows = super::super::history::display_rows(
                MessageKind::Tool,
                &crate::presentation::tool_summary(&long),
                usize::from(width),
            );
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
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
fn selection_highlight_is_inverse_video_on_text_cells_without_color() {
    let rows = [
        HistoryRow {
            text: "  selected text".into(),
            speaker: MessageKind::Assistant,
            heading: false,
            matched: false,
            selected: Some((2, 10)),
        },
        HistoryRow {
            text: "  body".into(),
            speaker: MessageKind::User,
            heading: false,
            matched: false,
            selected: Some((2, 6)),
        },
        HistoryRow {
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
        let mut rows = super::super::history::display_rows(
            MessageKind::Commentary,
            "hostile \u{1b}]52;c;eA==\u{7} tail",
            usize::from(width),
        );
        rows.extend(super::super::history::display_rows(
            MessageKind::Assistant,
            &"line\n".repeat(usize::from(height)),
            usize::from(width),
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
                            transcript.label.as_str().into(),
                            armed,
                            None,
                            fullscreen,
                            palette,
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
fn empty_composer_names_agent_keys_only_when_the_run_has_children() {
    for (width, children, draft, expected) in [
        (80, true, "", Some("Alt+Left/Right agents")),
        (50, true, "", Some("Alt+Left/Right agents")),
        (30, true, "", Some("Alt+←/→ agents")),
        (16, true, "", None),
        (80, false, "", None),
        (80, true, "x", None),
    ] {
        let model = PresentationModel {
            status_line: "ready".into(),
            activity_lines: Vec::new(),
            setup_required: false,
            draft_action: DraftAction::Submit,
            empty_chat: false,
            agent_switch: children,
        };
        let mut composer = Composer::default();
        for character in draft.chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| {
                draw_frame_with_history(
                    frame,
                    &model,
                    &composer,
                    None,
                    Palette { color: false },
                    &[],
                    None,
                );
            })
            .unwrap();
        let rows = (0..12)
            .map(|y| buffer_row(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");
        let context = format!("{width} cols, children {children}, draft {draft:?}: {rows}");
        match expected {
            Some(hint) => assert!(rows.contains(hint), "{context}"),
            None => assert!(!rows.contains("agents"), "{context}"),
        }
    }
}

#[test]
fn child_composer_is_labelled_literal_and_disabled_after_settlement() {
    let mut composer = Composer::text_only();
    composer.restore_prompt("/literal child note").unwrap();
    for (width, height) in [(80, 24), (16, 8), (8, 2)] {
        for editable in [false, true] {
            let backend = TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_agent_chat_frame(
                        frame,
                        bottom_area(frame.area(), AGENT_CHAT_ROWS),
                        AgentChatFrame {
                            label: "child 2",
                            draft: Some(&composer),
                            editable,
                        },
                        true,
                        Some("draft retained"),
                        true,
                        Palette { color: false },
                    )
                })
                .unwrap();
            let screen = (0..height)
                .map(|y| buffer_row(terminal.backend().buffer(), y))
                .collect::<Vec<_>>()
                .join("\n");
            if width == 80 {
                assert!(screen.contains(if editable {
                    "viewing child 2 (message)"
                } else {
                    "viewing child 2 (read-only)"
                }));
                assert!(screen.contains("> /literal child note"));
            }
            assert!(!composer.has_open_menu());
        }
    }
}
