use super::*;

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
fn dense_setup_choices_keep_error_feedback_and_selection_visible() {
    let choices = [
        (1, "API accounts"),
        (2, "Custom routes"),
        (3, "Role routing"),
        (4, "Roster profiles"),
    ];
    for (width, height) in [(16, 3), (40, 3), (16, 4), (40, 4), (16, 8), (40, 8)] {
        for (selected, expected) in [(0, "> API accounts"), (3, "> Roster")] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw_setup_choices_frame(
                        frame,
                        frame.area(),
                        "Provider setup",
                        "Manage access, custom routes, role routing and roster presets",
                        &choices,
                        selected,
                        Some("Use Up/Down and Enter to select"),
                        Palette { color: false },
                    );
                })
                .unwrap();
            let visible = (0..height)
                .map(|row| buffer_row(terminal.backend().buffer(), row))
                .collect::<Vec<_>>()
                .join(" ");
            assert!(visible.contains(expected), "{width}x{height}: {visible}");
            assert_eq!(
                visible.contains("Use Up/Down"),
                height > 3,
                "{width}x{height}: {visible}"
            );
            assert!(visible.contains("Enter") && visible.contains("Esc"));
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
fn setup_warning_wraps_unicode_without_splitting_utf8() {
    let warning = format!("{}éclair", "a".repeat(15));
    let lines = wrap_setup_warning(&warning, 16);
    assert_eq!(lines.concat(), warning);
    assert!(
        lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 16)
    );
}
