use super::*;

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
    for (width, height, expect_more) in [(100, 36, false), (60, 20, true)] {
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
            assert!(buffer_row(buffer, height - 4).contains("› Read only"));
            assert!(buffer_row(buffer, height - 1).contains("Ctrl+C exit"));
        }
    }
}

#[test]
fn permission_height_indents_only_the_profile_rows_the_draw_indents() {
    let lines = vec![
        "Do you trust this folder?".to_owned(),
        "› [ ] 3. Node v20.19.2".to_owned(),
        "x".repeat(152),
    ];

    let height = permission_height(&lines, 80, false, Some(1), 1);

    // One heading row, one profile row, the trailing note's two unindented rows and five control rows.
    assert_eq!(height, 1 + 1 + 2 + 5);
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
    let profile_count = long.len() - profile_start;
    for (lines, profile_start, pages) in
        [(&fitting, None, false), (&long, Some(profile_start), true)]
    {
        // As in Codex, the Classic viewport is min(screen, the dialog's content height).
        let content = permission_height(lines, 80, false, profile_start, profile_count);
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
fn sensitive_host_warning_leads_and_wraps_with_the_color_policy() {
    use crate::presentation::{
        HOST_SENSITIVE_PATH_WARNING, ToolApprovalChoice, tool_fixture, unguarded_approval_lines,
    };
    let tool = tool_fixture(
        arany::ToolCall::CommandUnguarded {
            program: "cat".into(),
            args: vec!["/private/credentials".into()],
            cwd: String::new(),
            justification: "Inspect the named credentials".into(),
            prefix_rule: None,
        },
        "",
    );
    let lines = unguarded_approval_lines(
        &tool.intent,
        &arany::UnguardedPrompt {
            executable: "/usr/bin/cat".into(),
            attribution: None,
            remember: false,
            prefix: false,
            sensitive: true,
            inactive_rules: false,
        },
    );
    for width in [120, 50, 16] {
        for color in [true, false] {
            let mut terminal = Terminal::new(TestBackend::new(width, 50)).unwrap();
            terminal
                .draw(|frame| {
                    draw_tool_approval_frame(
                        frame,
                        frame.area(),
                        &lines,
                        &[ToolApprovalChoice::Yes, ToolApprovalChoice::No],
                        ToolApprovalFrameState {
                            page: 0,
                            focus: Some(1),
                            review_notice: false,
                            palette: Palette { color },
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let gutter = u16::from(width >= 40) * 2;
            let wrapped = wrap_cells(
                HOST_SENSITIVE_PATH_WARNING,
                usize::from(width - gutter * 2),
                0,
            );
            for (index, expected) in wrapped.iter().enumerate() {
                let row = index as u16 + 1;
                assert_eq!(buffer_row(buffer, row).trim(), expected);
                for column in gutter..gutter + expected.len() as u16 {
                    assert_eq!(
                        buffer[(column, row)].fg,
                        if color { Color::Yellow } else { Color::Reset },
                        "warning style at width {width}, color {color}"
                    );
                }
            }
            assert!(buffer_row(buffer, wrapped.len() as u16 + 1).contains("Run outside"));
        }
    }
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
                "auto-edits-host",
                vec![crate::presentation::HOST_SESSION_APPROVAL_HINT.into()],
                vec![Yes, No],
                1,
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
            let height =
                crate::terminal::view::tool_approval_height(&lines, &choices, width).min(50);
            let mut page = 0;
            let mut seen = String::new();
            loop {
                let mut more = true;
                terminal
                    .draw(|frame| {
                        let area = Rect::new(0, 50 - height, width, height);
                        more = crate::terminal::view::draw_tool_approval_frame(
                            frame,
                            area,
                            &lines,
                            &choices,
                            ToolApprovalFrameState {
                                page,
                                focus: Some(focus),
                                review_notice: false,
                                palette: Palette { color: false },
                            },
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
            if name == "auto-edits-host" {
                let buffer = terminal.backend().buffer();
                let dimmed = (0..50)
                    .flat_map(|y| (0..width).map(move |x| &buffer[(x, y)]))
                    .filter(|cell| cell.modifier.contains(Modifier::DIM))
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert_eq!(
                    dimmed
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .collect::<String>(),
                    crate::presentation::HOST_SESSION_APPROVAL_HINT
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .collect::<String>(),
                    "hint wraps in dim text at width {width}"
                );
            }
            if name == "multi-page" {
                for line in body {
                    assert!(seen.contains(&line), "missing {line} at {width}");
                }
                assert!(page > 0);
            }
            if name == "unguarded" {
                assert!(
                    squashed.contains("2. Yes, for this session"),
                    "{width}: {squashed}"
                );
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
                    crate::terminal::view::draw_tool_approval_frame(
                        frame,
                        area,
                        &lines,
                        &choices,
                        ToolApprovalFrameState {
                            page: 0,
                            focus: None,
                            review_notice: false,
                            palette: Palette { color: false },
                        },
                    );
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
