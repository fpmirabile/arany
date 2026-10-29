use super::*;

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
fn dense_settings_choices_keep_the_change_keys_visible() {
    let choices = [
        (1, "Slash menu"),
        (2, "Chat view"),
        (3, "Provider calls per task"),
        (4, "Tokens per task"),
        (5, "Active time per task"),
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
        let footer = rows[7].trim_end();
        assert!(
            SETTINGS_FOOTERS.contains(&footer) && footer.ends_with("Esc"),
            "{width}: {footer}"
        );
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
fn quick_actions_keep_focus_and_draft_guard_visible_without_color() {
    for width in [16, 20, 24, 40, 50, 80] {
        for height in [4, 6, 7, 8] {
            for color in [false, true] {
                for (selected, name) in [
                    (0, "Models"),
                    (1, "Agents"),
                    (2, "Resume"),
                    (3, "Setup"),
                    (4, "Usage"),
                ] {
                    for has_draft in [false, true] {
                        let mut terminal =
                            Terminal::new(TestBackend::new(width, height)).expect("quick selector");
                        terminal
                            .draw(|frame| {
                                draw_quick_actions_frame(
                                    frame,
                                    bottom_area(frame.area(), 7),
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
fn inline_custom_model_catalog_shows_declared_efforts() {
    let items = vec![ModelEntry {
        id: "same-long-prefix-model-alpha".into(),
        context_window_tokens: None,
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
                        ComposerHints::new(
                            Palette { color: false },
                            composer.newline_hint(KeyboardSupport::Legacy),
                        ),
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
