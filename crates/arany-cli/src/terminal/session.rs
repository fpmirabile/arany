use super::*;

impl AttachedTerminal {
    pub fn acquire_with_mode(
        screen_reader: bool,
        mode: TerminalMode,
        pointer: PointerSettings,
    ) -> Result<Self, TerminalError> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(TerminalError::Ineligible);
        }
        let signals = TerminalSignals {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            suspend: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
                Signal::TSTP.as_raw(),
            ))?,
            hangup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        };
        let height = size().ok().map(|(_, height)| height);
        let mut mode_label = if screen_reader {
            Some("screen-reader")
        } else if std::env::var("TERM").ok().as_deref() == Some("dumb") {
            Some("linear fallback (TERM=dumb)")
        } else if height.is_some_and(|height| height < MIN_INTERACTIVE_HEIGHT) {
            Some("linear fallback (terminal too short)")
        } else if height.is_none() {
            Some("linear fallback (terminal size unavailable)")
        } else {
            None
        };
        let owned = OwnedTerminal::acquire(mode, &mut mode_label)?;
        Ok(Self::from_acquired(
            owned,
            mode_label,
            mode,
            pointer,
            screen_reader,
            signals,
        ))
    }

    // `[` in the pager: leave capture and the alternate screen, then append the bounded transcript.
    pub(super) fn dump_to_scrollback(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        let text = history::scrollback_transcript(view);
        self.clear_graphics_hover();
        let mut first_error = None;
        attempt_cleanup(&mut first_error, || self.delete_graphics_placement());
        self.owned
            .leave_screen(&mut self.transcript.scrollback_dump, &mut first_error);
        if let Some(error) = first_error {
            return Err(error.into());
        }
        // Raw mode turns off output processing, so the text carries its own carriage returns.
        let mut output = io::stderr().lock();
        output.write_all(text.as_bytes())?;
        output.flush()?;
        Ok(())
    }

    pub(super) fn end_scrollback_dump(&mut self) -> Result<(), TerminalError> {
        if !self.transcript.scrollback_dump {
            return Ok(());
        }
        self.owned.alternate = Some(AlternateScreen::acquire()?);
        self.transcript.scrollback_dump = false;
        self.owned
            .acquire_keyboard(graphics_probe_allowed(self.mode))?;
        self.owned.acquire_pointer(self.mode, false)?;
        // A Fullscreen resize clears and invalidates the frame; `Terminal::clear` would query the cursor.
        let terminal = self.owned();
        let area = Rect::from((ratatui::layout::Position::ORIGIN, terminal.size()?));
        terminal.resize(area)?;
        terminal.backend_mut().set_area(area);
        Ok(())
    }

    pub fn release_for_output(&mut self) -> Result<(), TerminalError> {
        self.release().map_err(TerminalError::Io)
    }

    pub fn restore(&mut self) -> Result<(), TerminalError> {
        let mut first_error = self.release().err();
        if let Err(error) = clipboard::shutdown() {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), |error| Err(TerminalError::Io(error)))
    }

    pub fn reacquire_after_output(&mut self) -> Result<(), TerminalError> {
        self.reacquire_with(|screen_reader, mode, pointer| {
            Self::acquire_with_mode(screen_reader, mode, pointer)
        })
    }

    pub(super) fn reacquire_with(
        &mut self,
        acquire: impl FnOnce(bool, TerminalMode, PointerSettings) -> Result<Self, TerminalError>,
    ) -> Result<(), TerminalError> {
        if self.owned.active() {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal is still active",
            )));
        }
        let mut replacement = acquire(self.requested_screen_reader, self.mode, self.pointer)?;
        replacement.color_enabled = self.color_enabled;
        self.replace_released(replacement);
        Ok(())
    }

    pub fn suspend_and_resume(&mut self, input_capacity: usize) -> Result<(), TerminalError> {
        let screen_reader = self.requested_screen_reader;
        let hidden_input = self.secret_echo.is_some();
        let mut panel = self.agent_panel.take();
        let help_selected = self.panels.help_selected.take();
        let usage_panel = self.panels.usage_panel.take();
        if let Some(panel) = &mut panel {
            panel.printed = None;
        }
        let reading_panel = self.transcript.transcript_open;
        self.release()?;
        kill_process(getpid(), Signal::STOP).map_err(io::Error::from)?;
        let mut resumed = Self::acquire_with_mode(screen_reader, self.mode, self.pointer)?;
        resumed.color_enabled = self.color_enabled;
        if hidden_input {
            resumed.begin_hidden_input()?;
        }
        resumed.agent_panel = panel;
        resumed.panels.help_selected = help_selected;
        resumed.panels.usage_panel = usage_panel;
        resumed.reader().restore_input_capacity(input_capacity)?;
        self.replace_released(resumed);
        if reading_panel {
            self.transcript.transcript_open = true;
            self.transcript.history.set_details(true);
        }
        Ok(())
    }
}

pub(super) fn mouse_capture_enabled() -> bool {
    !std::env::var_os("ARANY_DISABLE_MOUSE").is_some_and(|value| value != "0")
}

fn graphics_probe_allowed(mode: TerminalMode) -> bool {
    mode == TerminalMode::Fullscreen
        && mouse_capture_enabled()
        && std::env::var_os("TMUX").is_none()
}

pub(super) fn graphics_protocol(
    kitty_supported: bool,
    sixel_cell: Option<graphics::sixel::CellPixels>,
    tmux: bool,
    term_program: Option<&str>,
    lc_terminal: Option<&str>,
) -> Option<GraphicsProtocol> {
    if kitty_supported {
        Some(GraphicsProtocol::Kitty)
    } else if !tmux && let Some(cell) = sixel_cell {
        Some(GraphicsProtocol::Sixel(cell))
    } else if !tmux
        && (matches!(term_program, Some("iTerm.app" | "WezTerm")) || lc_terminal == Some("iTerm2"))
    {
        Some(GraphicsProtocol::ITerm2)
    } else {
        None
    }
}

fn detected_graphics_protocol(input: &TerminalReader, probe: bool) -> Option<GraphicsProtocol> {
    if !probe {
        return None;
    }
    graphics_protocol(
        input.kitty_graphics_supported(),
        input.sixel_cell_pixels(),
        std::env::var_os("TMUX").is_some(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("LC_TERMINAL").ok().as_deref(),
    )
}

impl OwnedTerminal {
    pub(super) fn new() -> Self {
        Self {
            raw_mode: None,
            input: None,
            alternate: None,
            terminal: None,
            bracketed_paste: None,
            keyboard: None,
            graphics_protocol: None,
            pointer_modes: None,
        }
    }

    pub(super) fn terminal(&mut self) -> &mut Terminal<TerminalBackend> {
        self.terminal_with_context("owned terminal")
    }

    pub(super) fn terminal_with_context(
        &mut self,
        context: &'static str,
    ) -> &mut Terminal<TerminalBackend> {
        self.terminal.as_mut().expect(context)
    }

    pub(super) fn reader(&mut self) -> &mut TerminalReader {
        self.input.as_mut().expect("active terminal input")
    }

    pub(super) fn active(&self) -> bool {
        self.input.is_some()
    }

    fn stop_reader(&mut self) {
        if let Some(input) = &mut self.input {
            input.stop();
        }
    }

    pub(super) fn release(
        &mut self,
        scrollback_dump: &mut bool,
        first_error: &mut Option<io::Error>,
        after_reader: impl FnOnce(&mut Option<io::Error>),
        close_linear: impl FnOnce() -> io::Result<()>,
    ) {
        self.stop_reader();
        self.input.take();
        after_reader(first_error);
        self.release_capture(first_error);
        if let Some(mut paste) = self.bracketed_paste.take() {
            attempt_cleanup(first_error, || paste.release());
        }
        if let Some(mut terminal) = self.terminal.take() {
            // Avoid input queries after reader stop and avoid clearing primary-screen dumps.
            attempt_cleanup(first_error, || {
                if std::mem::take(scrollback_dump) {
                    Ok(())
                } else {
                    terminal.backend_mut().clear_owned()
                }
            });
            attempt_cleanup(first_error, || terminal.show_cursor());
            attempt_cleanup(first_error, || terminal.backend_mut().flush());
            // Ratatui's Drop writes to stderr if cursor restoration fails.
            terminal.backend_mut().detach();
        }
        if let Some(mut alternate) = self.alternate.take() {
            attempt_cleanup(first_error, || alternate.release());
        }
        attempt_cleanup(first_error, close_linear);
        if let Some(mut raw_mode) = self.raw_mode.take() {
            attempt_cleanup(first_error, || raw_mode.release());
        }
    }

    fn release_capture(&mut self, first_error: &mut Option<io::Error>) {
        if let Some(mut modes) = self.pointer_modes.take() {
            attempt_cleanup(first_error, || modes.release());
        }
        if let Some(mut keyboard) = self.keyboard.take() {
            attempt_cleanup(first_error, || keyboard.release());
        }
    }

    // The pager's scrollback dump keeps raw input and the reader.
    fn leave_screen(&mut self, scrollback_dump: &mut bool, first_error: &mut Option<io::Error>) {
        self.release_capture(first_error);
        *scrollback_dump = true;
        if let Some(mut alternate) = self.alternate.take() {
            attempt_cleanup(first_error, || alternate.release());
        }
        attempt_cleanup(first_error, || self.terminal().show_cursor());
    }

    fn acquire_keyboard(&mut self, graphics_probe: bool) -> io::Result<()> {
        let reader = self.input.as_ref().expect("terminal reader acquired");
        self.keyboard = keyboard_mode(reader, graphics_probe)?;
        self.graphics_protocol = detected_graphics_protocol(reader, graphics_probe);
        Ok(())
    }

    fn acquire_pointer(&mut self, mode: TerminalMode, linear: bool) -> io::Result<()> {
        self.pointer_modes = pointer_modes(
            mode,
            linear,
            mouse_capture_enabled(),
            self.graphics_protocol.is_some(),
        )?;
        Ok(())
    }
}

impl OwnedTerminal {
    pub(super) fn acquire_in_order(
        &mut self,
        mut acquire: impl FnMut(AcquisitionStep, &mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        for step in ACQUISITION_STEPS {
            acquire(step, self)?;
        }
        Ok(())
    }
}

impl Drop for OwnedTerminal {
    fn drop(&mut self) {
        self.pointer_modes.take();
        self.keyboard.take();
        self.bracketed_paste.take();
        self.terminal.take();
        self.alternate.take();
        if let Some(input) = &mut self.input {
            input.stop();
        }
        self.input.take();
        self.raw_mode.take();
    }
}

pub(super) fn classic_cursor_timeout_fallback<T>(
    stop_reader: impl FnOnce(),
    release_raw_mode: impl FnOnce() -> io::Result<()>,
    start_linear_reader: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    stop_reader();
    release_raw_mode()?;
    start_linear_reader()
}

fn acquire_visual_terminal(
    mode: TerminalMode,
    mode_label: &mut Option<&'static str>,
    resources: &mut OwnedTerminal,
) -> io::Result<Option<Terminal<TerminalBackend>>> {
    if mode_label.is_some() {
        return Ok(None);
    }
    let reader = resources.input.as_ref().expect("terminal reader acquired");
    match screen_terminal(mode, reader.cursor_query()) {
        Ok(terminal) => Ok(Some(terminal)),
        Err(error) if mode == TerminalMode::Classic && error.kind() == io::ErrorKind::TimedOut => {
            let reader = resources.input.as_mut().expect("terminal reader acquired");
            *reader = classic_cursor_timeout_fallback(
                || reader.stop(),
                || {
                    resources
                        .raw_mode
                        .take()
                        .map_or(Ok(()), |mut raw| raw.release())
                },
                || TerminalReader::start(true),
            )?;
            *mode_label = Some("linear fallback (cursor response unavailable)");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

impl OwnedTerminal {
    pub(super) fn acquire(
        mode: TerminalMode,
        mode_label: &mut Option<&'static str>,
    ) -> io::Result<Self> {
        let mut resources = Self::new();
        resources.acquire_in_order(|step, resources| {
            match step {
                AcquisitionStep::RawMode => {
                    resources.raw_mode = mode_label.is_none().then(RawMode::acquire).transpose()?;
                }
                AcquisitionStep::Reader => {
                    resources.input = Some(TerminalReader::start(mode_label.is_some())?);
                }
                AcquisitionStep::AlternateScreen => {
                    resources.alternate = (mode_label.is_none()
                        && mode == TerminalMode::Fullscreen)
                        .then(AlternateScreen::acquire)
                        .transpose()?;
                }
                AcquisitionStep::VisualTerminal => {
                    resources.terminal = acquire_visual_terminal(mode, mode_label, resources)?;
                }
                AcquisitionStep::BracketedPaste => {
                    resources.bracketed_paste = mode_label
                        .is_none()
                        .then(|| ModeGuard::acquire(BRACKETED_PASTE))
                        .transpose()?;
                }
                AcquisitionStep::KeyboardMode => {
                    resources
                        .acquire_keyboard(mode_label.is_none() && graphics_probe_allowed(mode))?;
                }
                AcquisitionStep::PointerModes => {
                    resources.acquire_pointer(mode, mode_label.is_some())?;
                }
            }
            Ok(())
        })?;
        Ok(resources)
    }
}

pub(super) fn attempt_cleanup(
    first_error: &mut Option<io::Error>,
    cleanup: impl FnOnce() -> io::Result<()>,
) {
    if let Err(error) = cleanup() {
        first_error.get_or_insert(error);
    }
}

impl AlternateScreen {
    pub(super) fn acquire() -> io::Result<Self> {
        let guard = Self {
            active: true,
            #[cfg(test)]
            test_reset: None,
        };
        execute!(io::stderr(), EnterAlternateScreen)?;
        Ok(guard)
    }

    pub(super) fn release(&mut self) -> io::Result<()> {
        if self.active {
            #[cfg(test)]
            if let Some(result) = probe_reset(self.test_reset.as_ref()) {
                result?;
                self.active = false;
                return Ok(());
            }
            execute!(io::stderr(), LeaveAlternateScreen)?;
            self.active = false;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn tracked_for_test(
        name: &'static str,
        steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        fail_next: bool,
    ) -> Self {
        Self {
            active: true,
            test_reset: Some(reset_probe(name, steps, fail_next)),
        }
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl Write for TerminalOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.0.as_mut() {
            Some(output) => output.write(bytes),
            None => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.as_mut().map_or(Ok(()), Write::flush)
    }
}

impl RawMode {
    pub(super) fn acquire() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self {
            active: true,
            #[cfg(test)]
            test_reset: None,
        })
    }

    pub(super) fn release(&mut self) -> io::Result<()> {
        if self.active {
            #[cfg(test)]
            if let Some(result) = probe_reset(self.test_reset.as_ref()) {
                result?;
                self.active = false;
                return Ok(());
            }
            disable_raw_mode()?;
            self.active = false;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn tracked_for_test(
        name: &'static str,
        steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        fail_next: bool,
    ) -> Self {
        Self {
            active: true,
            test_reset: Some(reset_probe(name, steps, fail_next)),
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl SecretEcho {
    pub(super) fn acquire() -> io::Result<Self> {
        let original = tcgetattr(io::stdin()).map_err(io::Error::from)?;
        let mut hidden = original.clone();
        hidden.local_modes.remove(LocalModes::ECHO);
        tcsetattr(io::stdin(), OptionalActions::Now, &hidden).map_err(io::Error::from)?;
        Ok(Self {
            original,
            active: true,
        })
    }

    pub(super) fn release(&mut self) -> io::Result<()> {
        if self.active {
            tcsetattr(io::stdin(), OptionalActions::Now, &self.original)
                .map_err(io::Error::from)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for SecretEcho {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(test)]
fn probe_reset(
    probe: Option<&std::rc::Rc<std::cell::RefCell<ModeResetProbe>>>,
) -> Option<io::Result<()>> {
    let mut probe = probe?.borrow_mut();
    probe.steps.borrow_mut().push(probe.name);
    Some(if std::mem::take(&mut probe.fail_next) {
        Err(io::Error::other(format!(
            "injected {} reset failure",
            probe.name
        )))
    } else {
        Ok(())
    })
}

#[cfg(test)]
fn reset_probe(
    name: &'static str,
    steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    fail_next: bool,
) -> std::rc::Rc<std::cell::RefCell<ModeResetProbe>> {
    std::rc::Rc::new(std::cell::RefCell::new(ModeResetProbe {
        name,
        steps,
        fail_next,
    }))
}

impl ModeGuard {
    pub(super) fn acquire([enable, disable]: [&'static [u8]; 2]) -> io::Result<Self> {
        let guard = Self {
            disable,
            active: true,
            #[cfg(test)]
            test_reset: None,
        };
        let mut output = io::stderr();
        Self::finish_acquire(guard, || {
            output.write_all(enable)?;
            output.flush()
        })
    }

    pub(super) fn finish_acquire(
        guard: Self,
        enable: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<Self> {
        enable()?;
        Ok(guard)
    }

    pub(super) fn release(&mut self) -> io::Result<()> {
        if self.active {
            #[cfg(test)]
            if let Some(result) = probe_reset(self.test_reset.as_ref()) {
                result?;
                self.active = false;
                return Ok(());
            }
            let mut output = io::stderr();
            output.write_all(self.disable)?;
            output.flush()?;
            self.active = false;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn tracked_for_test(
        name: &'static str,
        steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        fail_next: bool,
    ) -> Self {
        Self {
            disable: b"",
            active: true,
            test_reset: Some(reset_probe(name, steps, fail_next)),
        }
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

// Fullscreen captures the mouse globally, or enables alternate scroll when capture is off.
fn pointer_modes(
    mode: TerminalMode,
    linear: bool,
    mouse_enabled: bool,
    graphics: bool,
) -> io::Result<Option<ModeGuard>> {
    if linear || mode == TerminalMode::Classic {
        return Ok(None);
    }
    ModeGuard::acquire(if mouse_enabled {
        if graphics {
            MOUSE_CAPTURE_WITH_MOTION
        } else {
            MOUSE_CAPTURE
        }
    } else {
        ALTERNATE_SCROLL
    })
    .map(Some)
}

fn keyboard_mode(input: &TerminalReader, graphics: bool) -> io::Result<Option<ModeGuard>> {
    match input.probe_keyboard(&mut io::stderr(), graphics)? {
        input::KeyboardSupport::Disambiguated => {
            ModeGuard::acquire(KEYBOARD_DISAMBIGUATION).map(Some)
        }
        input::KeyboardSupport::Legacy => Ok(None),
    }
}
