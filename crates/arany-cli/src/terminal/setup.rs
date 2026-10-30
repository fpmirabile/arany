use super::*;

impl AttachedTerminal {
    pub fn draw_workspace_permissions(
        &mut self,
        id: uuid::Uuid,
        workspace: &std::path::Path,
        profile_selection: ProfileSelection<'_>,
        page: usize,
        trust: bool,
        configured: bool,
    ) -> Result<(bool, usize), TerminalError> {
        let ProfileSelection {
            options,
            selected: selected_profiles,
            focused: focused_profile,
            enabled: profile_selection_enabled,
            discovery_error,
        } = profile_selection;
        self.cancel_clipboard_paste();
        let mut lines = crate::presentation::workspace_permission_lines(workspace);
        if configured {
            lines.push(
                "--tools is selected. Continue with its private configuration or remember folder trust; the explicit configuration keeps precedence.".into(),
            );
        }
        if let Some(error) = discovery_error {
            lines.push(format!(
                "Runtime profile discovery failed: {}; saved profiles stay listed.",
                crate::presentation::escape_terminal(error)
            ));
        }
        let profile_start;
        if options.is_empty() {
            lines.push("No supported runtime profiles were detected.".into());
            profile_start = None;
        } else {
            lines.extend(crate::presentation::RUNTIME_PROFILE_LINES.map(String::from));
            profile_start = Some(lines.len());
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let linear = self.linear.is_some();
            lines.extend(options.iter().enumerate().map(|(index, option)| {
                let marker = view::focus_marker(focused_profile == Some(index));
                let checked = selected_profiles
                    .get(index)
                    .copied()
                    .unwrap_or(option.selected);
                let number = index + 3;
                let body = if linear {
                    option.label.clone()
                } else {
                    let head = match &option.version {
                        Some(version) => format!("{} {version}", option.group),
                        None => option.group.to_owned(),
                    };
                    let path = crate::presentation::abbreviate_home(&option.path, home.as_deref());
                    let suffix = if option.detected {
                        ""
                    } else {
                        " · no longer detected"
                    };
                    let warning = if option.exclusive {
                        ""
                    } else {
                        " · runs whatever the directory currently holds"
                    };
                    format!("{head} · {path}{warning}{suffix}")
                };
                format!(
                    "{marker} [{}] {number}. {}",
                    if checked { "x" } else { " " },
                    crate::presentation::escape_terminal(&body)
                )
            }));
        }
        if let Some(linear) = &mut self.linear {
            linear
                .draw_permission_prompt(
                    id,
                    &lines,
                    if configured {
                        "1. Use explicit Tool config\n2. Trust this folder; 3+ select profiles. Type trust [profile numbers], trust none, or config; bare trust preserves selected profiles. Ctrl+C exits:"
                    } else {
                        "1. Continue read only\n2. Trust this folder; 3+ select profiles. Type trust [profile numbers], trust none, or read only; bare trust preserves selected profiles. Ctrl+C exits:"
                    },
                )
                .map_err(TerminalError::Io)?;
            return Ok((false, page));
        }
        let palette = self.palette();
        let mut progress = view::PermissionFrameProgress { more: true, page };
        let width = self.owned().size()?.width;
        let height = view::permission_height(&lines, width, false, profile_start, options.len());
        self.draw_frame(height, |frame, area| {
            if view::dialog_fits(area) {
                progress = view::draw_permission_frame(
                    frame,
                    area,
                    &lines,
                    view::PermissionFrameState {
                        choices: if configured && area.width < 24 {
                            &["Use config", "Trust folder"]
                        } else if configured {
                            &["Use explicit config", "Trust this folder"]
                        } else if area.width < 24 {
                            &["Read only", "Trust folder"]
                        } else {
                            &["Continue read only", "Trust this folder"]
                        },
                        page,
                        focus: usize::from(trust),
                        profile_start,
                        profile_count: options.len(),
                        focused_profile,
                        profile_selection_enabled: Some(profile_selection_enabled),
                        footer: None,
                        palette,
                        boxed: false,
                    },
                );
            } else {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("Resize to review; Ctrl+C exits"),
                    area,
                );
            }
        })?;
        Ok((progress.more, progress.page))
    }

    pub async fn discard_approval_typeahead(&mut self) -> Result<(), TerminalError> {
        self.reader().discard_typeahead().await
    }

    pub fn draw_tool_approval(
        &mut self,
        approval: &crate::presentation::ToolApprovalPrompt,
        session: &SessionView,
        page: usize,
        choice: Option<usize>,
        review_notice: bool,
    ) -> Result<bool, TerminalError> {
        self.clear_graphics_hover();
        self.cancel_clipboard_paste();
        // Approval owns the primary authority surface and arms through its own barrier.
        self.end_agent_chat()?;
        let lines = &approval.lines;
        let choices = &approval.choices;
        if let Some(linear) = &mut self.linear {
            linear
                .draw_tool_approval(approval.intent_id, lines, choices, choice.is_some())
                .map_err(TerminalError::Io)?;
            return Ok(false);
        }
        let screen = self.owned().size()?;
        let height = view::tool_approval_height(lines, choices, screen.width).min(screen.height);
        self.prepare_frame_height(height)?;
        let palette = self.palette();
        let terminal = self.owned.terminal();
        let history = &mut self.transcript.history;
        let mut more = true;
        let mut history_area = None;
        terminal.draw(|frame| {
            let area = dialog_area(frame.area(), height, self.mode == TerminalMode::Classic);
            if self.mode == TerminalMode::Fullscreen {
                history_area = Some(draw_history_above(frame, history, session, area.y, palette));
            }
            more = view::draw_tool_approval_frame(
                frame,
                area,
                lines,
                choices,
                view::ToolApprovalFrameState {
                    page,
                    focus: choice,
                    review_notice,
                    palette,
                },
            );
        })?;
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        self.panels.history_area = history_area;
        self.delete_graphics_placement()?;
        Ok(more)
    }

    pub fn draw_setup(
        &mut self,
        step: u8,
        title: &str,
        instruction: &str,
        input_chars: usize,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_setup(step, title, instruction, notice)
                .map_err(TerminalError::Io);
        }
        let palette = self.palette();
        let width = self.owned().size()?.width;
        self.draw_frame(if width < 20 { 8 } else { 7 }, |frame, area| {
            draw_setup_frame(
                frame,
                area,
                SetupFrame {
                    step,
                    title,
                    instruction,
                    input_chars,
                    notice,
                    palette,
                },
            )
        })?;
        Ok(())
    }

    pub fn draw_setup_text_input(
        &mut self,
        title: &str,
        instruction: &str,
        draft: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_setup(3, title, instruction, notice)
                .map_err(TerminalError::Io);
        }
        let palette = self.palette();
        let width = self.owned().size()?.width;
        self.draw_frame(if width < 20 { 8 } else { 7 }, |frame, area| {
            draw_setup_text_frame(
                frame,
                area,
                SetupTextFrame {
                    title,
                    instruction,
                    draft,
                    notice,
                    palette,
                },
            )
        })?;
        Ok(())
    }

    pub fn draw_setup_choices(&mut self, setup: SetupChoices<'_>) -> Result<(), TerminalError> {
        let SetupChoices {
            step,
            title,
            instruction,
            choices,
            selected,
            notice,
        } = setup;
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            let paged = setup_choices_need_paging(instruction, choices);
            let names = choices
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(" or ");
            let mut directions = if paged {
                let numbered = format!(
                    "type a listed choice number or exact name (1-{}); empty Enter selects {}",
                    choices.len(),
                    selected + 1
                );
                if instruction.is_empty() {
                    numbered
                } else {
                    format!("{instruction}; {numbered}")
                }
            } else if instruction.is_empty() {
                format!("Type a choice name: {names}")
            } else {
                format!("{instruction}; type a choice name: {names}")
            };
            if !paged && let Some((_, name)) = choices.get(selected) {
                directions.push_str("; empty Enter selects ");
                directions.push_str(name);
            }
            return linear
                .draw_setup_choices(step, title, &directions, notice)
                .map_err(TerminalError::Io);
        }
        let palette = self.palette();
        self.draw_frame(setup_choices_height(choices.len()), |frame, area| {
            draw_setup_choices_frame(
                frame,
                area,
                SetupChoicesFrame {
                    title,
                    instruction,
                    choices,
                    selected,
                    notice,
                    palette,
                },
            )
        })?;
        Ok(())
    }

    pub fn draw_setup_warning(
        &mut self,
        title: &str,
        warning: &str,
        page: usize,
        accept_selected: bool,
        notice: Option<&str>,
    ) -> Result<bool, TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            linear
                .draw_setup_warning(title, warning, notice)
                .map_err(TerminalError::Io)?;
            return Ok(false);
        }
        let palette = self.palette();
        let mut has_more = false;
        let mut visible = false;
        let width = self.owned().size()?.width;
        let height = view::setup_warning_height(warning, width);
        self.draw_frame(height, |frame, area| {
            visible = view::dialog_fits(area);
            has_more = draw_setup_warning_frame(
                frame,
                area,
                SetupWarningFrame {
                    title,
                    warning,
                    page,
                    accept_selected,
                    notice,
                    palette,
                },
            );
        })?;
        if !visible {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Confirmation needs a visible terminal with at least {} columns and {} rows; nothing accepted",
                    view::MIN_DIALOG_WIDTH,
                    view::MIN_DIALOG_HEIGHT,
                ),
            )));
        }
        Ok(has_more)
    }

    pub fn begin_hidden_input(&mut self) -> Result<(), TerminalError> {
        if self.linear.is_some() {
            {
                if self.secret_echo.is_none() {
                    self.secret_echo = Some(SecretEcho::acquire()?);
                }
            }
        }
        Ok(())
    }

    pub fn end_hidden_input(&mut self) -> Result<(), TerminalError> {
        if let Some(mut echo) = self.secret_echo.take() {
            writeln!(io::stderr())?;
            echo.release()?;
        }
        Ok(())
    }
}

const MAX_INLINE_SETUP_CHOICE_CHARS: usize = 180;

pub fn setup_choices_need_paging(instruction: &str, choices: &[(u8, &str)]) -> bool {
    let names = choices
        .iter()
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(" or ");
    let longest = choices
        .iter()
        .map(|(_, name)| *name)
        .max_by_key(|name| name.chars().count())
        .unwrap_or("");
    names.chars().count() > MAX_INLINE_SETUP_CHOICE_CHARS
        || format!("{instruction}; type a choice name: {names}; empty Enter selects {longest}")
            .chars()
            .count()
            > crate::presentation::MAX_ROW_WIDTH
}
