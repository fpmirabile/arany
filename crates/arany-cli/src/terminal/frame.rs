use super::*;

impl AttachedTerminal {
    pub fn draw(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.draw_notice(view, composer, notice.map(TerminalNotice::History))
    }

    pub fn draw_notice(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
    ) -> Result<(), TerminalError> {
        self.draw_snapshot(view, composer, notice, SnapshotState::Session, None)
    }

    pub(super) fn prepare_frame(&mut self) -> Result<(), TerminalError> {
        let height = self.owned().get_frame().area().height;
        self.prepare_frame_height(height)
    }

    pub(super) fn prepare_frame_height(&mut self, height: u16) -> Result<(), TerminalError> {
        // A dialog such as a Tool approval needs the screen, so it ends a scrollback dump.
        self.end_scrollback_dump()?;
        let terminal = self.owned.terminal();
        if self.mode == TerminalMode::Classic {
            let offset = terminal.backend().cursor_offset();
            let before = terminal.get_frame().area();
            let keep_top = classic_keeps_top(before, self.transcript.history_adjacent);
            let resized = resize_classic(terminal, offset, height, keep_top, || {
                let mut backend = TerminalBackend::new(TerminalMode::Classic, None);
                backend.detach();
                backend
            });
            if resized.is_err() {
                let _ = terminal.show_cursor();
                terminal.backend_mut().detach();
            }
            resized?;
            // A bottom-anchored shrink leaves cleared rows above the composer.
            self.transcript.history_adjacent &= terminal.get_frame().area().y <= before.y;
        } else {
            terminal.autoresize()?;
        }
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        Ok(())
    }

    pub(super) fn draw_frame(
        &mut self,
        height: u16,
        render: impl FnOnce(&mut ratatui::Frame<'_>, Rect),
    ) -> Result<(), TerminalError> {
        self.panels.panel_selection.area = None;
        self.panels.panel_selection.enabled = false;
        self.prepare_frame_height(height)?;
        let terminal = self.owned();
        terminal.draw(|frame| render(frame, frame.area()))?;
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        if self.hover.image.is_none() {
            self.delete_graphics_placement()?;
        }
        Ok(())
    }

    // A selector opens unarmed; `arm_dialog` admits input once the reader barrier completes.
    pub fn begin_dialog(&mut self) {
        self.clear_graphics_hover();
        self.panels.dialog_armed = false;
        self.panels.panel_selection.enabled = false;
        self.panels.panel_selection.area = None;
        self.panels.panel_selection.clear();
        self.panels.panel_selection.feedback = None;
        if let Some(linear) = &mut self.linear {
            linear.disarm();
        }
    }

    pub(super) fn clear_graphics_hover(&mut self) {
        self.hover.sixel.clear();
        transition_hover(&mut self.hover.image, None);
        self.hover.pointer = None;
        self.hover.digest = None;
        self.hover.chip_areas.clear();
        self.hover.changed = false;
    }

    pub(super) fn delete_graphics_placement(&mut self) -> io::Result<()> {
        if self.hover.placement.take().is_none() {
            return Ok(());
        }
        if self.owned.graphics_protocol != Some(GraphicsProtocol::Kitty) {
            return Ok(());
        }
        self.owned.terminal.as_mut().map_or(Ok(()), |terminal| {
            terminal
                .backend_mut()
                .write_graphics(graphics::kitty_delete())
        })
    }

    // Returns true when this call armed the selector, so the caller redraws before reading input.
    pub async fn arm_dialog(&mut self) -> Result<bool, TerminalError> {
        if self.panels.dialog_armed {
            return Ok(false);
        }
        self.reader().discard_typeahead().await?;
        self.panels.dialog_armed = true;
        if let Some(linear) = &mut self.linear {
            linear.arm(&mut io::stderr())?;
        }
        Ok(true)
    }

    // A failed admission barrier closes the dialog, so no unarmed dialog keeps input.
    pub(super) async fn open_dialog(
        &mut self,
        mut draw: impl FnMut(&mut Self) -> Result<(), TerminalError>,
        close: impl FnOnce(&mut Self),
    ) -> Result<(), TerminalError> {
        self.begin_dialog();
        draw(self)?;
        if let Err(error) = self.arm_dialog().await {
            close(self);
            return Err(error);
        }
        draw(self)
    }

    pub(super) fn draw_dialog(
        &mut self,
        session: Option<&SessionView>,
        height: u16,
        render: impl FnOnce(&mut ratatui::Frame<'_>, Rect),
    ) -> Result<Rect, TerminalError> {
        self.clear_graphics_hover();
        self.prepare_frame_height(height)?;
        let palette = self.palette();
        let session = session.filter(|_| self.mode == TerminalMode::Fullscreen);
        let history = &mut self.transcript.history;
        let feedback = if self.panels.panel_selection.enabled {
            self.panels.panel_selection.feedback.clone()
        } else {
            None
        };
        let selection = &mut self.panels.panel_selection;
        let terminal = self.owned.terminal();
        let mut area = Rect::default();
        let classic = self.mode == TerminalMode::Classic;
        let mut history_area = None;
        let mut panel_lines = Vec::new();
        terminal.draw(|frame| {
            area = dialog_area(frame.area(), height, classic);
            if let Some(session) = session {
                history_area = Some(draw_history_above(frame, history, session, area.y, palette));
            }
            render(frame, area);
            panel_lines = rendered_panel_lines(frame.buffer_mut(), area);
            selection.render(frame, area);
            if let Some(feedback) = &feedback {
                draw_panel_feedback(frame, area, feedback, palette);
            }
        })?;
        let full = terminal.get_frame().area();
        terminal.backend_mut().set_area(full);
        if feedback.is_some() {
            self.panels.panel_selection.feedback = None;
        }
        self.panels.history_area = history_area;
        self.panels.panel_selection.lines = panel_lines;
        self.delete_graphics_placement()?;
        Ok(area)
    }

    // Overlays own the screen: the scrollback dump, help, agents, agent chat and linear output.
    fn draw_snapshot_overlay(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
    ) -> Option<Result<(), TerminalError>> {
        // The primary screen shows the dump until Enter; the Run keeps going without redraws.
        if self.transcript.scrollback_dump {
            if let Some(notice) = notice {
                self.transcript.history.record_notice(notice);
            }
            return Some(Ok(()));
        }
        if let Some(selected) = self.panels.help_selected {
            return Some(self.draw_help(selected, view));
        }
        if self.agent_panel.is_some() {
            return Some(self.draw_agents(view));
        }
        if self.agent_chat_shown(view) {
            return Some(self.draw_agent_chat(view, notice));
        }
        let linear = self.linear.as_mut()?;
        if std::mem::take(&mut self.clipboard_preview)
            && let Err(error) = linear.write_clipboard_draft(&mut io::stderr(), composer)
        {
            return Some(Err(TerminalError::Io(error)));
        }
        Some(
            linear
                .draw(
                    view,
                    notice.map(TerminalNotice::text),
                    composer.approval_mode(),
                )
                .map_err(TerminalError::Io),
        )
    }

    // A resized screen or a changed image under the pointer invalidates the hover.
    fn drop_stale_hover(&mut self, screen: Rect, composer: &Composer) {
        if self
            .panels
            .frame_area
            .is_some_and(|previous| previous != screen)
        {
            self.clear_graphics_hover();
        }
        self.panels.frame_area = Some(screen);
        if let Some(index) = self.hover.image
            && self.hover.digest.is_some_and(|digest| {
                composer.images().get(index).map(ImageAttachment::digest) != Some(digest)
            })
        {
            self.clear_graphics_hover();
        }
    }

    fn snapshot_height(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        model: &PresentationModel,
        screen: Rect,
        picker: Option<&ModelPicker<'_>>,
    ) -> u16 {
        if let Some(picker) = picker {
            composer_height(composer, screen)
                .saturating_add(1)
                .saturating_add(
                    u16::try_from(picker.items.len().max(1).saturating_add(3))
                        .unwrap_or(u16::MAX)
                        .min(12),
                )
                .max(view::MIN_DIALOG_HEIGHT)
                .min(screen.height)
        } else if self.mode == TerminalMode::Classic {
            self.transcript.classic_history.sync(
                view,
                usize::from(screen.width),
                usize::from(screen.height),
            );
            view::classic_height(
                model,
                composer,
                screen,
                self.transcript.classic_history.live_rows(view).len(),
            )
        } else {
            screen.height
        }
    }

    // A Classic History notice goes to native scrollback once per Session sequence.
    fn record_classic_notice(
        &mut self,
        view: &SessionView,
        text: &str,
    ) -> Result<(), TerminalError> {
        let text = history::bounded_notice_text(text).into_owned();
        if self
            .transcript
            .native
            .last_notice
            .as_ref()
            .is_none_or(|(sequence, prior)| *sequence != view.last_sequence || prior != &text)
        {
            let (kind, body) = history::notice_kind(&text);
            self.insert_native(kind, body, None)?;
            self.transcript.native.last_notice = Some((view.last_sequence, text));
        }
        Ok(())
    }

    fn history_status(
        &mut self,
        view: &SessionView,
        model: &mut PresentationModel,
        width: u16,
    ) -> Option<String> {
        let native_transcript = self.mode == TerminalMode::Classic && !view.runs.is_empty();
        if !native_transcript {
            return self.history_mut().status();
        }
        let hint = classic_history_status(
            width,
            self.transcript.native.omitted_history,
            self.classic_pinned,
        );
        if self.transcript.native.omitted_history {
            Some(hint.to_owned())
        } else {
            // State and model lead; the mode hint is secondary and truncation drops it first.
            model.status_line = crate::presentation::safe_truncate(
                &format!("{} · {hint}", model.status_line),
                usize::from(width),
            );
            None
        }
    }

    pub(super) fn draw_snapshot(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
        state: SnapshotState,
        picker: Option<&ModelPicker<'_>>,
    ) -> Result<(), TerminalError> {
        if picker.is_some()
            || self.panels.help_selected.is_some()
            || self.agent_panel.is_some()
            || self.agent_chat_shown(view)
            || self.transcript.transcript_open
            || composer.paste_detail().is_some()
        {
            self.clear_graphics_hover();
        }
        let notice = notice.or_else(|| {
            crate::presentation::tool_capacity_notice(view, self.waiting_tool)
                .map(TerminalNotice::Transient)
        });
        if let Some(drawn) = self.draw_snapshot_overlay(view, composer, notice) {
            return drawn;
        }
        let notice = notice.or_else(|| {
            self.clipboard
                .as_ref()
                .map(|_| TerminalNotice::Transient("Reading clipboard... draft retained"))
        });
        let cue = self.panels.selection_cue.clone();
        let notice = notice.or_else(|| cue.as_deref().map(TerminalNotice::Transient));
        let screen = Rect::from((ratatui::layout::Position::ORIGIN, self.owned().size()?));
        self.drop_stale_hover(screen, composer);
        if let Some((label, text, scroll)) = composer.paste_detail() {
            let palette = self.palette();
            return self.draw_frame(screen.height, |frame, area| {
                view::draw_paste_detail(
                    frame,
                    area,
                    PasteDetailFrame {
                        label: &label,
                        text,
                        scroll,
                        palette,
                    },
                );
            });
        }
        if self.mode == TerminalMode::Fullscreen && self.transcript.transcript_open {
            return self.draw_pager(view, notice, screen);
        }
        let snapshot = ChatSnapshot {
            view,
            composer,
            notice,
            state,
            picker,
            screen,
        };
        let input = self.chat_frame_input(&snapshot)?;
        self.render_chat_frame(&snapshot, input)
    }

    fn chat_frame_input<'a>(
        &mut self,
        snapshot: &ChatSnapshot<'a>,
    ) -> Result<ChatFrameInput<'a>, TerminalError> {
        let ChatSnapshot {
            view,
            composer,
            notice,
            state,
            picker,
            screen,
        } = *snapshot;
        let mut model = match state {
            SnapshotState::Preparing => PresentationModel::preparing(view, screen.width),
            _ => PresentationModel::from_session(view, screen.width),
        };
        let height = self.snapshot_height(view, composer, &model, screen, picker);
        self.prepare_frame_height(height)?;
        self.sync_native(view)?;
        if self.mode == TerminalMode::Classic
            && let Some(TerminalNotice::History(text)) = notice
        {
            self.record_classic_notice(view, text)?;
        }
        let area = self
            .owned
            .terminal_with_context("inline terminal")
            .get_frame()
            .area();
        if matches!(state, SnapshotState::Busy) {
            model.draft_action = DraftAction::Retain;
            model.status_line = "Busy · Ctrl+C interrupts".to_owned();
        }
        if self.clipboard.is_some() {
            model.draft_action = DraftAction::Retain;
        }
        if let Some(mode) = composer.approval_mode() {
            append_approval_mode(&mut model, mode, area.width);
        }
        let history_height = view::frame_layout(composer, area, model.activity_lines.len()).history;
        self.history_mut()
            .sync(view, usize::from(area.width), usize::from(history_height));
        if let Some(notice) = notice {
            self.history_mut().record_notice(notice);
        }
        let history_rows = if self.mode == TerminalMode::Fullscreen {
            self.history_mut().visible(view)
        } else {
            self.transcript.classic_history.live_rows(view)
        };
        let history_status = self.history_status(view, &mut model, area.width);
        let status_notice = notice
            .filter(|notice| {
                matches!(notice, TerminalNotice::Transient(_))
                    || (self.mode == TerminalMode::Fullscreen && history_height == 0)
            })
            .map(TerminalNotice::text);
        Ok(ChatFrameInput {
            model,
            height,
            history_rows,
            history_status,
            status_notice,
        })
    }

    fn render_chat_frame(
        &mut self,
        snapshot: &ChatSnapshot<'_>,
        input: ChatFrameInput<'_>,
    ) -> Result<(), TerminalError> {
        let (composer, picker) = (snapshot.composer, snapshot.picker);
        let ChatFrameInput {
            model,
            height,
            history_rows,
            history_status,
            status_notice,
        } = input;
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        let mut history_area = Rect::default();
        let mut image_chip_areas = Vec::new();
        let mut preview_inner = None;
        let pointer_position = self.hover.pointer;
        let mut hovered_image = self.hover.image;
        let graphics_protocol = self.owned.graphics_protocol;
        let published_graphics = self.hover.placement;
        let ready_graphics = self.hover.sixel.ready_placement();
        let newline_hint = composer.newline_hint(self.keyboard_support());
        self.draw_frame(height, |frame, _| {
            let geometry = view::draw_frame_with_history_and_chips(
                frame,
                view::FrameContent {
                    model: &model,
                    composer,
                    notice: status_notice,
                    hints: view::ComposerHints::new(palette, newline_hint),
                    history_rows: &history_rows,
                    history_status: history_status.as_deref(),
                },
            );
            history_area = geometry.history_area;
            image_chip_areas = geometry.image_chips;
            if let Some(position) = pointer_position {
                hovered_image = hovered_image_at(&image_chip_areas, position.x, position.y);
            }
            if let Some(index) = hovered_image {
                preview_inner =
                    draw_hover_preview(frame, composer, &image_chip_areas, index, |placement| {
                        graphics::preview_interior(
                            graphics_protocol,
                            published_graphics,
                            ready_graphics,
                            placement,
                        )
                    });
            }
            if let Some(picker) = picker {
                draw_model_catalog_frame(
                    frame,
                    ModelCatalogFrame {
                        picker,
                        armed,
                        composer,
                        model: &model,
                        hints: view::ComposerHints::new(palette, newline_hint),
                    },
                );
            }
        })?;
        self.panels.history_area = (self.mode == TerminalMode::Fullscreen).then_some(history_area);
        self.hover.chip_areas = image_chip_areas;
        self.hover.image = hovered_image;
        self.hover.digest = self
            .hover
            .image
            .and_then(|index| composer.images().get(index).map(ImageAttachment::digest));
        self.emit_graphics_preview(composer, preview_inner)?;
        Ok(())
    }

    fn emit_graphics_preview(
        &mut self,
        composer: &Composer,
        inner: Option<Rect>,
    ) -> Result<(), TerminalError> {
        let image = self
            .hover
            .image
            .and_then(|index| composer.images().get(index).map(|image| (index, image)));
        let preview = image.and_then(|(index, image)| inner.map(|inner| (index, image, inner)));
        let placement = preview.map(|(index, image, inner)| GraphicsPlacement {
            image_index: index,
            digest: image.digest(),
            inner,
        });
        if placement.is_none() {
            self.hover.sixel.clear();
        }
        let unchanged = placement.is_some() && self.hover.placement == placement;
        if unchanged {
            return Ok(());
        }
        let mut bytes = Vec::new();
        if self.hover.placement.take().is_some()
            && self.owned.graphics_protocol == Some(GraphicsProtocol::Kitty)
        {
            bytes.extend_from_slice(graphics::kitty_delete());
        }
        if let (Some(protocol), Some((_, image, inner))) = (self.owned.graphics_protocol, preview) {
            bytes.extend_from_slice(&match protocol {
                GraphicsProtocol::Kitty => graphics::kitty_image(image, inner),
                GraphicsProtocol::ITerm2 => graphics::inline_image(image, inner),
                GraphicsProtocol::Sixel(cell) => {
                    if let Some(placement) = placement {
                        self.hover.sixel.request(placement, image, cell);
                    }
                    let Some(bytes) = self.hover.sixel.take_ready() else {
                        return Ok(());
                    };
                    bytes
                }
            });
            self.hover.placement = placement;
        }
        if !bytes.is_empty() {
            self.owned().backend_mut().write_graphics(&bytes)?;
        }
        Ok(())
    }

    // The transcript pager hides the composer and gives detailed History every row above its footer.
    fn draw_pager(
        &mut self,
        view: &SessionView,
        notice: Option<TerminalNotice<'_>>,
        screen: Rect,
    ) -> Result<(), TerminalError> {
        self.transcript.history.sync(
            view,
            usize::from(screen.width),
            usize::from(screen.height.saturating_sub(1)),
        );
        if let Some(notice) = notice {
            self.transcript.history.record_notice(notice);
        }
        let rows = self.transcript.history.visible(view);
        let footer = match notice {
            Some(TerminalNotice::Transient(text)) => text.to_owned(),
            _ => self.transcript.history.pager_status(),
        };
        let palette = self.palette();
        let mut history_area = Rect::default();
        self.draw_frame(screen.height, |frame, _| {
            history_area = view::draw_pager_frame(frame, &rows, &footer, palette);
        })?;
        self.panels.history_area = Some(history_area);
        Ok(())
    }

    pub fn draw_progress(&mut self, view: &SessionView, notice: &str) -> Result<(), TerminalError> {
        self.draw_progress_with_draft(view, &Composer::default(), notice)
    }

    pub fn draw_progress_with_draft(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: &str,
    ) -> Result<(), TerminalError> {
        if self.linear.is_none()
            && view
                .runs
                .last()
                .is_some_and(|run| run.status == RunStatus::Active)
        {
            return self.draw_notice(view, composer, Some(TerminalNotice::Transient(notice)));
        }
        self.draw_busy_notice(view, composer, Some(TerminalNotice::Transient(notice)))
    }

    pub fn draw_busy(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.draw_busy_notice(view, composer, notice.map(TerminalNotice::History))
    }

    pub fn draw_busy_notice(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
    ) -> Result<(), TerminalError> {
        self.draw_in_flight(view, composer, notice, SnapshotState::Busy)
    }

    pub fn draw_preparing(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.draw_preparing_notice(view, composer, notice.map(TerminalNotice::History))
    }

    pub fn draw_preparing_notice(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
    ) -> Result<(), TerminalError> {
        let notice = if self.is_linear() {
            notice.or(Some(TerminalNotice::Transient(
                "Preparing request... Ctrl+C cancels",
            )))
        } else {
            notice
        };
        self.draw_in_flight(view, composer, notice, SnapshotState::Preparing)
    }

    fn draw_in_flight(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
        state: SnapshotState,
    ) -> Result<(), TerminalError> {
        if let Some(selected) = self.panels.help_selected {
            return self.draw_help(selected, view);
        }
        if self.agent_panel.is_some() {
            return self.draw_agents(view);
        }
        if self.agent_chat_shown(view) {
            return self.draw_agent_chat(view, notice);
        }
        if let Some(linear) = &mut self.linear {
            if std::mem::take(&mut self.clipboard_preview) {
                linear
                    .write_clipboard_draft(&mut io::stderr(), composer)
                    .map_err(TerminalError::Io)?;
            }
            let prompt_needed = linear.prompt_needed;
            linear.prompt_needed = false;
            let result = linear.draw(
                view,
                notice.map(TerminalNotice::text),
                composer.approval_mode(),
            );
            linear.prompt_needed = prompt_needed;
            return result.map_err(TerminalError::Io);
        }
        self.draw_snapshot(view, composer, notice, state, None)
    }
}

// A Classic viewport is exactly its dialog; Fullscreen dialogs sit at the bottom under live
// history.
pub(super) fn dialog_area(frame: Rect, height: u16, classic: bool) -> Rect {
    if classic {
        frame
    } else {
        bottom_area(frame, height)
    }
}

fn append_approval_mode(model: &mut PresentationModel, mode: arany::ApprovalMode, width: u16) {
    let permissions = if width >= 64 {
        format!("{} · Shift+Tab", mode.label())
    } else {
        mode.label().to_owned()
    };
    model.status_line = crate::presentation::safe_truncate(
        &format!("{} · {permissions}", model.status_line),
        usize::from(width),
    );
}

// The hovered image's preview balloon, drawn beside its chip; returns the preview interior.
fn draw_hover_preview(
    frame: &mut ratatui::Frame,
    composer: &Composer,
    chips: &[(usize, Rect)],
    index: usize,
    interior: impl FnOnce(GraphicsPlacement) -> view::PreviewInterior,
) -> Option<Rect> {
    let image = composer.images().get(index)?;
    let chip = chips
        .iter()
        .find_map(|(chip_index, area)| (*chip_index == index).then_some(*area))?;
    let (balloon, inner) = graphics::preview_geometry(frame.area(), chip, image)?;
    let placement = GraphicsPlacement {
        image_index: index,
        digest: image.digest(),
        inner,
    };
    view::draw_image_preview(
        frame,
        balloon,
        &image_description(image, index),
        interior(placement),
    );
    Some(inner)
}

#[derive(Clone, Copy)]
struct ChatSnapshot<'a> {
    view: &'a SessionView,
    composer: &'a Composer,
    notice: Option<TerminalNotice<'a>>,
    state: SnapshotState,
    picker: Option<&'a ModelPicker<'a>>,
    screen: Rect,
}

struct ChatFrameInput<'a> {
    model: PresentationModel,
    height: u16,
    history_rows: Vec<history::HistoryRow>,
    history_status: Option<String>,
    status_notice: Option<&'a str>,
}
