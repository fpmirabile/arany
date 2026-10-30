use super::*;

#[test]
fn usage_details_wrap_without_losing_scope_or_unknown_labels() {
    let page = "Session totals\nInput: 12,340 known; some calls unreported\nCache write (included in input): unknown (not reported)\nChatGPT weekly: unknown (not reported)\nAPI rate limits\nRole: compaction\nRequests\nLimit: 100\nRemaining: 0\nGroups 1-1 of 1\n";
    for width in [80, 50, 40, 16] {
        let lines = usage_lines(page, width);
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        let joined = lines.join(" ");
        for phrase in [
            "Session totals",
            "some calls unreported",
            "unknown (not reported)",
            "Role: compaction",
            "Remaining: 0",
        ] {
            assert!(joined.contains(phrase));
        }
        for color in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
            for scroll in [0, lines.len()] {
                terminal
                    .draw(|frame| {
                        draw_usage_frame(
                            frame,
                            frame.area(),
                            UsageFrame {
                                lines: &lines,
                                scroll,
                                page: (0, 2),
                                armed: true,
                                palette: Palette { color },
                            },
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let header = (0..width)
                    .map(|x| buffer[(x, 0)].symbol())
                    .collect::<String>();
                let footer = (0..width)
                    .map(|x| buffer[(x, 7)].symbol())
                    .collect::<String>();
                assert!(header.contains("Usage 1/2"));
                assert!(footer.contains("Esc"));
            }
        }
    }
}

#[tokio::test]
async fn usage_scroll_up_moves_immediately_after_reaching_the_last_window() {
    let mut attached = crate::terminal::clipboard::cleanup_tests::released_terminal(None);
    let lines: Vec<_> = (0..30).map(|index| format!("row-{index:02}")).collect();
    attached.panels.help_selected = Some(0);
    attached.panels.usage_panel = Some(crate::terminal::UsagePanel {
        pages: vec![lines.join("\n")],
        page: 0,
        sequence: 0,
        status: false,
        starting_sequence: None,
        rows: lines.len(),
        visible: 6,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
    for (input, expected) in [
        (TerminalInput::End, 24),
        (TerminalInput::Down, 24),
        (TerminalInput::PageDown, 24),
        (TerminalInput::Up, 23),
    ] {
        attached.help_input(input);
        let scroll = attached.panels.help_selected.unwrap();
        assert_eq!(scroll, expected);
        terminal
            .draw(|frame| {
                draw_usage_frame(
                    frame,
                    frame.area(),
                    UsageFrame {
                        lines: &lines,
                        scroll,
                        page: (0, 1),
                        armed: true,
                        palette: Palette { color: false },
                    },
                );
            })
            .unwrap();
        assert_eq!(
            buffer_row(terminal.backend().buffer(), 1).trim(),
            lines[expected]
        );
    }
}
