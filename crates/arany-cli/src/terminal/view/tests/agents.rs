use super::*;

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
