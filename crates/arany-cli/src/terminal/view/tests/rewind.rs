use super::*;

#[test]
fn rewind_frame_corpus_keeps_every_panel_state_inert_and_paged_at_all_widths() {
    use crate::presentation::{
        escape_terminal,
        rewind::{KEEP, OVERWRITE},
    };
    use crate::terminal::rewind::{RewindFrame, draw_rewind_frame};
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
                            RewindFrame {
                                title,
                                lines: &lines,
                                choices: &choices,
                                selected: 0,
                                page,
                                palette: Palette { color: false },
                            },
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
