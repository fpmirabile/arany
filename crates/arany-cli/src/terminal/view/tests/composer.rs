use super::*;

#[test]
fn image_chip_rectangles_match_only_visible_full_chips() {
    let model = PresentationModel {
        status_line: "ready".into(),
        activity_lines: Vec::new(),
        setup_required: false,
        draft_action: DraftAction::Submit,
        empty_chat: false,
        agent_switch: false,
    };
    let mut composer = Composer::default();
    composer.insert_paste("task").unwrap();
    for _ in 0..2 {
        composer
            .attach_image(arany::test_support::test_image())
            .unwrap();
    }
    composer.apply(TerminalInput::Character('!'));
    for width in [40, 80, 120] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        let mut chips = Vec::new();
        terminal
            .draw(|frame| {
                chips = draw_frame_with_history_and_chips(
                    frame,
                    FrameContent {
                        model: &model,
                        composer: &composer,
                        notice: None,
                        hints: ComposerHints::new(Palette { color: false }, "\\ then Enter"),
                        history_rows: &[],
                        history_status: None,
                    },
                )
                .image_chips;
            })
            .unwrap();
        assert_eq!(chips.len(), 2, "{width} columns");
        for (index, area) in chips {
            assert!(area.width > 0 && area.height == 1);
            let rendered = (area.x..area.right())
                .map(|column| terminal.backend().buffer()[(column, area.y)].symbol())
                .collect::<String>();
            assert_eq!(rendered, format!("[Image #{}]", index + 1));
        }
    }

    let mut narrow = Terminal::new(TestBackend::new(16, 8)).unwrap();
    let mut chips = Vec::new();
    narrow
        .draw(|frame| {
            chips = draw_frame_with_history_and_chips(
                frame,
                FrameContent {
                    model: &model,
                    composer: &composer,
                    notice: None,
                    hints: ComposerHints::new(Palette { color: false }, "\\ then Enter"),
                    history_rows: &[],
                    history_status: None,
                },
            )
            .image_chips;
        })
        .unwrap();
    assert!(chips.is_empty());
}

#[test]
fn image_preview_cells_are_hidden_and_skipped_for_terminal_graphics() {
    let history_text = |buffer: &ratatui::buffer::Buffer| {
        (3..10)
            .map(|column| buffer[(column, 4)].symbol())
            .collect::<String>()
    };
    let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new("History text"), Rect::new(3, 4, 8, 1));
        })
        .unwrap();
    let mut hidden = false;
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new("History text"), Rect::new(3, 4, 8, 1));
            draw_image_preview(
                frame,
                Rect::new(2, 2, 10, 6),
                "Image 1: PNG",
                PreviewInterior::Graphics,
            );
            hidden = frame.buffer_mut()[(3, 4)]
                .modifier
                .contains(Modifier::HIDDEN);
            assert_eq!(frame.buffer_mut()[(3, 4)].diff_option, CellDiffOption::Skip);
        })
        .unwrap();
    assert!(hidden);
    assert_eq!(history_text(terminal.backend().buffer()), "History");

    let graphics_cell = ratatui::buffer::Cell::new("█");
    terminal
        .backend_mut()
        .draw([(3, 4, &graphics_cell), (3, 5, &graphics_cell)].into_iter())
        .unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new("History text"), Rect::new(3, 4, 8, 1));
        })
        .unwrap();
    assert_eq!(history_text(terminal.backend().buffer()), "History");
    assert_eq!(terminal.backend().buffer()[(3, 5)].symbol(), " ");
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
            terminal
                .draw(|frame| {
                    draw_frame_with_history(
                        frame,
                        FrameContent {
                            model: &model,
                            composer: &composer,
                            notice: None,
                            hints: ComposerHints::new(
                                Palette { color: false },
                                composer.newline_hint(KeyboardSupport::Disambiguated),
                            ),
                            history_rows: &[],
                            history_status: None,
                        },
                    );
                })
                .unwrap();
            let footer = buffer_row(terminal.backend().buffer(), 6);
            assert!(footer.contains(shortcut.label()), "{width}: {footer}");
            assert!(footer.contains("\\ Enter"), "{width}: {footer}");
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
            context_window_tokens: None,
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
            context_window_tokens: None,
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
    composer.set_runtime_skills(&["review".into(), "help".into()]);
    for (prefix, focused, category) in [
        ("/rev", "review", "(skill)"),
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
        (crate::terminal::CompletionLayout::Tabs, 64, false, false),
        (crate::terminal::CompletionLayout::Tabs, 64, true, true),
        (crate::terminal::CompletionLayout::Tabs, 0, true, false),
        (crate::terminal::CompletionLayout::Combined, 64, false, true),
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
                        if layout == crate::terminal::CompletionLayout::Combined {
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
                    if layout == crate::terminal::CompletionLayout::Tabs {
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
                        assert!(labels.iter().any(|row| row.starts_with(&safe_truncate(
                            "> (skill) /skill-63",
                            usize::from(width),
                        ))));
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
                        assert!(
                            labels
                                .iter()
                                .any(|row| row.contains("Report owned storage metadata"))
                        );
                        assert!(!labels.iter().any(|row| row.contains("/settings")));
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
                context_window_tokens: None,
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
    for width in [16, 28, 32, 34, 35, 40, 80] {
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
                for _ in 0..4 {
                    composer
                        .attach_image(arany::test_support::test_image())
                        .unwrap();
                }
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
                let rows = (0..8)
                    .map(|y| buffer_row(terminal.backend().buffer(), y))
                    .collect::<Vec<_>>();
                assert!(rows[3].contains(">[Image #4]"), "{width}: {rows:?}");
                assert_eq!(
                    rows[6].trim_matches('─'),
                    if width >= 35 {
                        " ←/→ select · Del/Backspace remove "
                    } else {
                        " Del remove "
                    },
                    "{width}: image selection footer"
                );
                assert!(rows[4].contains("alpha") && rows[5].contains('中'));
                assert!(!rows.iter().any(|row| row.contains("iVBOR")));
                assert_eq!(composer.apply(TerminalInput::Left), ComposerEdit::Changed);
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .unwrap();
                let rows = (0..8)
                    .map(|y| buffer_row(terminal.backend().buffer(), y))
                    .collect::<Vec<_>>();
                assert!(rows[3].contains(">[Image #3]"), "{width}: {rows:?}");
                assert_eq!(
                    composer.apply(TerminalInput::Character('!')),
                    ComposerEdit::Changed
                );
                terminal
                    .draw(|frame| draw_frame(frame, &model, &composer, None, Palette { color }))
                    .unwrap();
                let heading = buffer_row(terminal.backend().buffer(), 3);
                assert!(!heading.contains('>'), "{width}: {heading}");
                if width < 80 {
                    assert!(heading.contains("[4 images]"), "{width}: {heading}");
                } else {
                    for index in 1..=composer.images().len() {
                        assert!(
                            heading.contains(&format!("[Image #{index}]")),
                            "{width}: {heading}"
                        );
                    }
                }
                if !color {
                    assert!(
                        terminal
                            .backend()
                            .buffer()
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
                    FrameContent {
                        model: &model,
                        composer: &composer,
                        notice: None,
                        hints: ComposerHints::new(
                            Palette { color: false },
                            composer.newline_hint(KeyboardSupport::Legacy),
                        ),
                        history_rows: &[],
                        history_status: None,
                    },
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
fn paste_chips_and_detail_remain_inert_at_narrow_widths_without_color() {
    for width in [16, 20, 27, 28, 50, 80] {
        let mut composer = Composer::default();
        let payload = format!("literal @missing /exit\n{}", "content ".repeat(1152));
        composer.insert_paste("before ").unwrap();
        composer.insert_paste(&payload).unwrap();
        composer.apply(TerminalInput::Left);
        let caret = composer.cursor_byte_offset();
        let compact = wrapped_composer(&composer, usize::from(width));
        assert!(!compact.lines.join(" ").contains("@missing"));
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        let model = PresentationModel {
            status_line: "ready".into(),
            activity_lines: Vec::new(),
            setup_required: false,
            draft_action: DraftAction::Submit,
            empty_chat: true,
            agent_switch: false,
        };
        terminal
            .draw(|frame| {
                draw_composer(
                    frame,
                    &model,
                    &composer,
                    frame.area(),
                    ComposerHints::new(
                        Palette { color: false },
                        composer.newline_hint(KeyboardSupport::Legacy),
                    ),
                );
            })
            .unwrap();
        let hint = buffer_row(terminal.backend().buffer(), 7);
        assert!(
            hint.contains(if width >= 28 {
                "Tab: inspect · Del: remove"
            } else {
                "Tab / Del"
            }),
            "{width}: {hint}"
        );
        assert!(composer.inspect_input(TerminalInput::Tab));
        let (label, text, _) = composer.paste_detail().unwrap();
        terminal
            .draw(|frame| {
                draw_paste_detail(
                    frame,
                    frame.area(),
                    PasteDetailFrame {
                        label: &label,
                        text,
                        scroll: 0,
                        palette: Palette { color: false },
                    },
                )
            })
            .unwrap();
        let rendered = (0..8)
            .map(|y| buffer_row(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("literal @missing"));
        composer.inspect_input(TerminalInput::End);
        composer.clamp_detail_scroll(paste_detail_scroll_limit(&payload, usize::from(width), 8));
        let last = composer.paste_detail().unwrap().2;
        composer.inspect_input(TerminalInput::Up);
        assert_eq!(composer.paste_detail().unwrap().2, last - 1);
        composer.inspect_input(TerminalInput::Escape);
        assert_eq!(composer.cursor_byte_offset(), caret);
        assert_eq!(composer.text(), format!("before {payload}"));
    }
}
