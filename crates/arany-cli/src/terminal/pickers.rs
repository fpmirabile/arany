use super::*;

impl AttachedTerminal {
    pub fn draw_sessions(
        &mut self,
        items: &[SessionListItem],
        selected: usize,
        session: Option<&SessionView>,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_sessions(items, selected)
                .map_err(TerminalError::Io);
        }
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        let result = self.draw_dialog(
            session,
            u16::try_from(items.len().saturating_add(6))
                .unwrap_or(u16::MAX)
                .min(12),
            |frame, area| {
                draw_session_picker_frame(
                    frame,
                    area,
                    SessionPickerFrame {
                        items,
                        selected,
                        armed,
                        palette,
                    },
                )
            },
        );
        match result {
            Ok(area) => {
                self.panels.session_picker_area = Some(area);
                Ok(())
            }
            Err(error) => {
                let _ = self.close_sessions();
                Err(error)
            }
        }
    }

    pub fn draw_settings(
        &mut self,
        title: &str,
        rows: &[String],
        selected: usize,
        notice: Option<&str>,
        session: &SessionView,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_settings(title, rows, notice)
                .map_err(TerminalError::Io);
        }
        self.panels.panel_selection.enabled = true;
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        let width = self.owned().size()?.width;
        let max_rows = if self.mode == TerminalMode::Classic {
            view::SELECTOR_ROWS
        } else {
            rows.len()
        };
        self.draw_dialog(
            Some(session),
            view::settings_height(rows, width, max_rows)
                .saturating_add(notice.map_or(0, |text| view::guidance_height(text, width))),
            |frame, area| {
                draw_settings_frame(
                    frame,
                    area,
                    SettingsFrame {
                        title,
                        rows,
                        selected,
                        armed,
                        notice,
                        palette,
                    },
                );
            },
        )?;
        Ok(())
    }

    pub fn draw_quick_actions(
        &mut self,
        selected: usize,
        has_draft: bool,
        session: &SessionView,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        self.panels.panel_selection.enabled = true;
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        self.draw_dialog(Some(session), 7, |frame, area| {
            draw_quick_actions_frame(
                frame,
                area,
                QuickActionsFrame {
                    selected,
                    armed,
                    has_draft,
                    palette,
                },
            );
        })?;
        Ok(())
    }

    pub async fn open_usage(
        &mut self,
        session: &SessionView,
        status: bool,
        starting_sequence: Option<u64>,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        let pages = crate::presentation::usage::usage_pages_at(
            session,
            status,
            starting_sequence,
            usage_now(),
        );
        if let Some(linear) = &mut self.linear {
            return linear
                .write_usage(&mut io::stderr(), &pages)
                .map_err(TerminalError::Io);
        }
        self.panels.help_selected = Some(0);
        self.panels.usage_panel = Some(UsagePanel {
            pages,
            page: 0,
            sequence: session.last_sequence,
            status,
            starting_sequence,
            rows: 0,
            visible: 0,
        });
        self.open_dialog(|terminal| terminal.draw_help(0, session), Self::close_help)
            .await
    }

    pub async fn open_help(&mut self, session: &SessionView) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .write_help(&mut io::stderr())
                .map_err(TerminalError::Io);
        }
        self.panels.help_selected = Some(0);
        self.open_dialog(|terminal| terminal.draw_help(0, session), Self::close_help)
            .await
    }

    #[must_use]
    pub fn help_open(&self) -> bool {
        self.panels.help_selected.is_some()
    }

    pub fn close_help(&mut self) {
        self.panels.help_selected = None;
        self.panels.usage_panel = None;
        // Discard queued dialog keys before the composer owns input again.
        self.discard_draft_input();
    }

    pub fn help_input(&mut self, input: TerminalInput) {
        let Some(selected) = self.panels.help_selected.as_mut() else {
            return;
        };
        if let Some(panel) = &mut self.panels.usage_panel {
            let max_scroll = panel.rows.saturating_sub(panel.visible.max(1));
            match input {
                TerminalInput::Left => {
                    panel.page = panel.page.saturating_sub(1);
                    *selected = 0;
                }
                TerminalInput::Right => {
                    panel.page = (panel.page + 1).min(panel.pages.len() - 1);
                    *selected = 0;
                }
                TerminalInput::PointerScrollUp { .. } => *selected = selected.saturating_sub(1),
                TerminalInput::PointerScrollDown { .. } => {
                    *selected = (*selected + 1).min(max_scroll)
                }
                TerminalInput::Escape
                | TerminalInput::Submit
                | TerminalInput::Interrupt
                | TerminalInput::EndOfInput => self.close_help(),
                input => {
                    if let Some(next) = input.step_selection(*selected, max_scroll) {
                        *selected = next;
                    }
                }
            }
            return;
        }
        match input {
            TerminalInput::Escape
            | TerminalInput::Submit
            | TerminalInput::Interrupt
            | TerminalInput::EndOfInput => self.close_help(),
            input => {
                if let Some(next) = input.step_selection(*selected, help_len() - 1) {
                    *selected = next;
                }
            }
        }
    }

    pub(super) fn draw_help(
        &mut self,
        selected: usize,
        session: &SessionView,
    ) -> Result<(), TerminalError> {
        self.panels.panel_selection.enabled = true;
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        if let Some(panel) = &mut self.panels.usage_panel {
            if panel.sequence != session.last_sequence {
                panel.pages = crate::presentation::usage::usage_pages_at(
                    session,
                    panel.status,
                    panel.starting_sequence,
                    usage_now(),
                );
                panel.sequence = session.last_sequence;
                panel.page = panel.page.min(panel.pages.len() - 1);
            }
            let width = self
                .owned
                .terminal_with_context("visual terminal")
                .size()?
                .width;
            let lines = view::usage_lines(&panel.pages[panel.page], width);
            panel.rows = lines.len();
            let page = panel.page;
            let pages = panel.pages.len();
            let height = if self.mode == TerminalMode::Classic {
                u16::try_from(lines.len().saturating_add(2)).unwrap_or(u16::MAX)
            } else {
                20
            };
            let area = self.draw_dialog(Some(session), height, |frame, area| {
                view::draw_usage_frame(
                    frame,
                    area,
                    UsageFrame {
                        lines: &lines,
                        scroll: selected,
                        page: (page, pages),
                        armed,
                        palette,
                    },
                );
            })?;
            let panel = self.panels.usage_panel.as_mut().expect("open usage panel");
            panel.visible = usize::from(area.height.saturating_sub(2));
            self.panels.help_selected =
                Some(selected.min(panel.rows.saturating_sub(panel.visible.max(1))));
            return Ok(());
        }
        // Classic shows the header, the capped command rows, the focused description and the
        // key hint; Fullscreen keeps its 20-row panel below live history.
        let height = if self.mode == TerminalMode::Classic {
            u16::try_from(view::SELECTOR_ROWS + 3).unwrap_or(u16::MAX)
        } else {
            20
        };
        self.draw_dialog(Some(session), height, |frame, area| {
            draw_help_frame(frame, area, selected, armed, palette);
        })?;
        Ok(())
    }

    #[must_use]
    pub fn session_picker_target(
        &self,
        column: u16,
        row: u16,
        selected: usize,
        count: usize,
    ) -> Option<usize> {
        self.panels
            .session_picker_area
            .and_then(|area| session_picker_index(area, column, row, selected, count))
    }

    pub fn close_sessions(&mut self) -> Result<(), TerminalError> {
        self.panels.session_picker_area = None;
        if let Some(linear) = &mut self.linear {
            linear.last_picker_page = None;
            linear.prompt_needed = true;
        }
        Ok(())
    }

    pub fn redraw_sessions(&mut self) {
        if let Some(linear) = &mut self.linear {
            linear.last_picker_page = None;
        }
    }

    pub fn draw_models(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        picker: &ModelPicker<'_>,
    ) -> Result<bool, TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_models(
                    view.defaults.provider.as_deref().unwrap_or("not selected"),
                    picker,
                )
                .map(|()| true)
                .map_err(TerminalError::Io);
        }
        let area = self.owned.terminal_with_context("inline terminal").size()?;
        self.draw_snapshot(view, composer, None, SnapshotState::Session, Some(picker))?;
        self.owned
            .terminal_with_context("inline terminal")
            .hide_cursor()?;
        Ok(view::dialog_fits(Rect::from((
            ratatui::layout::Position::ORIGIN,
            area,
        ))))
    }

    pub fn close_models(&mut self) {
        if let Some(linear) = &mut self.linear {
            linear.last_model_page = None;
            linear.prompt_needed = true;
        }
    }

    pub fn session_picker_invalid(&mut self) -> Result<(), TerminalError> {
        if let Some(linear) = &mut self.linear {
            linear.write_picker_invalid(&mut io::stderr())?;
        }
        Ok(())
    }

    pub async fn open_agents(
        &mut self,
        view: &SessionView,
        locked: bool,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        self.agent_panel = Some(AgentPanel::new(locked));
        self.admit_agents(view).await
    }

    async fn admit_agents(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        self.open_dialog(
            |terminal| terminal.draw_agents(view),
            |terminal| {
                let _ = terminal.close_agents();
            },
        )
        .await
    }

    #[must_use]
    pub fn agents_open(&self) -> bool {
        self.agent_panel.is_some()
    }

    pub fn close_agents(&mut self) -> Result<(), TerminalError> {
        if self.agent_panel.is_none() {
            return Ok(());
        }
        self.panels.agent_picker_area = None;
        if self.agent_panel.take().is_some() {
            self.discard_draft_input();
            if let Some(linear) = &mut self.linear {
                linear.prompt_needed = true;
            }
        }
        Ok(())
    }

    pub fn close_agents_at_run_end(&mut self) -> Result<(), TerminalError> {
        if self.agent_panel.is_some() && self.linear.is_some() {
            tcflush(io::stdin(), QueueSelector::IFlush).map_err(io::Error::from)?;
        }
        self.close_agents()
    }

    // Enter opens the focused agent's chat; the caller then draws whichever view owns input.
    pub async fn agent_input(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
    ) -> Result<Option<&'static str>, TerminalError> {
        if matches!(
            input,
            TerminalInput::LineRejected | TerminalInput::LineContinued
        ) {
            self.discard_draft_input();
        }
        let model = self.agent_model(view)?;
        let target = match input {
            TerminalInput::PointerMove { column, row }
            | TerminalInput::PointerClick { column, row, .. }
            | TerminalInput::PointerScrollUp { column, row }
            | TerminalInput::PointerScrollDown { column, row } => {
                self.panels.agent_picker_area.and_then(|area| {
                    agent_picker_index(area, column, row, model.selected, model.rows.len())
                })
            }
            _ => None,
        };
        let panel = self.agent_panel.as_mut().expect("open agent panel");
        let update = match input {
            TerminalInput::PointerMove { .. } | TerminalInput::PointerClick { .. } => {
                target.map_or(PanelUpdate::Unchanged, |index| panel.focus(&model, index))
            }
            TerminalInput::PointerScrollUp { .. } if target.is_some() => {
                panel.handle(TerminalInput::Up, &model, false)
            }
            TerminalInput::PointerScrollDown { .. } if target.is_some() => {
                panel.handle(TerminalInput::Down, &model, false)
            }
            TerminalInput::PointerScrollUp { .. } | TerminalInput::PointerScrollDown { .. } => {
                PanelUpdate::Unchanged
            }
            _ => panel.handle(input, &model, self.linear.is_some()),
        };
        match update {
            PanelUpdate::Unchanged => Ok(None),
            PanelUpdate::Redraw => self.draw_agents(view).map(|()| None),
            PanelUpdate::Rearm => self.admit_agents(view).await.map(|()| None),
            PanelUpdate::Closed => self.close_agents().map(|()| None),
            PanelUpdate::Open(run, agent) => {
                self.close_agents()?;
                self.open_agent(view, run, agent).await
            }
        }
    }

    fn agent_model(&mut self, view: &SessionView) -> Result<AgentInspectorModel, TerminalError> {
        let width = match &mut self.owned.terminal {
            Some(terminal) if self.linear.is_none() => terminal.size()?.width,
            _ => 120,
        };
        let panel = self.agent_panel.as_ref().expect("open agent panel");
        Ok(AgentInspectorModel::from_session(
            view,
            &panel.query,
            width,
            panel.locked,
        ))
    }

    pub(super) fn draw_agents(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        let result = self.draw_agents_inner(view);
        if result.is_err() {
            let _ = self.close_agents();
        }
        result
    }

    fn draw_agents_inner(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        let model = self.agent_model(view)?;
        let panel = self.agent_panel.as_mut().expect("open agent panel");
        panel.query.focus = model.shown_focus(panel.query.focus);
        if let Some(linear) = &mut self.linear {
            // Committed changes never renumber printed choices; only a user action reprints.
            if panel.printed.is_none() {
                let start = view::window_start(model.selected, MAX_AGENT_ROWS);
                linear.write_agents(&mut io::stderr(), &model, start, panel.notice.take())?;
                let rows = model.rows.iter().skip(start).take(MAX_AGENT_ROWS);
                panel.printed = Some((start, rows.copied().collect()));
            }
            return Ok(());
        }
        let width = self
            .owned
            .terminal_with_context("inline terminal")
            .size()?
            .width;
        let (_, screen_height) = size()?;
        let detail_lines = agent_detail_lines(&model, width).len();
        let height =
            u16::try_from(detail_lines.saturating_add(3 + model.choices.len().min(MAX_AGENT_ROWS)))
                .unwrap_or(u16::MAX)
                .min(AGENT_PICKER_ROWS)
                .min(screen_height);
        panel.scroll = panel
            .scroll
            .min(detail_lines.saturating_sub(agent_detail_rows(height, model.choices.len())));
        let scroll = panel.scroll;
        let notice = panel.notice;
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        let area = self.draw_dialog(Some(view), height, |frame, area| {
            draw_agent_inspector_frame(
                frame,
                area,
                AgentInspectorFrame {
                    model: &model,
                    scroll,
                    armed,
                    notice,
                    palette,
                },
            );
        })?;
        self.panels.agent_picker_area = Some(area);
        Ok(())
    }
}
