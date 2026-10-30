use super::*;

impl AttachedTerminal {
    // Mouse gestures select History text; only they, a copy key or `/copy` reach the clipboard writer.
    pub(super) async fn pointer_input(
        &mut self,
        input: TerminalInput,
        view: &SessionView,
        composer: &Composer,
    ) -> Option<bool> {
        let mouse_enabled = mouse_capture_enabled();
        let selects = mouse_enabled;
        match input {
            TerminalInput::PointerClick {
                column,
                row,
                clicks,
            } if selects => match self.history_cell(column, row) {
                Some((row, column)) => {
                    self.transcript.history.select_at(view, row, column, clicks);
                    Some(true)
                }
                None => self.transcript.history.clear_selection().then_some(true),
            },
            TerminalInput::PointerDrag { column, row }
                if selects && self.transcript.history.selecting() =>
            {
                let area = self.panels.history_area?;
                // Dragging past either edge scrolls one row per motion event.
                let row = if row < area.y {
                    self.transcript.history.scroll(true, 1);
                    0
                } else if row >= area.bottom() {
                    self.transcript.history.scroll(false, 1);
                    area.height.saturating_sub(1)
                } else {
                    row - area.y
                };
                self.transcript.history.extend_selection(
                    view,
                    usize::from(row),
                    usize::from(column.saturating_sub(area.x)),
                );
                Some(true)
            }
            TerminalInput::PointerRelease { .. } if self.pointer.copy_on_select => {
                self.copy_selection(view).await.then_some(true)
            }
            TerminalInput::Interrupt | TerminalInput::CopySelection => {
                self.copy_selection(view).await.then_some(true)
            }
            TerminalInput::Escape
                if self.transcript.history.has_selection()
                    && self.clipboard.is_none()
                    && !self.transcript.history.find_open() =>
            {
                Some(self.transcript.history.clear_selection())
            }
            TerminalInput::Up | TerminalInput::Down
                if !mouse_enabled
                    && composer.is_empty()
                    && !composer.has_open_menu()
                    && !composer.recall_pending() =>
            {
                Some(
                    self.transcript
                        .history
                        .scroll(input == TerminalInput::Up, 1),
                )
            }
            _ => None,
        }
    }

    pub(super) async fn panel_pointer_input(
        &mut self,
        input: TerminalInput,
        mouse_enabled: bool,
    ) -> Option<TerminalInput> {
        if !self.panels.panel_selection.enabled
            || self.is_linear()
            || self.mode != TerminalMode::Fullscreen
            || !mouse_enabled
        {
            return None;
        }
        if self.panels.panel_selection.selecting()
            && !matches!(
                input,
                TerminalInput::PointerDrag { .. }
                    | TerminalInput::PointerRelease { .. }
                    | TerminalInput::CopySelection
                    | TerminalInput::Escape
            )
        {
            self.panels.panel_selection.clear();
        }
        let area = self.panels.panel_selection.area?;
        let cell = |column, row| {
            area.contains(ratatui::layout::Position::new(column, row))
                .then(|| (usize::from(column - area.x), usize::from(row - area.y)))
        };
        match input {
            TerminalInput::PointerClick { column, row, .. } => {
                let (column, row) = cell(column, row)?;
                self.panels.panel_selection.select(column, row);
                Some(TerminalInput::PanelSelectionChanged)
            }
            TerminalInput::PointerDrag { column, row }
                if self.panels.panel_selection.selecting() =>
            {
                let column = column.clamp(area.x, area.right().saturating_sub(1));
                let row = row.clamp(area.y, area.bottom().saturating_sub(1));
                let (column, row) = cell(column, row)?;
                self.panels.panel_selection.extend(column, row);
                Some(TerminalInput::PanelSelectionChanged)
            }
            TerminalInput::PointerRelease { .. }
                if self.pointer.copy_on_select && self.panels.panel_selection.shown() =>
            {
                self.start_panel_copy();
                Some(TerminalInput::PanelSelectionChanged)
            }
            TerminalInput::CopySelection if self.panels.panel_selection.shown() => {
                self.start_panel_copy();
                Some(TerminalInput::PanelSelectionChanged)
            }
            TerminalInput::Escape if self.panels.panel_selection.shown() => {
                self.panels.panel_selection.clear();
                Some(TerminalInput::PanelSelectionChanged)
            }
            _ => None,
        }
    }

    fn start_panel_copy(&mut self) {
        if self.panels.panel_copy.is_some() {
            self.panels.panel_selection.feedback = Some("Copy already in progress".to_owned());
            return;
        }
        let Some(text) = self.panels.panel_selection.text() else {
            return;
        };
        let selection = match text {
            Ok(selection) => selection,
            Err(error) => {
                self.panels.panel_selection.feedback = Some(error.to_owned());
                return;
            }
        };
        match clipboard::CopyRequest::selection(selection.clone()) {
            Ok(request) => {
                let local_request = request.clone();
                self.panels.panel_copy = Some(PanelCopy {
                    task: tokio::spawn(async move { clipboard::copy_local(&local_request).await }),
                    selection,
                    request,
                });
            }
            Err(error) => self.panels.panel_selection.feedback = Some(error.to_owned()),
        }
    }

    pub(super) fn finish_panel_copy(
        &mut self,
        result: PanelCopyResult,
        terminal_output: Option<&mut dyn Write>,
    ) {
        self.panels.panel_copy = None;
        let PanelCopyResult {
            selection,
            request,
            result,
        } = result;
        let result = match result {
            Ok(Some(cue)) => Ok(cue),
            Ok(None) => {
                let terminal_output = (!self.is_linear()).then_some(terminal_output).flatten();
                clipboard::copy_to_terminal(&request, terminal_output)
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(cue) => {
                let same_selection = self
                    .panels
                    .panel_selection
                    .text()
                    .and_then(Result::ok)
                    .is_some_and(|current| current == selection);
                if same_selection {
                    self.panels.panel_selection.clear();
                }
                self.panels.panel_selection.feedback = Some(cue);
            }
            Err(error) => self.panels.panel_selection.feedback = Some(error),
        }
    }

    // Pointer input with nothing to do, discarded while chat waits so it causes no redraw.
    pub fn ignores_pointer(&mut self, input: TerminalInput) -> bool {
        let selects =
            !self.is_linear() && self.mode == TerminalMode::Fullscreen && mouse_capture_enabled();
        match input {
            TerminalInput::PointerMove { .. } => {
                !selects || !std::mem::take(&mut self.hover.changed)
            }
            TerminalInput::PointerClick { column, row, .. } => {
                !selects
                    || (self.history_cell(column, row).is_none()
                        && !self.transcript.history.selecting())
            }
            TerminalInput::PointerDrag { .. } => {
                let graphics_changed = std::mem::take(&mut self.hover.changed);
                !selects || (!self.transcript.history.selecting() && !graphics_changed)
            }
            TerminalInput::PointerRelease { .. } => {
                if !self.transcript.history.has_selection() {
                    self.transcript.history.clear_selection();
                }
                let graphics_changed = std::mem::take(&mut self.hover.changed);
                !selects
                    || (!graphics_changed
                        && (!self.pointer.copy_on_select
                            || !self.transcript.history.has_selection()))
            }
            _ => false,
        }
    }

    pub fn take_graphics_hover_changed(&mut self) -> bool {
        std::mem::take(&mut self.hover.changed)
    }

    fn history_cell(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        let area = self.panels.history_area?;
        area.contains(ratatui::layout::Position::new(column, row))
            .then(|| (usize::from(row - area.y), usize::from(column - area.x)))
    }

    // Returns false when nothing is selected, so the input keeps its ordinary meaning.
    async fn copy_selection(&mut self, view: &SessionView) -> bool {
        let Some(text) = self.transcript.history.selection_text(view) else {
            self.transcript.history.clear_selection();
            return false;
        };
        let mut stderr = io::stderr();
        let copied = match text.and_then(clipboard::CopyRequest::selection) {
            Ok(request) => {
                clipboard::copy(request, Some(&mut stderr as &mut (dyn Write + Send))).await
            }
            Err(error) => Err(error.to_owned()),
        };
        match copied {
            Ok(cue) => {
                self.transcript.history.clear_selection();
                self.panels.selection_cue =
                    Some(if SELECTION_HINT_SHOWN.swap(true, Ordering::Relaxed) {
                        cue
                    } else {
                        format!(
                            "{cue} · {}",
                            clipboard::native_selection_hint(&|name| std::env::var_os(name))
                        )
                    });
            }
            Err(error) => self
                .transcript
                .history
                .record_notice(TerminalNotice::History(&error)),
        }
        true
    }
}

pub(super) async fn wait_panel_copy(panel_copy: &mut Option<PanelCopy>) -> PanelCopyResult {
    let Some(copy) = panel_copy.as_mut() else {
        return std::future::pending().await;
    };
    let selection = copy.selection.clone();
    let request = copy.request.clone();
    let result = match (&mut copy.task).await {
        Ok(result) => result,
        Err(_) => Err("Copy unavailable: clipboard operation failed".to_owned()),
    };
    PanelCopyResult {
        selection,
        request,
        result,
    }
}

impl PanelSelection {
    pub(super) fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
    }

    fn selecting(&self) -> bool {
        self.anchor.is_some()
    }

    fn shown(&self) -> bool {
        self.anchor
            .zip(self.head)
            .is_some_and(|(anchor, head)| anchor != head)
    }

    fn select(&mut self, column: usize, row: usize) {
        self.anchor = Some((row, column));
        self.head = Some((row, column));
    }

    fn extend(&mut self, column: usize, row: usize) {
        if self.selecting() {
            self.head = Some((row, column));
        }
    }

    fn text(&self) -> Option<Result<String, &'static str>> {
        let (mut start, mut end) = self.anchor.zip(self.head)?;
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        let mut selected = String::new();
        let mut bytes = 0usize;
        for row in start.0..=end.0 {
            let line = self.lines.get(row)?;
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 {
                end.1.saturating_add(1)
            } else {
                usize::MAX
            };
            let mut column = 0;
            if row > start.0 {
                bytes = bytes.saturating_add(1);
                if bytes > MAX_COPY_BYTES {
                    return Some(Err(COPY_TOO_LARGE));
                }
                selected.push('\n');
            }
            for grapheme in line.graphemes(true) {
                let next = column + UnicodeWidthStr::width(grapheme);
                if next > from && column < to {
                    bytes = bytes.saturating_add(display_bytes(grapheme));
                    if bytes > MAX_COPY_BYTES {
                        return Some(Err(COPY_TOO_LARGE));
                    }
                    selected.push_str(grapheme);
                }
                column = next;
            }
        }
        let selected = selected.trim_end();
        (!selected.is_empty()).then(|| Ok(display_text(selected)))
    }

    pub(super) fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if self.area != Some(area) {
            self.clear();
        }
        self.area = Some(area);
        if area.is_empty() {
            return;
        }
        let (Some((mut start_row, mut start_col)), Some((mut end_row, mut end_col))) =
            (self.anchor, self.head)
        else {
            return;
        };
        if (end_row, end_col) < (start_row, start_col) {
            std::mem::swap(&mut start_row, &mut end_row);
            std::mem::swap(&mut start_col, &mut end_col);
        }
        for row in start_row..=end_row {
            let from = if row == start_row { start_col } else { 0 };
            let to = if row == end_row {
                end_col
            } else {
                usize::from(area.width).saturating_sub(1)
            };
            for column in from..=to {
                if let (Ok(x), Ok(y)) = (u16::try_from(column), u16::try_from(row)) {
                    let x = area.x.saturating_add(x);
                    let y = area.y.saturating_add(y);
                    if area.contains(ratatui::layout::Position::new(x, y))
                        && let Some(cell) = frame.buffer_mut().cell_mut((x, y))
                    {
                        cell.set_style(cell.style().add_modifier(
                            ratatui::style::Modifier::REVERSED
                                | ratatui::style::Modifier::UNDERLINED,
                        ));
                    }
                }
            }
        }
    }
}

pub(super) fn hovered_image_at(chips: &[(usize, Rect)], column: u16, row: u16) -> Option<usize> {
    chips.iter().find_map(|(index, area)| {
        area.contains(ratatui::layout::Position::new(column, row))
            .then_some(*index)
    })
}

pub(super) fn pointer_position(input: TerminalInput) -> Option<(u16, u16)> {
    match input {
        TerminalInput::PointerMove { column, row }
        | TerminalInput::PointerDrag { column, row }
        | TerminalInput::PointerRelease { column, row } => Some((column, row)),
        _ => None,
    }
}

pub(super) fn transition_hover(current: &mut Option<usize>, next: Option<usize>) -> bool {
    if *current == next {
        return false;
    }
    *current = next;
    true
}

pub(super) fn rendered_panel_lines(buffer: &ratatui::buffer::Buffer, area: Rect) -> Vec<String> {
    (area.y..area.bottom())
        .map(|y| {
            let mut line = String::new();
            let mut skip = 0u16;
            for x in area.x..area.right() {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if let Some(cell) = buffer.cell((x, y)) {
                    line.push_str(cell.symbol());
                    skip = cell.cell_width().saturating_sub(1);
                }
            }
            line.trim_end().to_owned()
        })
        .collect()
}

pub(super) fn draw_panel_feedback(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    feedback: &str,
    palette: Palette,
) {
    if area.is_empty() {
        return;
    }
    let text = crate::presentation::escape_terminal(feedback);
    let text = crate::presentation::safe_truncate(&text, usize::from(area.width));
    let feedback_area = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    frame.render_widget(ratatui::widgets::Clear, feedback_area);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(text).style(palette.attention()),
        feedback_area,
    );
}

#[cfg(test)]
mod panel_selection_tests {
    use super::super::{
        PanelCopy, PanelSelection, PointerSettings, TerminalInput, TerminalMode,
        rendered_panel_lines, wait_panel_copy,
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Rect, widgets::Paragraph};

    #[test]
    fn panel_selection_copies_visible_rows_and_wide_graphemes() {
        let mut selection = PanelSelection {
            lines: vec!["Usage: café界".into(), "  remaining".into()],
            ..PanelSelection::default()
        };
        selection.select(7, 0);
        selection.extend(3, 1);
        assert_eq!(selection.text().unwrap().unwrap(), "café界\n  re");
    }

    #[test]
    fn panel_selection_requires_a_drag_before_it_is_shown() {
        let mut selection = PanelSelection {
            lines: vec!["panel".into()],
            ..PanelSelection::default()
        };
        selection.select(1, 0);
        assert!(!selection.shown());
        selection.extend(2, 0);
        assert!(selection.shown());
        selection.clear();
        assert!(!selection.selecting());
    }

    #[test]
    fn panel_selection_clears_before_rendering_changed_dialog_bounds() {
        let previous = Rect::new(0, 1, 8, 3);
        for current in [
            previous,
            Rect::new(0, 2, 8, 3),
            Rect::new(0, 1, 8, 1),
            Rect::new(0, 1, 4, 3),
        ] {
            let mut selection = PanelSelection {
                area: Some(previous),
                lines: vec!["panel".into(); usize::from(previous.height)],
                ..PanelSelection::default()
            };
            selection.select(0, 2);
            selection.extend(2, 2);
            let mut terminal = Terminal::new(TestBackend::new(8, 6)).expect("test terminal");
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new("panel\npanel\npanel"), current);
                    selection.render(frame, current);
                })
                .expect("panel frame");
            let highlighted = (0..6).any(|row| {
                (0..8).any(|column| {
                    terminal.backend().buffer()[(column, row)]
                        .modifier
                        .contains(ratatui::style::Modifier::UNDERLINED)
                })
            });
            let unchanged = current == previous;
            assert_eq!(highlighted, unchanged, "panel bounds {current:?}");
            assert_eq!(selection.shown(), unchanged, "panel bounds {current:?}");
            assert_eq!(selection.text(), unchanged.then(|| Ok("pan".into())));
            assert_eq!(selection.area, Some(current));
        }
    }

    #[test]
    fn panel_selection_reads_wide_graphemes_from_a_rendered_buffer() {
        let mut terminal = Terminal::new(TestBackend::new(8, 1)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("界abc"), frame.area()))
            .expect("wide text frame");
        let lines = rendered_panel_lines(terminal.backend().buffer(), Rect::new(0, 0, 8, 1));
        assert_eq!(lines[0], "界abc");

        let mut selection = PanelSelection {
            lines,
            ..PanelSelection::default()
        };
        selection.select(2, 0);
        selection.extend(3, 0);
        assert_eq!(selection.text().unwrap().unwrap(), "ab");
    }

    #[tokio::test]
    async fn pending_panel_copy_survives_run_progress_and_reports_once() {
        let mut terminal = crate::terminal::clipboard::cleanup_tests::released_terminal(None);
        terminal.panels.panel_selection.lines = vec!["abc".into()];
        terminal.panels.panel_selection.select(0, 0);
        terminal.panels.panel_selection.extend(2, 0);

        let (copy_started, copy_started_rx) = tokio::sync::oneshot::channel();
        let (finish_copy, finish_copy_rx) = tokio::sync::oneshot::channel();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let completed_task = completed.clone();
        let request = crate::terminal::clipboard::CopyRequest::selection("abc".into())
            .expect("small test selection");
        terminal.panels.panel_copy = Some(PanelCopy {
            task: tokio::spawn(async move {
                let _ = copy_started.send(());
                let _ = finish_copy_rx.await;
                completed_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Some("Copied selection to clipboard".to_owned()))
            }),
            selection: "abc".into(),
            request,
        });
        copy_started_rx.await.expect("copy started");

        let (progress, progress_rx) = tokio::sync::oneshot::channel();
        progress
            .send("progress")
            .expect("progress receiver is waiting");
        let copy_wait = wait_panel_copy(&mut terminal.panels.panel_copy);
        tokio::select! {
            _ = copy_wait => panic!("copy completed before release"),
            result = progress_rx => assert_eq!(result.expect("Run progress"), "progress"),
        }

        finish_copy.send(()).expect("copy task is waiting");
        let result = wait_panel_copy(&mut terminal.panels.panel_copy).await;
        terminal.finish_panel_copy(result, None);

        assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            terminal.panels.panel_selection.feedback.as_deref(),
            Some("Copied selection to clipboard")
        );
        assert!(!terminal.panels.panel_selection.shown());
    }

    #[tokio::test]
    async fn panel_copy_feedback_keeps_the_clipboard_error_prefix_once() {
        let mut terminal = crate::terminal::clipboard::cleanup_tests::released_terminal(None);
        let request = crate::terminal::clipboard::CopyRequest::selection("abc".into())
            .expect("small test selection");
        terminal.panels.panel_copy = Some(PanelCopy {
            task: tokio::spawn(async { Err("Copy unavailable: clipboard unavailable".to_owned()) }),
            selection: "abc".into(),
            request,
        });

        let result = wait_panel_copy(&mut terminal.panels.panel_copy).await;
        terminal.finish_panel_copy(result, None);

        assert_eq!(
            terminal.panels.panel_selection.feedback.as_deref(),
            Some("Copy unavailable: clipboard unavailable")
        );
    }

    #[tokio::test]
    async fn pending_panel_copy_does_not_use_osc52_after_linear_replacement() {
        let mut terminal = crate::terminal::clipboard::cleanup_tests::released_terminal(None);
        terminal.mode = TerminalMode::Fullscreen;
        terminal.panels.panel_selection.lines = vec!["abc".into()];
        terminal.panels.panel_selection.select(0, 0);
        terminal.panels.panel_selection.extend(2, 0);
        let request = crate::terminal::clipboard::CopyRequest::selection("abc".into())
            .expect("small test selection");
        let (copy_started, copy_started_rx) = tokio::sync::oneshot::channel();
        let (finish_copy, finish_copy_rx) = tokio::sync::oneshot::channel();
        terminal.panels.panel_copy = Some(PanelCopy {
            task: tokio::spawn(async move {
                let _ = copy_started.send(());
                let _ = finish_copy_rx.await;
                Ok(None)
            }),
            selection: "abc".into(),
            request,
        });
        copy_started_rx.await.expect("copy started");

        let replacement = crate::terminal::clipboard::cleanup_tests::released_terminal(Some(
            crate::terminal::linear::LinearState::new("test"),
        ));
        terminal.replace_released(replacement);
        assert!(terminal.is_linear());
        assert!(
            terminal.panels.panel_copy.is_some(),
            "the pending task survives replacement"
        );

        finish_copy.send(()).expect("copy task is waiting");
        let result = wait_panel_copy(&mut terminal.panels.panel_copy).await;
        let mut output = Vec::new();
        terminal.finish_panel_copy(result, Some(&mut output));

        assert!(output.is_empty(), "linear fallback emits no OSC 52");
        assert_eq!(
            terminal.panels.panel_selection.feedback.as_deref(),
            Some(
                "Copy unavailable: no native or tmux clipboard worked; OSC 52 unavailable in linear mode"
            )
        );
        assert!(
            terminal.panels.panel_selection.shown(),
            "failed copy keeps selection"
        );
    }

    #[test]
    fn panel_copy_journey_keeps_failed_selection_and_escape_precedes_close() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut terminal =
                    crate::terminal::clipboard::cleanup_tests::released_terminal(None);
                terminal.mode = TerminalMode::Fullscreen;
                terminal.pointer = PointerSettings {
                    copy_on_select: false,
                };
                terminal.panels.panel_selection.enabled = true;
                terminal.panels.panel_selection.area = Some(Rect::new(0, 0, 4_096, 25));
                terminal.panels.panel_selection.lines = vec!["a".repeat(4_096); 25];

                let click = TerminalInput::PointerClick {
                    column: 0,
                    row: 0,
                    clicks: 1,
                };
                assert_eq!(
                    terminal.panel_pointer_input(click, false).await,
                    None,
                    "disabled capture ignores panel clicks"
                );
                assert!(!terminal.panels.panel_selection.selecting());
                assert_eq!(
                    terminal.panel_pointer_input(click, true).await,
                    Some(TerminalInput::PanelSelectionChanged)
                );
                assert_eq!(
                    terminal
                        .panel_pointer_input(
                            TerminalInput::PointerDrag {
                                column: 4_095,
                                row: 24,
                            },
                            true
                        )
                        .await,
                    Some(TerminalInput::PanelSelectionChanged)
                );
                assert_eq!(
                    terminal
                        .panel_pointer_input(
                            TerminalInput::PointerRelease {
                                column: 4_095,
                                row: 24,
                            },
                            true
                        )
                        .await,
                    None,
                    "Copy on select is disabled"
                );
                assert!(terminal.panels.panel_selection.shown());

                assert_eq!(
                    terminal
                        .panel_pointer_input(TerminalInput::CopySelection, true)
                        .await,
                    Some(TerminalInput::PanelSelectionChanged),
                    "explicit copy still runs"
                );
                assert!(
                    terminal.panels.panel_selection.shown(),
                    "failure keeps selection"
                );
                assert_eq!(
                    terminal.panels.panel_selection.feedback.as_deref(),
                    Some(super::super::COPY_TOO_LARGE)
                );
                assert_eq!(
                    terminal
                        .panel_pointer_input(TerminalInput::Escape, true)
                        .await,
                    Some(TerminalInput::PanelSelectionChanged)
                );
                assert!(!terminal.panels.panel_selection.selecting());
                assert_eq!(
                    terminal
                        .panel_pointer_input(TerminalInput::Escape, true)
                        .await,
                    None,
                    "the next Escape reaches the panel's close handler"
                );
            });
    }
}
