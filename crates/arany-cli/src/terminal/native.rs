use super::*;

impl AttachedTerminal {
    pub(super) fn insert_native(
        &mut self,
        kind: history::MessageKind,
        text: &str,
        skill: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.transcript.history_adjacent = true;
        let palette = self.palette();
        let terminal = self.owned();
        let width = terminal.get_frame().area().width;
        let rows = history::display_rows(kind, text, usize::from(width), skill);
        let viewport = terminal.get_frame().area();
        let screen = terminal.size()?.height;
        // Ratatui would need a one-row scroll region here, which DECSTBM terminals such as tmux reject.
        if screen < viewport.height + 2 {
            let lines = rows
                .iter()
                .map(|row| view::native_line(row, width, palette))
                .chain(std::iter::once(ratatui::text::Line::default()));
            insert_rows_scrolling_screen(terminal, screen, lines)?;
            return Ok(());
        }
        for chunk in rows.chunks(64) {
            terminal.insert_before(chunk.len() as u16, |buffer| {
                use ratatui::widgets::Widget;
                let lines = chunk
                    .iter()
                    .map(|row| view::native_line(row, width, palette))
                    .collect::<Vec<_>>();
                ratatui::widgets::Paragraph::new(lines).render(buffer.area, buffer);
            })?;
        }
        terminal.insert_before(1, |_| {})?;
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        Ok(())
    }

    pub(super) fn sync_native(&mut self, session: &SessionView) -> Result<(), TerminalError> {
        if self.mode != TerminalMode::Classic {
            return Ok(());
        }
        if self.transcript.native.session != Some(session.id) {
            let width = usize::from(self.owned().get_frame().area().width);
            let projection = history::classic_projection(session, width);
            self.transcript.native = NativeTranscript {
                session: Some(session.id),
                initial_run_count: Some(session.runs.len()),
                first_projected_run_index: projection.first_run_index,
                omitted_history: projection.omitted_history,
                ..NativeTranscript::default()
            };
        }
        for (index, run) in session.runs.iter().enumerate() {
            if self
                .transcript
                .native
                .initial_run_count
                .is_some_and(|count| index < count)
                && self
                    .transcript
                    .native
                    .first_projected_run_index
                    .is_none_or(|first| index < first)
            {
                continue;
            }
            if !self.transcript.native.objectives.contains(&run.id) {
                if self.transcript.native.objectives.is_empty() {
                    let terminal = self.owned();
                    let area = terminal.get_frame().area();
                    if history_top_padding(terminal.size()?.height.saturating_sub(area.height)) > 0
                    {
                        terminal.insert_before(1, |_| {})?;
                        let area = terminal.get_frame().area();
                        terminal.backend_mut().set_area(area);
                    }
                }
                self.insert_native(
                    history::MessageKind::User,
                    &crate::presentation::image_message_text(&run.objective, &run.images),
                    run.skill.as_deref(),
                )?;
                self.transcript.native.objectives.insert(run.id);
            }
            for (range, summary) in crate::presentation::settled_tool_groups(run) {
                if !self
                    .transcript
                    .native
                    .tools
                    .contains(&(run.id, range.start))
                {
                    self.insert_native(history::MessageKind::Tool, &summary, None)?;
                    for index in range {
                        self.transcript.native.tools.insert((run.id, index));
                    }
                }
            }
            if !matches!(
                run.status,
                arany::RunStatus::Pending | arany::RunStatus::Active
            ) && run.assistant_message.is_none()
                && !self.transcript.native.answers.contains(&run.id)
                && let Some(text) = crate::presentation::display_answer(run)
            {
                self.insert_native(history::MessageKind::Incomplete, &text, None)?;
                self.transcript.native.answers.insert(run.id);
            }
            if self.active_run != Some(run.id)
                && let Some(answer) = &run.assistant_message
                && !self.transcript.native.answers.contains(&run.id)
            {
                self.insert_native(history::MessageKind::Assistant, answer, None)?;
                self.transcript.native.answers.insert(run.id);
            }
        }
        Ok(())
    }

    pub fn record_tool_rows(&mut self, run: &arany::RunView) -> Result<(), TerminalError> {
        if let Some(linear) = &mut self.linear {
            linear.write_tools(&mut io::stderr(), run)?;
        }
        Ok(())
    }

    pub fn record_user_message(&mut self, objective: &str) -> Result<(), TerminalError> {
        if let Some(linear) = &mut self.linear {
            return linear
                .write_user_message(&mut io::stderr(), objective, None)
                .map_err(TerminalError::Io);
        }
        Ok(())
    }

    pub fn record_user_run(&mut self, run: &arany::RunView) -> Result<(), TerminalError> {
        self.active_run = Some(run.id);
        if let Some(linear) = &mut self.linear {
            let mut writer = io::stderr();
            linear.write_user_message(&mut writer, &run.objective, run.skill.as_deref())?;
            for line in image_message_lines(&run.images, 80) {
                writeln!(writer, "{line}")?;
            }
            writer.flush()?;
        }
        Ok(())
    }

    pub fn print_user_message(objective: &str) -> Result<(), TerminalError> {
        Self::print_user_message_with_heading(objective, "\n› You:")
    }

    fn print_user_message_with_heading(
        objective: &str,
        heading: &str,
    ) -> Result<(), TerminalError> {
        let mut writer = io::stderr();
        writeln!(writer, "{heading}")?;
        for line in user_message_lines(objective, 80).into_iter().skip(1) {
            writeln!(writer, "{line}")?;
        }
        writer.flush().map_err(TerminalError::Io)
    }

    pub fn print_user_run(run: &arany::RunView, inline: bool) -> Result<(), TerminalError> {
        let heading = commands::invoked_skill_token(&run.objective, run.skill.as_deref())
            .map(|token| format!("{}You ({}):", if inline { "\n› " } else { "" }, token.kind));
        Self::print_user_message_with_heading(
            &run.objective,
            heading
                .as_deref()
                .unwrap_or(if inline { "\n› You:" } else { "You:" }),
        )?;
        let mut writer = io::stderr();
        for line in image_message_lines(&run.images, 80) {
            writeln!(writer, "{line}")?;
        }
        writer.flush().map_err(TerminalError::Io)
    }

    pub fn print_assistant_run(run: &arany::RunView) -> Result<(), TerminalError> {
        io::stdout()
            .write_all(render_answer(run, "Arany · Answer:").as_bytes())
            .map_err(TerminalError::Io)
    }
}

// The escaped detail text bounded like `/transcript`: 16 Runs, 64 KiB and a limit cue.
pub(super) fn bounded_transcript(view: &SessionView) -> io::Result<String> {
    let mut text = Vec::new();
    LinearState::new("classic").write_transcript(&mut text, view)?;
    Ok(String::from_utf8(text).expect("escaped transcript is UTF-8"))
}

// History keeps a rectangle disjoint from bottom-area chrome, so it cannot overwrite a dialog.
pub(super) fn draw_history_above(
    frame: &mut ratatui::Frame<'_>,
    history: &mut History,
    session: &SessionView,
    bottom: u16,
    palette: Palette,
) -> Rect {
    let full = frame.area();
    let area = Rect::new(full.x, full.y, full.width, bottom.saturating_sub(full.y));
    history.sync(session, usize::from(area.width), usize::from(area.height));
    view::draw_history_rows(frame, area, &history.visible(session), palette);
    area
}

// Every Classic surface takes min(screen, its content height), as Codex's inline viewport
// does. A shrink keeps the viewport top while history stays visible above it, so the
// composer remains under the latest history; otherwise the viewport returns to the bottom.
pub(super) fn classic_keeps_top(area: Rect, history_adjacent: bool) -> bool {
    history_adjacent && area.y > 0
}

pub(super) fn resize_classic<B: Backend>(
    terminal: &mut Terminal<B>,
    cursor_offset: u16,
    height: u16,
    keep_top: bool,
    detached: impl FnOnce() -> B,
) -> Result<(), B::Error> {
    let size = terminal.size()?;
    let area = terminal.get_frame().area();
    let height = height.max(1).min(size.height);
    if area.width == size.width
        && area.height == height
        && area.bottom() <= size.height
        && (keep_top || area.bottom() == size.height)
    {
        return Ok(());
    }
    // Width reflow moves the caret; recover the owned top before clearing transient rows.
    let top = if size.width != area.width || size.height < area.bottom() {
        terminal
            .get_cursor_position()?
            .y
            .saturating_sub(cursor_offset)
    } else {
        area.y.min(size.height.saturating_sub(1))
    };
    let backend = terminal.backend_mut();
    backend.set_cursor_position((0, top))?;
    backend.clear_region(ratatui::backend::ClearType::AfterCursor)?;
    backend.show_cursor()?;
    // Growing from the top scrolls rows above into scrollback instead of overwriting them.
    let start = if keep_top {
        top
    } else {
        top.max(size.height.saturating_sub(height))
    };
    backend.set_cursor_position((0, start))?;
    let backend = std::mem::replace(backend, detached());
    let mut resized = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(height),
        },
    )?;
    let area = resized.get_frame().area();
    resized.backend_mut().set_cursor_position((0, area.y))?;
    resized
        .backend_mut()
        .clear_region(ratatui::backend::ClearType::AfterCursor)?;
    *terminal = resized;
    Ok(())
}

// Too short for Ratatui's scrolling region: draw rows as diffs from blank cells so wide-cell padding is not sent.
fn insert_rows_scrolling_screen<'a, B: Backend>(
    terminal: &mut Terminal<B>,
    screen: u16,
    lines: impl Iterator<Item = ratatui::text::Line<'a>>,
) -> Result<(), B::Error> {
    use ratatui::{backend::ClearType, buffer::Buffer, widgets::Paragraph, widgets::Widget};
    let viewport = terminal.get_frame().area();
    let backend = terminal.backend_mut();
    for line in lines {
        if viewport.y > 0 {
            backend.scroll_region_up(0..screen, 1)?;
        }
        let mut row = Buffer::empty(Rect::new(0, 0, viewport.width, 1));
        Paragraph::new(line).render(row.area, &mut row);
        backend.set_cursor_position((0, 0))?;
        backend.clear_region(ClearType::CurrentLine)?;
        backend.draw(Buffer::empty(row.area).diff(&row).into_iter())?;
        if viewport.y == 0 {
            backend.scroll_region_up(0..screen, 1)?;
        }
    }
    // Line erases keep tmux from scrolling the stale screen into history, unlike a home-cursor erase.
    for y in viewport.y..screen {
        backend.set_cursor_position((0, y))?;
        backend.clear_region(ClearType::CurrentLine)?;
    }
    terminal.swap_buffers();
    Ok(())
}

#[cfg(test)]
mod insert_rows_tests {
    use super::{classic_keeps_top, insert_rows_scrolling_screen, resize_classic};
    use ratatui::{
        Terminal, TerminalOptions, Viewport,
        backend::{Backend, ClearType, CrosstermBackend, TestBackend, WindowSize},
        buffer::{Buffer, Cell},
        layout::{Position, Rect, Size},
        text::Line,
    };
    use std::{cell::Cell as StateCell, cell::RefCell, io, rc::Rc};

    fn text_rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn rows_with_separator() -> impl Iterator<Item = Line<'static>> {
        ["alpha", "beta"]
            .into_iter()
            .map(Line::from)
            .chain(std::iter::once(Line::default()))
    }

    #[test]
    fn classic_resize_preserves_native_scrollback() {
        use ratatui::widgets::Paragraph;

        fn fixture(width: u16, height: u16, answer: &[String]) -> Terminal<TestBackend> {
            let mut backend = TestBackend::new(width, height);
            backend.set_cursor_position((0, height - 1)).unwrap();
            for line in answer {
                use ratatui::widgets::Widget;
                backend.append_lines(1).unwrap();
                let mut row = Buffer::empty(Rect::new(0, height - 1, width, 1));
                Paragraph::new(line.as_str()).render(row.area, &mut row);
                backend
                    .draw(Buffer::empty(row.area).diff(&row).into_iter())
                    .unwrap();
            }
            backend.append_lines(1).unwrap();
            let mut terminal = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(12),
                },
            )
            .unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    frame.render_widget(
                        Paragraph::new("stale composer draft ".repeat(4))
                            .wrap(ratatui::widgets::Wrap { trim: false }),
                        area,
                    );
                    frame.set_cursor_position((20, area.y + 2));
                })
                .unwrap();
            terminal
        }

        let answer = (1..=200)
            .map(|line| format!("native answer {line:03}"))
            .collect::<Vec<_>>();
        for width in [65, 40] {
            let mut terminal = fixture(80, 24, &answer);
            let before = text_rows(terminal.backend().scrollback())
                .into_iter()
                .chain(text_rows(terminal.backend().buffer()))
                .filter(|row| row.starts_with("native answer "))
                .collect::<Vec<_>>();
            assert_eq!(before, answer, "complete transcript before resize");
            for (columns, height) in [(width, 25), (80, 24)] {
                // TestBackend::resize reshapes its flat scrollback bytes instead of preserving rows.
                *terminal.backend_mut() = fixture(columns, height, &answer).backend().clone();
                resize_classic(&mut terminal, 2, 12, false, || TestBackend::new(1, 1)).unwrap();
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        frame.render_widget(Paragraph::new("current composer"), area);
                        frame.set_cursor_position((2, area.y + 2));
                    })
                    .unwrap();
                let rows = text_rows(terminal.backend().scrollback())
                    .into_iter()
                    .chain(text_rows(terminal.backend().buffer()))
                    .collect::<Vec<_>>();
                let actual = rows
                    .iter()
                    .filter(|row| row.starts_with("native answer "))
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(actual, answer, "native transcript at {columns}x{height}");
                assert!(rows.iter().all(|row| !row.contains("stale composer")));
            }
        }

        let mut height_shrunk = fixture(80, 24, &answer);
        let cursor = height_shrunk.backend().cursor_position();
        height_shrunk.backend_mut().resize(80, 20);
        height_shrunk
            .backend_mut()
            .set_cursor_position((cursor.x, cursor.y.min(19)))
            .unwrap();
        let before = text_rows(height_shrunk.backend().scrollback())
            .into_iter()
            .chain(text_rows(height_shrunk.backend().buffer()))
            .filter(|row| row.starts_with("native answer "))
            .collect::<Vec<_>>();
        assert_eq!(
            before, answer,
            "transcript survives backend 24-to-20 resize"
        );
        resize_classic(&mut height_shrunk, 2, 12, false, || TestBackend::new(1, 1)).unwrap();
        let rows = text_rows(height_shrunk.backend().scrollback())
            .into_iter()
            .chain(text_rows(height_shrunk.backend().buffer()))
            .collect::<Vec<_>>();
        let actual = rows
            .iter()
            .filter(|row| row.starts_with("native answer "))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(actual, answer, "native transcript after height-only shrink");

        let mut short = fixture(80, 8, &answer);
        assert_eq!(short.get_frame().area().height, 8);
        short.backend_mut().resize(80, 24);
        resize_classic(&mut short, 2, 12, false, || TestBackend::new(1, 1)).unwrap();
        assert_eq!(short.get_frame().area().height, 12);
        let rows = text_rows(short.backend().scrollback())
            .into_iter()
            .chain(text_rows(short.backend().buffer()))
            .collect::<Vec<_>>();
        let actual = rows
            .iter()
            .filter(|row| row.starts_with("native answer "))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            actual, answer,
            "native transcript after short-screen recovery"
        );
    }

    #[test]
    fn classic_viewport_takes_its_content_and_keeps_history_above_the_composer() {
        use ratatui::widgets::{Paragraph, Widget};
        for height in [8, 12, 24, 40, 50, 60] {
            let mut backend = TestBackend::new(80, height);
            let mut settled = Buffer::empty(Rect::new(0, height - 5, 80, 1));
            Paragraph::new("settled history").render(settled.area, &mut settled);
            backend
                .draw(Buffer::empty(settled.area).diff(&settled).into_iter())
                .unwrap();
            backend.set_cursor_position((0, height - 4)).unwrap();
            let mut terminal = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(4),
                },
            )
            .unwrap();
            let mut history_adjacent = true;
            for (needed, dialog) in [
                (4, false),
                (6, true),
                (4, false),
                (5, false),
                (height, true),
                (4, false),
            ] {
                let before = terminal.get_frame().area();
                let keep_top = classic_keeps_top(before, history_adjacent);
                resize_classic(&mut terminal, 0, needed, keep_top, || {
                    TestBackend::new(1, 1)
                })
                .unwrap();
                let adjacent_before = history_adjacent;
                history_adjacent &= terminal.get_frame().area().y <= before.y;
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        frame.render_widget(
                            Paragraph::new(if dialog { "dialog" } else { "composer" }),
                            area,
                        );
                        frame.set_cursor_position((2, area.y));
                    })
                    .unwrap();
                let area = terminal.get_frame().area();
                let context = format!("{height} rows, {needed} needed, dialog {dialog}");
                // As in Codex, every surface takes min(screen, its content height).
                assert_eq!(area.height, needed.min(height), "{context}");
                if area.height < before.height {
                    if adjacent_before && before.y > 0 {
                        assert_eq!(area.y, before.y, "a shrink keeps the top: {context}");
                    } else {
                        assert_eq!(
                            area.bottom(),
                            height,
                            "a full viewport returns down: {context}"
                        );
                    }
                }
                let rows = text_rows(terminal.backend().buffer());
                assert!(
                    rows[..usize::from(area.y)]
                        .iter()
                        .all(|row| row.is_empty() || row == "settled history"),
                    "stale transient rows: {context}: {rows:?}"
                );
                // Visible history sits directly above the viewport, with no blank rows between.
                if let Some(row) = rows[..usize::from(area.y)]
                    .iter()
                    .position(|row| row == "settled history")
                {
                    assert_eq!(row + 1, usize::from(area.y), "{context}: {rows:?}");
                }
                let all_rows = text_rows(terminal.backend().scrollback())
                    .into_iter()
                    .chain(rows)
                    .collect::<Vec<_>>();
                assert_eq!(
                    all_rows
                        .iter()
                        .filter(|row| row.as_str() == "settled history")
                        .count(),
                    1,
                    "history is kept once, never duplicated: {context}"
                );
                assert!(
                    text_rows(terminal.backend().scrollback())
                        .iter()
                        .all(|row| row.is_empty() || row == "settled history"),
                    "{context}"
                );
                if needed == 5 {
                    assert_reflow_keeps_adjacent_history(&mut terminal, height, &context);
                }
            }
        }
    }

    // Rows are rebuilt by hand because TestBackend::resize would reshape its flat cells.
    fn assert_reflow_keeps_adjacent_history(
        terminal: &mut Terminal<TestBackend>,
        height: u16,
        context: &str,
    ) {
        use ratatui::widgets::{Paragraph, Widget};
        let before = terminal.get_frame().area();
        let mut narrower = TestBackend::new(60, height);
        for (y, row) in text_rows(terminal.backend().buffer())
            .into_iter()
            .enumerate()
        {
            let area = Rect::new(0, u16::try_from(y).unwrap(), 60, 1);
            let mut line = Buffer::empty(area);
            Paragraph::new(row).render(area, &mut line);
            narrower
                .draw(Buffer::empty(area).diff(&line).into_iter())
                .unwrap();
        }
        narrower.set_cursor_position((2, before.y)).unwrap();
        *terminal.backend_mut() = narrower;
        resize_classic(terminal, 0, before.height, true, || TestBackend::new(1, 1)).unwrap();
        let area = terminal.get_frame().area();
        assert_eq!(
            area,
            Rect::new(0, before.y, 60, before.height),
            "reflow keeps the top: {context}"
        );
        let rows = text_rows(terminal.backend().buffer());
        assert_eq!(
            rows[usize::from(area.y) - 1],
            "settled history",
            "reflow keeps history directly above: {context}: {rows:?}"
        );
        assert!(
            rows[usize::from(area.y)..].iter().all(String::is_empty),
            "reflow clears the old viewport rows: {context}: {rows:?}"
        );
    }

    #[derive(Clone, Copy)]
    enum ResizeFailure {
        Construction,
        Clear,
    }

    struct FailingResizeBackend {
        inner: TestBackend,
        shown: Rc<StateCell<bool>>,
        failure: Option<ResizeFailure>,
    }

    impl Backend for FailingResizeBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.draw(content).unwrap();
            Ok(())
        }

        fn append_lines(&mut self, count: u16) -> io::Result<()> {
            self.inner.append_lines(count).unwrap();
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.shown.set(false);
            self.inner.hide_cursor().unwrap();
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.shown.set(true);
            self.inner.show_cursor().unwrap();
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.inner.get_cursor_position().unwrap())
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.inner.set_cursor_position(position).unwrap();
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            self.clear_region(ClearType::All)
        }

        fn clear_region(&mut self, region: ClearType) -> io::Result<()> {
            if self.shown.get() && matches!(self.failure, Some(ResizeFailure::Clear)) {
                return Err(io::Error::other("resize clear"));
            }
            self.inner.clear_region(region).unwrap();
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            if self.shown.get() && matches!(self.failure, Some(ResizeFailure::Construction)) {
                return Err(io::Error::other("resize construction"));
            }
            Ok(self.inner.size().unwrap())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(self.inner.window_size().unwrap())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush().unwrap();
            Ok(())
        }

        fn scroll_region_up(&mut self, region: std::ops::Range<u16>, count: u16) -> io::Result<()> {
            self.inner.scroll_region_up(region, count).unwrap();
            Ok(())
        }

        fn scroll_region_down(
            &mut self,
            region: std::ops::Range<u16>,
            count: u16,
        ) -> io::Result<()> {
            self.inner.scroll_region_down(region, count).unwrap();
            Ok(())
        }
    }

    #[test]
    fn classic_resize_failure_retains_detached_backend_and_previous_viewport() {
        for (failure, message) in [
            (ResizeFailure::Construction, "resize construction"),
            (ResizeFailure::Clear, "resize clear"),
        ] {
            let shown = Rc::new(StateCell::new(false));
            let mut terminal = Terminal::with_options(
                FailingResizeBackend {
                    inner: TestBackend::new(80, 24),
                    shown: Rc::clone(&shown),
                    failure: None,
                },
                TerminalOptions {
                    viewport: Viewport::Inline(12),
                },
            )
            .unwrap();
            terminal.hide_cursor().unwrap();
            let viewport = terminal.get_frame().area();
            terminal.backend_mut().inner.resize(40, 24);
            terminal.backend_mut().failure = Some(failure);
            let error = resize_classic(&mut terminal, 0, 12, false, || FailingResizeBackend {
                inner: TestBackend::new(1, 1),
                shown: Rc::new(StateCell::new(false)),
                failure: None,
            })
            .unwrap_err();
            assert_eq!(error.to_string(), message);
            assert!(shown.get(), "original backend cursor shown before swap");
            assert_eq!(terminal.backend().size().unwrap(), Size::new(1, 1));
            assert_eq!(terminal.get_frame().area(), viewport);
            terminal.show_cursor().unwrap();
            assert!(
                shown.get(),
                "detached cleanup preserves the original cursor"
            );
        }
    }

    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn strip_csi(bytes: &[u8]) -> String {
        let mut out = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
                index += 2;
                while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
                index += 1;
            } else {
                out.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn short_pane_at_screen_top_scrolls_rows_into_scrollback() {
        let mut terminal = Terminal::with_options(
            TestBackend::new(12, 8),
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("terminal");
        insert_rows_scrolling_screen(&mut terminal, 8, rows_with_separator()).expect("insert");
        assert_eq!(
            text_rows(terminal.backend().scrollback()),
            ["alpha", "beta", ""]
        );
        assert!(
            text_rows(terminal.backend().buffer())
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn classic_inserts_container_rows_once_with_the_shared_layout() {
        use super::super::history::{MessageKind, display_rows};
        use super::super::view::{Palette, native_line};
        let source = "> - alpha beta gamma delta\n>\n>   ~~~\n>   code\n>   ~~~\n\n-";
        for width in [16, 40, 50, 80, 120] {
            let rows = display_rows(MessageKind::Assistant, source, usize::from(width), None);
            let mut terminal = Terminal::with_options(
                TestBackend::new(width, 8),
                TerminalOptions {
                    viewport: Viewport::Inline(8),
                },
            )
            .unwrap();
            let lines = rows
                .iter()
                .map(|row| native_line(row, width, Palette { color: false }));
            insert_rows_scrolling_screen(&mut terminal, 8, lines).unwrap();
            let mut expected = rows
                .iter()
                .map(|row| row.text.trim_end().to_owned())
                .collect::<Vec<_>>();
            expected[0].insert_str(0, "● ");
            let actual = text_rows(terminal.backend().scrollback());
            assert_eq!(actual, expected);
            let scrollback = actual.join("\n");
            assert_eq!(scrollback.matches("│ • alpha").count(), 1);
            assert_eq!(scrollback.matches("code").count(), 1);
        }
    }

    #[test]
    fn short_pane_below_row_zero_inserts_above_viewport_and_erases_it() {
        let mut backend = TestBackend::new(12, 9);
        backend.set_cursor_position((0, 1)).expect("cursor");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("terminal");
        assert_eq!(terminal.get_frame().area().y, 1);
        insert_rows_scrolling_screen(&mut terminal, 9, rows_with_separator()).expect("insert");
        assert_eq!(
            text_rows(terminal.backend().scrollback()),
            ["", "alpha", "beta"]
        );
        assert!(
            text_rows(terminal.backend().buffer())
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn wide_cells_emit_no_padding_shift() {
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(Sink(Rc::clone(&sink))),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 8, 2)),
            },
        )
        .expect("terminal");
        let lines = [Line::from("コンab"), Line::default()].into_iter();
        insert_rows_scrolling_screen(&mut terminal, 3, lines).expect("insert");
        let text = strip_csi(&sink.borrow());
        assert!(text.contains("コンab"), "{text:?}");
    }
}
