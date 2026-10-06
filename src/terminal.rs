use crate::presentation::{
    AgentInspectorModel, DraftAction, PresentationModel, image_message_lines, render_answer,
    user_message_lines,
};
use crate::provider::{Effort, ModelEntry};
use crate::session::{RunStatus, SessionListItem, SessionView};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
#[cfg(unix)]
use rustix::process::{Signal, getpid, kill_process};
#[cfg(unix)]
use rustix::termios::{
    LocalModes, OptionalActions, QueueSelector, Termios, tcflush, tcgetattr, tcsetattr,
};
use std::io::{self, IsTerminal, Stderr, Write};

mod agents;
mod clipboard;
mod commands;
mod composer;
mod history;
mod input;
mod linear;
mod view;
use agents::{AgentPanel, PanelUpdate};
use clipboard::{ClipboardContent, ClipboardRead};
use commands::help_len;
pub use commands::{
    CommandAvailability, CommandParseError, InteractiveCommand, Submission, command_completions,
    parse_submission,
};
pub use composer::{CompletionLayout, Composer, ComposerEdit};
use history::History;
use input::TerminalReader;
pub use input::{ShutdownSignal, TerminalInput};
use linear::LinearState;
use view::{
    Palette, agent_detail_rows, agent_picker_index, bottom_area, completion_height,
    composer_height, draw_agent_inspector_frame, draw_frame_with_history, draw_help_frame,
    draw_model_catalog_frame, draw_quick_actions_frame, draw_session_picker_frame,
    draw_setup_choices_frame, draw_setup_frame, draw_setup_text_frame, draw_setup_warning_frame,
    history_top_padding, screen_terminal, session_picker_index,
};

const MIN_INTERACTIVE_HEIGHT: u16 = 8;

#[derive(Clone, Copy)]
enum SnapshotState {
    Session,
    Busy,
    Preparing,
}

pub struct ModelPicker<'a> {
    pub items: &'a [ModelEntry],
    pub efforts: &'a [Option<Effort>],
    pub selected: usize,
    pub exact_custom: bool,
    pub filter: &'a str,
    pub invalid: bool,
    pub catalog_state: ModelCatalogState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelCatalogState {
    #[default]
    Current,
    Refreshing,
    Updated,
    RefreshFailed,
}

impl ModelCatalogState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Current => "",
            Self::Refreshing => "refreshing",
            Self::Updated => "refreshed",
            Self::RefreshFailed => "refresh failed; cached models",
        }
    }
}

fn transient_notice(notice: &str) -> bool {
    notice.starts_with("Draft retained: ")
        || matches!(
            notice,
            "Model catalog closed"
                | "Model catalog cancelled"
                | "Agent inspection closed"
                | "Session selection closed"
                | "Setup closed"
                | "Terminal resumed"
                | "Preparing request... Ctrl+C cancels"
                | "Stopping request..."
                | "Cancelling Run..."
                | "Compacting Session... Ctrl+C interrupts"
                | "Automatically compacting Session... Ctrl+C interrupts"
                | "Draft retained; press Enter after compaction to submit"
                | "Draft retained; press Enter after this Run to submit"
                | "Loading model catalog; Ctrl+C cancels"
                | "Reading clipboard... draft retained"
                | "Clipboard loading; press Enter after paste to submit"
                | "Clipboard read cancelled; draft retained"
        )
}

fn model_catalog_status(item: &ModelEntry, exact_custom: bool) -> String {
    if exact_custom {
        let efforts = item
            .efforts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if efforts.is_empty() {
            "exact profile; effort provider default".to_owned()
        } else {
            format!(
                "exact profile; effort provider default, {}",
                efforts.join(", ")
            )
        }
    } else if item.runnable {
        let efforts = item
            .efforts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("selectable; effort {efforts}")
    } else {
        "availability only; compatibility unknown".to_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("attached mode requires terminal stdin and stderr; use arany exec for pipes")]
    Ineligible,
    #[error("terminal is too short for inline mode")]
    TooShort,
    #[error("terminal input closed unexpectedly")]
    InputClosed,
    #[error("terminal rendering failed")]
    Io(#[from] io::Error),
}

struct RawMode {
    active: bool,
}

impl RawMode {
    fn acquire() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self { active: true })
    }

    fn release(&mut self) -> io::Result<()> {
        if self.active {
            disable_raw_mode()?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(unix)]
struct SecretEcho {
    original: Termios,
    active: bool,
}

#[cfg(unix)]
impl SecretEcho {
    fn acquire() -> io::Result<Self> {
        let original = tcgetattr(io::stdin()).map_err(io::Error::from)?;
        let mut hidden = original.clone();
        hidden.local_modes.remove(LocalModes::ECHO);
        tcsetattr(io::stdin(), OptionalActions::Now, &hidden).map_err(io::Error::from)?;
        Ok(Self {
            original,
            active: true,
        })
    }

    fn release(&mut self) -> io::Result<()> {
        if self.active {
            tcsetattr(io::stdin(), OptionalActions::Now, &self.original)
                .map_err(io::Error::from)?;
            self.active = false;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for SecretEcho {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

struct MouseCapture {
    active: bool,
}

impl MouseCapture {
    fn acquire() -> io::Result<Self> {
        if let Err(error) = execute!(io::stderr(), EnableMouseCapture) {
            let _ = execute!(io::stderr(), DisableMouseCapture);
            return Err(error);
        }
        Ok(Self { active: true })
    }

    fn release(&mut self) -> io::Result<()> {
        if self.active {
            execute!(io::stderr(), DisableMouseCapture)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(unix)]
struct BracketedPaste {
    active: bool,
}

#[cfg(unix)]
impl BracketedPaste {
    fn acquire() -> io::Result<Self> {
        let guard = Self { active: true };
        let mut output = io::stderr();
        output.write_all(b"\x1b[?2004h")?;
        output.flush()?;
        Ok(guard)
    }

    fn release(&mut self) -> io::Result<()> {
        if self.active {
            let mut output = io::stderr();
            output.write_all(b"\x1b[?2004l")?;
            output.flush()?;
            self.active = false;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for BracketedPaste {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

pub struct AttachedTerminal {
    terminal: Option<Terminal<CrosstermBackend<Stderr>>>,
    history: History,
    input: Option<TerminalReader>,
    _raw_mode: Option<RawMode>,
    mouse_capture: Option<MouseCapture>,
    #[cfg(unix)]
    bracketed_paste: Option<BracketedPaste>,
    session_picker_area: Option<Rect>,
    agent_picker_area: Option<Rect>,
    linear: Option<LinearState>,
    #[cfg(unix)]
    secret_echo: Option<SecretEcho>,
    agent_panel: Option<AgentPanel>,
    help_selected: Option<usize>,
    clipboard: Option<ClipboardRead>,
    clipboard_paste: Option<Result<ClipboardContent, &'static str>>,
    clipboard_preview: bool,
    requested_screen_reader: bool,
    color_enabled: bool,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    suspend: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl AttachedTerminal {
    pub fn draw_workspace_permissions(
        &mut self,
        id: uuid::Uuid,
        workspace: &std::path::Path,
        page: usize,
        trust: bool,
    ) -> Result<bool, TerminalError> {
        self.cancel_clipboard_paste();
        let lines = crate::presentation::workspace_permission_lines(workspace);
        if let Some(linear) = &mut self.linear {
            linear
                .draw_permission_prompt(
                    id,
                    &lines,
                    "1. Continue read only\n2. Trust this folder\nType trust or read only; empty Enter selects read only. Ctrl+C exits:",
                )
                .map_err(TerminalError::Io)?;
            return Ok(false);
        }
        let palette = self.palette();
        let mut more = true;
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                if frame.area().width >= 16 && frame.area().height >= 8 {
                    more = view::draw_permission_frame(
                        frame,
                        &lines,
                        if frame.area().width < 24 {
                            ("Read only", "Trust folder")
                        } else {
                            ("Continue read only", "Trust this folder")
                        },
                        page,
                        trust,
                        if frame.area().width < 24 {
                            "↑↓ Enter ^C exit"
                        } else if frame.area().width < 50 {
                            "↑↓ Enter · Ctrl+C exits"
                        } else {
                            "↑↓ choose · Enter confirm · Ctrl+C exit"
                        },
                        palette,
                    );
                } else {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new("Resize to review; Ctrl+C exits"),
                        frame.area(),
                    );
                }
            })?;
        Ok(more)
    }
    pub fn draw_tool_approval(
        &mut self,
        intent: &crate::tools::EffectIntent,
        page: usize,
        allow: bool,
    ) -> Result<bool, TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            linear
                .draw_tool_approval(intent)
                .map_err(TerminalError::Io)?;
            return Ok(false);
        }
        let palette = self.palette();
        let mut more = true;
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                if frame.area().width >= 16 && frame.area().height >= 8 {
                    more = view::draw_permission_frame(
                        frame,
                        &crate::presentation::tool_approval_lines(intent),
                        ("Deny", "Allow once"),
                        page,
                        allow,
                        if frame.area().width < 24 {
                            "↑↓ Enter Esc"
                        } else if frame.area().width < 50 {
                            "↑↓ Enter · Esc denies"
                        } else {
                            "↑↓ choose · Enter confirm · Esc denies"
                        },
                        palette,
                    );
                } else {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new("Resize to review; Esc denies"),
                        frame.area(),
                    );
                }
            })?;
        Ok(more)
    }
    pub fn acquire() -> Result<Self, TerminalError> {
        Self::acquire_with_preference(false)
    }

    pub fn acquire_with_preference(screen_reader: bool) -> Result<Self, TerminalError> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(TerminalError::Ineligible);
        }
        #[cfg(unix)]
        let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        #[cfg(unix)]
        let suspend = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
            Signal::TSTP.as_raw(),
        ))?;
        #[cfg(unix)]
        let hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        #[cfg(unix)]
        let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let height = size().ok().map(|(_, height)| height);
        let mode_label = if screen_reader {
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
        let raw_mode = mode_label.is_none().then(RawMode::acquire).transpose()?;
        let terminal = if mode_label.is_none() {
            Some(screen_terminal()?)
        } else {
            None
        };
        let input = TerminalReader::start(mode_label.is_some())?;
        #[cfg(unix)]
        let bracketed_paste = mode_label
            .is_none()
            .then(BracketedPaste::acquire)
            .transpose()?;
        Ok(Self {
            terminal,
            history: History::default(),
            input: Some(input),
            _raw_mode: raw_mode,
            mouse_capture: None,
            #[cfg(unix)]
            bracketed_paste,
            session_picker_area: None,
            agent_picker_area: None,
            linear: mode_label.map(LinearState::new),
            #[cfg(unix)]
            secret_echo: None,
            agent_panel: None,
            help_selected: None,
            clipboard: None,
            clipboard_paste: None,
            clipboard_preview: false,
            requested_screen_reader: screen_reader,
            color_enabled: std::env::var_os("NO_COLOR").is_none(),
            #[cfg(unix)]
            interrupt,
            #[cfg(unix)]
            suspend,
            #[cfg(unix)]
            hangup,
            #[cfg(unix)]
            terminate,
        })
    }

    pub async fn next_input(&mut self) -> Result<TerminalInput, TerminalError> {
        self.clipboard_paste = None;
        let clipboard_ready = self.input.as_ref().is_some_and(TerminalReader::paste_ready);
        #[cfg(unix)]
        let input = tokio::select! {
            input = self.input.as_mut().expect("active terminal input").recv() => input?,
            signal = self.interrupt.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                TerminalInput::Interrupt
            },
            signal = self.suspend.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                TerminalInput::Suspend
            },
            signal = self.hangup.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                TerminalInput::Shutdown(ShutdownSignal::Hangup)
            },
            signal = self.terminate.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                TerminalInput::Shutdown(ShutdownSignal::Terminate)
            },
            paste = async {
                match self.clipboard.as_mut() {
                    Some(read) => read.recv().await,
                    None => std::future::pending().await,
                }
            }, if clipboard_ready => {
                self.clipboard.take();
                self.clipboard_paste = Some(paste);
                TerminalInput::Paste
            },
        };
        #[cfg(not(unix))]
        let input = tokio::select! {
            input = self.input.as_mut().expect("active terminal input").recv() => input?,
            paste = async {
                match self.clipboard.as_mut() {
                    Some(read) => read.recv().await,
                    None => std::future::pending().await,
                }
            }, if clipboard_ready => {
                self.clipboard.take();
                self.clipboard_paste = Some(paste);
                TerminalInput::Paste
            },
        };
        if let Some(linear) = &mut self.linear {
            match input {
                TerminalInput::Submit => {
                    linear.prompt_needed = true;
                    linear.line_open = false;
                }
                TerminalInput::Interrupt
                | TerminalInput::EndOfInput
                | TerminalInput::Suspend
                | TerminalInput::Shutdown(_)
                | TerminalInput::LineRejected
                | TerminalInput::LineContinued
                | TerminalInput::Paste => {
                    linear.prompt_needed = true;
                    linear.newline_pending = linear.line_open;
                }
                _ => {}
            }
        }
        Ok(input)
    }

    #[must_use]
    pub fn is_linear(&self) -> bool {
        self.linear.is_some()
    }

    /// Whether an accepted canonical segment has finished delivering its input events.
    #[must_use]
    pub fn input_boundary_ready(&self) -> bool {
        self.input.as_ref().is_some_and(TerminalReader::paste_ready)
    }

    pub fn disable_color(&mut self) {
        self.color_enabled = false;
    }

    fn palette(&self) -> Palette {
        Palette {
            color: self.color_enabled,
        }
    }

    pub fn handle_history_input(&mut self, input: TerminalInput, view: &SessionView) -> bool {
        let handled = !self.is_linear() && self.history.handle(input, view);
        if handled && input == TerminalInput::HistoryFind {
            self.cancel_clipboard_paste();
        }
        handled
    }

    pub fn discard_draft_input(&mut self) {
        self.input
            .as_mut()
            .expect("active terminal input")
            .reset_draft();
    }

    pub fn take_paste(&mut self) -> Result<String, &'static str> {
        if let Some(paste) = self.clipboard_paste.take() {
            return match paste? {
                ClipboardContent::Text(text) => Ok(text),
                ClipboardContent::Image(_) => Err("image paste is only available in chat"),
            };
        }
        self.input
            .as_mut()
            .and_then(TerminalReader::take_paste)
            .ok_or("paste is unavailable")?
    }

    pub fn request_clipboard_paste(&mut self) -> Result<(), &'static str> {
        if self.input.is_none() {
            return Err("terminal input is not active");
        }
        if self.clipboard.is_some() {
            return Err("clipboard read already in progress");
        }
        self.clipboard = Some(ClipboardRead::start()?);
        Ok(())
    }

    pub fn clipboard_loading(&self) -> bool {
        self.clipboard.is_some()
    }

    pub fn cancel_clipboard_paste(&mut self) {
        self.clipboard.take();
        self.clipboard_paste = None;
        self.clipboard_preview = false;
    }

    pub fn paste_into(&mut self, composer: &mut Composer) -> Result<ComposerEdit, &'static str> {
        if let Some(paste) = self.clipboard_paste.take() {
            match paste? {
                ClipboardContent::Image(image) => {
                    composer.attach_image(image)?;
                    self.clipboard_preview = self.is_linear();
                    return Ok(ComposerEdit::Changed);
                }
                ClipboardContent::Text(text) => {
                    self.clipboard_paste = Some(Ok(ClipboardContent::Text(text)));
                }
            }
        }
        let text = self.take_paste()?;
        match composer.insert_paste(&text)? {
            ComposerEdit::AtCapacity => Err("paste is too large; paste a smaller section"),
            edit => {
                self.input
                    .as_mut()
                    .expect("active terminal input")
                    .restore_draft_bytes(composer.text().len())
                    .expect("admitted Composer draft length");
                self.clipboard_preview = self.is_linear() && edit == ComposerEdit::Changed;
                Ok(edit)
            }
        }
    }

    pub fn restore_draft_input(&mut self, bytes: usize) -> Result<(), TerminalError> {
        self.input
            .as_mut()
            .expect("active terminal input")
            .restore_draft_bytes(bytes)
            .map_err(TerminalError::Io)
    }

    pub fn restore(&mut self) -> Result<(), TerminalError> {
        self.release().map_err(TerminalError::Io)
    }

    /// Reacquires a restored terminal while retaining queued Unix signals.
    pub fn reacquire_after_output(&mut self) -> Result<(), TerminalError> {
        if self.input.is_some() {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal is still active",
            )));
        }
        let mut replacement = Self::acquire_with_preference(self.requested_screen_reader)?;
        replacement.color_enabled = self.color_enabled;
        self.replace_released(replacement);
        Ok(())
    }

    #[cfg(unix)]
    pub fn suspend_and_resume(&mut self, draft_bytes: usize) -> Result<(), TerminalError> {
        let screen_reader = self.requested_screen_reader;
        let secret_input = self.secret_echo.is_some();
        let mut panel = self.agent_panel.take();
        let help_selected = self.help_selected.take();
        if let Some(panel) = &mut panel {
            panel.last_linear = None;
        }
        self.release()?;
        kill_process(getpid(), Signal::STOP).map_err(io::Error::from)?;
        let mut resumed = Self::acquire_with_preference(screen_reader)?;
        resumed.color_enabled = self.color_enabled;
        if secret_input {
            resumed.begin_secret_input()?;
        }
        resumed.agent_panel = panel;
        resumed.help_selected = help_selected;
        resumed
            .input
            .as_mut()
            .expect("active terminal input")
            .restore_draft_bytes(draft_bytes)?;
        self.replace_released(resumed);
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn suspend_and_resume(&mut self, _draft_bytes: usize) -> Result<(), TerminalError> {
        Err(TerminalError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal suspension is unavailable",
        )))
    }

    pub fn draw(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.draw_snapshot(view, composer, notice, SnapshotState::Session, None)
    }

    fn draw_snapshot(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
        state: SnapshotState,
        picker: Option<&ModelPicker<'_>>,
    ) -> Result<(), TerminalError> {
        if let Some(selected) = self.help_selected {
            return self.draw_help(selected);
        }
        if self.agent_panel.is_some() {
            return self.draw_agents(view);
        }
        if let Some(linear) = &mut self.linear {
            if std::mem::take(&mut self.clipboard_preview) {
                linear
                    .write_clipboard_draft(&mut io::stderr(), composer)
                    .map_err(TerminalError::Io)?;
            }
            return linear
                .draw(view, notice, composer.approval_mode())
                .map_err(TerminalError::Io);
        }
        let notice = notice.or_else(|| {
            self.clipboard
                .as_ref()
                .map(|_| "Reading clipboard... draft retained")
        });
        let area = self.terminal.as_mut().expect("inline terminal").size()?;
        let mut model = match state {
            SnapshotState::Preparing => PresentationModel::preparing(view, area.width),
            _ => PresentationModel::from_session(view, area.width),
        };
        if matches!(state, SnapshotState::Busy) {
            model.draft_action = DraftAction::Retain;
            model.status_line = "Busy · Ctrl+C interrupts".to_owned();
        }
        if self.clipboard.is_some() {
            model.draft_action = DraftAction::Retain;
        }
        if let Some(mode) = composer.approval_mode() {
            let permissions = if area.width >= 64 {
                format!("{} · Shift+Tab", mode.label())
            } else {
                mode.label().to_owned()
            };
            model.status_line = crate::presentation::safe_truncate(
                &format!("{permissions} · {}", model.status_line),
                usize::from(area.width),
            );
        }
        let composer_height = composer_height(composer, area.into());
        let above_composer = area.height.saturating_sub(1 + composer_height);
        let above_composer = above_composer - completion_height(composer, above_composer);
        let activity_height =
            above_composer.min(u16::try_from(model.activity_lines.len()).unwrap_or(u16::MAX));
        let history_height = above_composer - activity_height;
        let history_height = history_height - history_top_padding(history_height);
        self.history
            .sync(view, usize::from(area.width), usize::from(history_height));
        if let Some(notice) = notice.filter(|notice| !transient_notice(notice)) {
            self.history.record_notice(notice);
        }
        let history_rows = self.history.visible(view);
        let history_status = self.history.status();
        let palette = self.palette();
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                draw_frame_with_history(
                    frame,
                    &model,
                    composer,
                    notice.filter(|notice| transient_notice(notice) || history_height == 0),
                    palette,
                    &history_rows,
                    history_status.as_deref(),
                );
                if let Some(picker) = picker {
                    draw_model_catalog_frame(frame, picker, composer, &model, palette);
                }
            })?;
        Ok(())
    }

    pub fn draw_progress(&mut self, view: &SessionView, notice: &str) -> Result<(), TerminalError> {
        self.draw_progress_with_draft(view, &Composer::default(), notice)
    }

    pub fn record_user_message(&mut self, objective: &str) -> Result<(), TerminalError> {
        if let Some(linear) = &mut self.linear {
            return linear
                .write_user_message(&mut io::stderr(), objective)
                .map_err(TerminalError::Io);
        }
        Ok(())
    }

    pub fn record_user_run(&mut self, run: &crate::session::RunView) -> Result<(), TerminalError> {
        self.record_user_message(&run.objective)?;
        if self.linear.is_some() {
            let mut writer = io::stderr();
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

    pub fn print_user_run(
        run: &crate::session::RunView,
        inline: bool,
    ) -> Result<(), TerminalError> {
        Self::print_user_message_with_heading(
            &run.objective,
            if inline { "\n› You:" } else { "You:" },
        )?;
        let mut writer = io::stderr();
        for line in image_message_lines(&run.images, 80) {
            writeln!(writer, "{line}")?;
        }
        writer.flush().map_err(TerminalError::Io)
    }

    pub fn print_assistant_run(run: &crate::session::RunView) -> Result<(), TerminalError> {
        io::stdout()
            .write_all(render_answer(run, "Arany · Answer:").as_bytes())
            .map_err(TerminalError::Io)
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
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                draw_setup_frame(
                    frame,
                    step,
                    title,
                    instruction,
                    input_chars,
                    notice,
                    palette,
                )
            })?;
        Ok(())
    }

    /// Draw a visible, non-secret setup field without taking the Session composer.
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
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                draw_setup_text_frame(frame, title, instruction, draft, notice, palette)
            })?;
        Ok(())
    }

    pub fn draw_setup_choices(
        &mut self,
        step: u8,
        title: &str,
        instruction: &str,
        choices: &[(u8, &str)],
        selected: usize,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            let names = choices
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(" or ");
            let mut directions = if instruction.is_empty() {
                format!("Type a choice name: {names}")
            } else {
                format!("{instruction}; type a choice name: {names}")
            };
            if let Some((_, name)) = choices.get(selected) {
                directions.push_str("; empty Enter selects ");
                directions.push_str(name);
            }
            return linear
                .draw_setup_choices(step, title, &directions, notice)
                .map_err(TerminalError::Io);
        }
        let palette = self.palette();
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                draw_setup_choices_frame(
                    frame,
                    title,
                    instruction,
                    choices,
                    selected,
                    notice,
                    palette,
                )
            })?;
        Ok(())
    }

    pub fn draw_setup_warning(
        &mut self,
        warning: &str,
        page: usize,
        accept_selected: bool,
        notice: Option<&str>,
    ) -> Result<bool, TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            linear
                .draw_setup_warning(warning, notice)
                .map_err(TerminalError::Io)?;
            return Ok(false);
        }
        let palette = self.palette();
        let mut has_more = false;
        let mut visible = false;
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| {
                visible = frame.area().width >= 16 && frame.area().height >= 8;
                has_more = draw_setup_warning_frame(
                    frame,
                    warning,
                    page,
                    accept_selected,
                    notice,
                    palette,
                );
            })?;
        if !visible {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ChatGPT consent needs a visible terminal with at least 16 columns and 8 rows; no sign-in started",
            )));
        }
        Ok(has_more)
    }

    pub fn begin_secret_input(&mut self) -> Result<(), TerminalError> {
        if self.linear.is_some() {
            #[cfg(unix)]
            {
                if self.secret_echo.is_none() {
                    self.secret_echo = Some(SecretEcho::acquire()?);
                }
            }
            #[cfg(not(unix))]
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "hidden input unavailable in linear mode",
            )));
        }
        Ok(())
    }

    pub fn end_secret_input(&mut self) -> Result<(), TerminalError> {
        #[cfg(unix)]
        if let Some(mut echo) = self.secret_echo.take() {
            writeln!(io::stderr())?;
            echo.release()?;
        }
        Ok(())
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
            return self.draw(view, composer, Some(notice));
        }
        self.draw_busy(view, composer, Some(notice))
    }

    pub fn draw_busy(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        self.draw_in_flight(view, composer, notice, SnapshotState::Busy)
    }

    pub fn draw_preparing(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
    ) -> Result<(), TerminalError> {
        let notice = if self.is_linear() {
            notice.or(Some("Preparing request... Ctrl+C cancels"))
        } else {
            notice
        };
        self.draw_in_flight(view, composer, notice, SnapshotState::Preparing)
    }

    fn draw_in_flight(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<&str>,
        state: SnapshotState,
    ) -> Result<(), TerminalError> {
        if let Some(selected) = self.help_selected {
            return self.draw_help(selected);
        }
        if self.agent_panel.is_some() {
            return self.draw_agents(view);
        }
        if let Some(linear) = &mut self.linear {
            if std::mem::take(&mut self.clipboard_preview) {
                linear
                    .write_clipboard_draft(&mut io::stderr(), composer)
                    .map_err(TerminalError::Io)?;
            }
            let prompt_needed = linear.prompt_needed;
            linear.prompt_needed = false;
            let result = linear.draw(view, notice, composer.approval_mode());
            linear.prompt_needed = prompt_needed;
            return result.map_err(TerminalError::Io);
        }
        self.draw_snapshot(view, composer, notice, state, None)
    }

    pub fn draw_sessions(
        &mut self,
        items: &[SessionListItem],
        selected: usize,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_sessions(items, selected)
                .map_err(TerminalError::Io);
        }
        if self.mouse_capture.is_none() {
            self.mouse_capture = Some(MouseCapture::acquire()?);
        }
        let palette = self.palette();
        let result = self
            .terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| draw_session_picker_frame(frame, items, selected, palette));
        if let Err(error) = result {
            let _ = self.close_sessions();
            return Err(TerminalError::Io(error));
        }
        self.session_picker_area = Some(bottom_area(
            self.terminal
                .as_mut()
                .expect("inline terminal")
                .get_frame()
                .area(),
            12,
        ));
        Ok(())
    }

    pub fn draw_quick_actions(
        &mut self,
        selected: usize,
        has_draft: bool,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        let palette = self.palette();
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| draw_quick_actions_frame(frame, selected, has_draft, palette))?;
        Ok(())
    }

    pub fn open_help(&mut self) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .write_help(&mut io::stderr())
                .map_err(TerminalError::Io);
        }
        self.help_selected = Some(0);
        self.draw_help(0)
    }

    #[must_use]
    pub fn help_open(&self) -> bool {
        self.help_selected.is_some()
    }

    pub fn close_help(&mut self) {
        self.help_selected = None;
    }

    pub fn help_input(&mut self, input: TerminalInput) {
        let Some(selected) = self.help_selected.as_mut() else {
            return;
        };
        match input {
            TerminalInput::Up => *selected = selected.saturating_sub(1),
            TerminalInput::Down => *selected = (*selected + 1).min(help_len() - 1),
            TerminalInput::PageUp => *selected = selected.saturating_sub(10),
            TerminalInput::PageDown => *selected = (*selected + 10).min(help_len() - 1),
            TerminalInput::Home => *selected = 0,
            TerminalInput::End => *selected = help_len() - 1,
            TerminalInput::Escape
            | TerminalInput::Submit
            | TerminalInput::Interrupt
            | TerminalInput::EndOfInput => self.close_help(),
            _ => {}
        }
    }

    fn draw_help(&mut self, selected: usize) -> Result<(), TerminalError> {
        let palette = self.palette();
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| draw_help_frame(frame, selected, palette))?;
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
        self.session_picker_area
            .and_then(|area| session_picker_index(area, column, row, selected, count))
    }

    pub fn close_sessions(&mut self) -> Result<(), TerminalError> {
        self.session_picker_area = None;
        if let Some(capture) = self.mouse_capture.as_mut() {
            capture.release()?;
        }
        self.mouse_capture = None;
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
        let area = self.terminal.as_mut().expect("inline terminal").size()?;
        self.draw_snapshot(view, composer, None, SnapshotState::Session, Some(picker))?;
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .hide_cursor()?;
        Ok(area.width >= 16 && area.height >= 8)
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

    pub fn open_agents(&mut self, view: &SessionView, locked: bool) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        self.agent_panel = Some(AgentPanel::new(locked));
        self.draw_agents(view)
    }

    #[must_use]
    pub fn agents_open(&self) -> bool {
        self.agent_panel.is_some()
    }

    pub fn close_agents(&mut self) -> Result<(), TerminalError> {
        if self.agent_panel.is_none() {
            return Ok(());
        }
        self.agent_picker_area = None;
        if let Some(capture) = self.mouse_capture.as_mut() {
            capture.release()?;
        }
        self.mouse_capture = None;
        if self.agent_panel.take().is_some() {
            self.discard_draft_input();
            if let Some(linear) = &mut self.linear {
                linear.prompt_needed = true;
            }
        }
        Ok(())
    }

    pub fn close_agents_at_run_end(&mut self) -> Result<(), TerminalError> {
        #[cfg(unix)]
        if self.agent_panel.is_some() && self.linear.is_some() {
            tcflush(io::stdin(), QueueSelector::IFlush).map_err(io::Error::from)?;
        }
        self.close_agents()
    }

    pub fn agent_input(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
    ) -> Result<(), TerminalError> {
        if matches!(
            input,
            TerminalInput::LineRejected | TerminalInput::LineContinued
        ) {
            self.discard_draft_input();
        }
        let count = AgentInspectorModel::recent_agent_count(view);
        let selected = self
            .agent_panel
            .as_ref()
            .expect("open agent panel")
            .selected;
        let target = match input {
            TerminalInput::PointerMove { column, row }
            | TerminalInput::PointerClick { column, row }
            | TerminalInput::PointerScrollUp { column, row }
            | TerminalInput::PointerScrollDown { column, row } => self
                .agent_picker_area
                .and_then(|area| agent_picker_index(area, column, row, selected, count)),
            _ => None,
        };
        let panel = self.agent_panel.as_mut().expect("open agent panel");
        let update = match input {
            TerminalInput::PointerMove { .. } | TerminalInput::PointerClick { .. } => {
                target.map_or(PanelUpdate::Unchanged, |index| panel.select(index, count))
            }
            TerminalInput::PointerScrollUp { .. } if target.is_some() => {
                panel.handle(TerminalInput::Up, count, false)
            }
            TerminalInput::PointerScrollDown { .. } if target.is_some() => {
                panel.handle(TerminalInput::Down, count, false)
            }
            TerminalInput::PointerScrollUp { .. } | TerminalInput::PointerScrollDown { .. } => {
                PanelUpdate::Unchanged
            }
            _ => panel.handle(input, count, self.linear.is_some()),
        };
        match update {
            PanelUpdate::Unchanged => Ok(()),
            PanelUpdate::Redraw => self.draw_agents(view),
            PanelUpdate::Closed => self.close_agents(),
        }
    }

    fn draw_agents(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        let result = self.draw_agents_inner(view);
        if result.is_err() {
            let _ = self.close_agents();
        }
        result
    }

    fn draw_agents_inner(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        let panel = self.agent_panel.as_ref().expect("open agent panel");
        let selected = panel.selected;
        let locked = panel.locked;
        if let Some(linear) = &mut self.linear {
            let model = AgentInspectorModel::from_session(view, selected, 120, locked);
            let panel = self.agent_panel.as_mut().expect("open agent panel");
            panel.selected = model.selected;
            if panel.last_linear != Some((view.last_sequence, model.selected)) {
                linear.write_agents(&mut io::stderr(), &model, panel.invalid_notice)?;
                panel.invalid_notice = false;
                panel.last_linear = Some((view.last_sequence, model.selected));
            }
            return Ok(());
        }
        let width = self
            .terminal
            .as_mut()
            .expect("inline terminal")
            .size()?
            .width;
        let model = AgentInspectorModel::from_session(view, selected, width, locked);
        let panel = self.agent_panel.as_mut().expect("open agent panel");
        panel.selected = model.selected;
        let (_, screen_height) = size()?;
        let height = screen_height.clamp(1, 14);
        panel.scroll = panel.scroll.min(
            model
                .lines
                .len()
                .saturating_sub(1)
                .saturating_sub(agent_detail_rows(height, model.choices.len())),
        );
        let scroll = panel.scroll;
        if !model.choices.is_empty() && self.mouse_capture.is_none() {
            self.mouse_capture = Some(MouseCapture::acquire()?);
        }
        let palette = self.palette();
        self.terminal
            .as_mut()
            .expect("inline terminal")
            .draw(|frame| draw_agent_inspector_frame(frame, &model, scroll, palette))?;
        self.agent_picker_area = Some(bottom_area(
            self.terminal
                .as_mut()
                .expect("inline terminal")
                .get_frame()
                .area(),
            14,
        ));
        Ok(())
    }

    fn release(&mut self) -> io::Result<()> {
        self.cancel_clipboard_paste();
        if let Some(mut input) = self.input.take() {
            input.stop();
        }
        let mut first_error = None;
        #[cfg(unix)]
        if let Some(mut echo) = self.secret_echo.take()
            && let Err(error) = echo.release()
        {
            first_error.get_or_insert(error);
        }
        self.session_picker_area = None;
        self.agent_picker_area = None;
        if let Some(mut capture) = self.mouse_capture.take()
            && let Err(error) = capture.release()
        {
            first_error.get_or_insert(error);
        }
        #[cfg(unix)]
        if let Some(mut paste) = self.bracketed_paste.take()
            && let Err(error) = paste.release()
        {
            first_error.get_or_insert(error);
        }
        if let Some(mut terminal) = self.terminal.take() {
            for result in [
                terminal.clear(),
                terminal.show_cursor(),
                terminal.backend_mut().flush(),
            ] {
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
            }
        }
        if self.linear.take().is_some_and(|linear| linear.line_open)
            && let Err(error) = writeln!(io::stderr())
        {
            first_error.get_or_insert(error);
        }
        if let Some(mut raw_mode) = self._raw_mode.take()
            && let Err(error) = raw_mode.release()
        {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn replace_released(&mut self, mut replacement: Self) {
        replacement.history = std::mem::take(&mut self.history);
        #[cfg(unix)]
        {
            // Preserve notifications received before the new listeners were registered.
            std::mem::swap(&mut self.interrupt, &mut replacement.interrupt);
            std::mem::swap(&mut self.suspend, &mut replacement.suspend);
            std::mem::swap(&mut self.hangup, &mut replacement.hangup);
            std::mem::swap(&mut self.terminate, &mut replacement.terminate);
        }
        *self = replacement;
    }
}

impl Drop for AttachedTerminal {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
