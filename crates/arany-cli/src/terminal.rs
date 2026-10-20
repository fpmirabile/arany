use crate::presentation::{
    AgentInspectorModel, DraftAction, PresentationModel, image_message_lines, render_answer,
    user_message_lines,
};
use arany::{Effort, ModelEntry};
use arany::{RunStatus, SessionListItem, SessionView};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    },
};
use ratatui::{Terminal, backend::Backend, layout::Rect};
#[cfg(unix)]
use rustix::process::{Signal, getpid, kill_process};
#[cfg(unix)]
use rustix::termios::{
    LocalModes, OptionalActions, QueueSelector, Termios, tcflush, tcgetattr, tcsetattr,
};
use std::collections::HashSet;
use std::io::{self, IsTerminal, Stderr, Write};

mod agents;
mod backend;
mod clipboard;
mod commands;
mod composer;
mod history;
mod input;
mod linear;
mod rewind;
pub use rewind::RewindPanelError;
mod view;
use agents::{AgentPanel, PanelUpdate};
use backend::TerminalBackend;
use clipboard::{ClipboardContent, ClipboardRead};
use commands::help_len;
pub use commands::{
    CommandAvailability, CommandParseError, InteractiveCommand, Submission, command_completions,
    invoked_objective, parse_submission, skill_invocation,
};
pub use composer::{CompletionLayout, Composer, ComposerEdit};
use history::History;
use input::TerminalReader;
pub use input::{ShutdownSignal, TerminalInput};
use linear::LinearState;
use view::{
    Palette, agent_detail_lines, agent_detail_rows, agent_picker_index, bottom_area,
    completion_height, composer_height, draw_agent_inspector_frame, draw_frame_with_history,
    draw_help_frame, draw_model_catalog_frame, draw_quick_actions_frame, draw_session_picker_frame,
    draw_setup_choices_frame, draw_setup_frame, draw_setup_text_frame, draw_setup_warning_frame,
    history_top_padding, screen_terminal, session_picker_index,
};

const MIN_INTERACTIVE_HEIGHT: u16 = 8;

pub fn report_error(message: impl std::fmt::Display) {
    let _ = writeln!(io::stderr().lock(), "error: {message}");
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    Classic,
    #[default]
    Fullscreen,
}

impl TerminalMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Fullscreen => "fullscreen",
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Classic => Self::Fullscreen,
            Self::Fullscreen => Self::Classic,
        }
    }
}

struct AlternateScreen {
    active: bool,
}

impl AlternateScreen {
    fn acquire() -> io::Result<Self> {
        let guard = Self { active: true };
        execute!(io::stderr(), EnterAlternateScreen)?;
        Ok(guard)
    }

    fn release(&mut self) -> io::Result<()> {
        if self.active {
            execute!(io::stderr(), LeaveAlternateScreen)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[derive(Default)]
struct NativeTranscript {
    session: Option<arany::SessionId>,
    initial_run_count: Option<usize>,
    first_projected_run_index: Option<usize>,
    omitted_history: bool,
    objectives: HashSet<arany::RunId>,
    answers: HashSet<arany::RunId>,
    tools: HashSet<(arany::RunId, usize)>,
    last_notice: Option<(u64, String)>,
}

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

#[derive(Clone, Copy)]
pub enum TerminalNotice<'a> {
    History(&'a str),
    Transient(&'a str),
}

impl<'a> TerminalNotice<'a> {
    fn text(self) -> &'a str {
        match self {
            Self::History(text) | Self::Transient(text) => text,
        }
    }
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

fn classic_history_status(width: u16, omitted_history: bool, pinned: bool) -> &'static str {
    if omitted_history {
        if width < 32 {
            "Ctrl+T history"
        } else {
            "More history · Ctrl+T history"
        }
    } else if pinned {
        "Classic (--classic) · Ctrl+T details"
    } else {
        "Classic · Ctrl+T details · /settings"
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

struct TerminalOutput(Option<Stderr>);

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
    waiting_tool: Option<uuid::Uuid>,
    terminal: Option<Terminal<TerminalBackend>>,
    parked_terminal: Option<Terminal<TerminalBackend>>,
    mode: TerminalMode,
    classic_pinned: bool,
    alternate: Option<AlternateScreen>,
    transcript_open: bool,
    native: NativeTranscript,
    active_run: Option<arany::RunId>,
    history: History,
    classic_history: History,
    input: Option<TerminalReader>,
    _raw_mode: Option<RawMode>,
    mouse_capture: Option<MouseCapture>,
    #[cfg(unix)]
    bracketed_paste: Option<BracketedPaste>,
    session_picker_area: Option<Rect>,
    agent_picker_area: Option<Rect>,
    linear: Option<LinearState>,
    kept_tools: Option<(arany::RunId, usize)>,
    #[cfg(unix)]
    secret_echo: Option<SecretEcho>,
    agent_panel: Option<AgentPanel>,
    help_selected: Option<usize>,
    clipboard: Option<ClipboardRead>,
    clipboard_paste: Option<Result<ClipboardContent, &'static str>>,
    clipboard_preview: bool,
    shutdown_clipboard_on_drop: bool,
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

pub struct ProfileSelection<'a> {
    pub options: &'a [arany::RuntimeProfileOption],
    pub selected: &'a [bool],
    pub focused: Option<usize>,
    pub enabled: bool,
    pub discovery_error: Option<&'a str>,
}

impl AttachedTerminal {
    pub fn set_waiting_tool(&mut self, id: Option<uuid::Uuid>) {
        self.waiting_tool = id;
    }
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
                let marker = if focused_profile == Some(index) {
                    "›"
                } else {
                    " "
                };
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
                    format!("{head} · {path}{suffix}")
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
        self.draw_frame(|frame| {
            if frame.area().width >= 16 && frame.area().height >= 8 {
                progress = view::draw_permission_frame(
                    frame,
                    &lines,
                    view::PermissionFrameState {
                        choices: if configured && frame.area().width < 24 {
                            &["Use config", "Trust folder"]
                        } else if configured {
                            &["Use explicit config", "Trust this folder"]
                        } else if frame.area().width < 24 {
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
                    frame.area(),
                );
            }
        })?;
        Ok((progress.more, progress.page))
    }
    pub fn draw_tool_approval(
        &mut self,
        approval: &arany::ToolApproval,
        page: usize,
        choice: usize,
    ) -> Result<bool, TerminalError> {
        self.cancel_clipboard_paste();
        let lines = match &approval.unguarded {
            Some(prompt) => crate::presentation::unguarded_approval_lines(&approval.intent, prompt),
            None => crate::presentation::tool_approval_lines(&approval.intent),
        };
        if let Some(linear) = &mut self.linear {
            linear
                .draw_tool_approval(
                    approval.intent.id,
                    &lines,
                    approval
                        .unguarded
                        .as_ref()
                        .is_some_and(|prompt| prompt.remember),
                )
                .map_err(TerminalError::Io)?;
            return Ok(false);
        }
        let palette = self.palette();
        let mut more = true;
        self.draw_frame(|frame| {
            if frame.area().width >= 16 && frame.area().height >= 8 {
                let width = frame.area().width;
                let choices: &[&str] = match (
                    approval.unguarded.as_ref().map(|prompt| prompt.remember),
                    width,
                ) {
                    (None, _) => &["Deny", "Allow once"],
                    (Some(false), _) => &["Deny", "Allow once (unguarded)"],
                    (Some(true), 68..) => &[
                        "Deny",
                        "Allow once (unguarded)",
                        "Always allow this exact command in this project (unguarded)",
                    ],
                    (Some(true), 40..) => {
                        &["Deny", "Allow once", "Always allow exact command here"]
                    }
                    (Some(true), _) => &["Deny", "Allow once", "Always allow"],
                };
                more = view::draw_permission_frame(
                    frame,
                    &lines,
                    view::PermissionFrameState {
                        choices,
                        page,
                        focus: choice,
                        profile_start: None,
                        profile_count: 0,
                        focused_profile: None,
                        profile_selection_enabled: None,
                        footer: Some(if width < 24 {
                            "↑↓ Enter Esc"
                        } else if width < 50 {
                            "↑↓ Enter · Esc denies"
                        } else {
                            "↑↓ choose · Enter confirm · Esc denies"
                        }),
                        palette,
                        boxed: true,
                    },
                )
                .more;
            } else {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("Resize to review; Esc denies"),
                    frame.area(),
                );
            }
        })?;
        Ok(more)
    }
    pub fn acquire_with_mode(
        screen_reader: bool,
        mode: TerminalMode,
    ) -> Result<Self, TerminalError> {
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
        #[cfg(not(unix))]
        if mode == TerminalMode::Classic && !io::stdout().is_terminal() && mode_label.is_none() {
            mode_label = Some("linear fallback (console cursor unavailable)");
        }
        let mut raw_mode = mode_label.is_none().then(RawMode::acquire).transpose()?;
        let mut input = TerminalReader::start(mode_label.is_some())?;
        let alternate = (mode_label.is_none() && mode == TerminalMode::Fullscreen)
            .then(AlternateScreen::acquire)
            .transpose()?;
        let terminal = if mode_label.is_none() {
            match screen_terminal(mode, input.cursor_query()) {
                Ok(terminal) => Some(terminal),
                Err(error)
                    if mode == TerminalMode::Classic && error.kind() == io::ErrorKind::TimedOut =>
                {
                    input.stop();
                    if let Some(mut raw) = raw_mode.take() {
                        raw.release()?;
                    }
                    input = TerminalReader::start(true)?;
                    mode_label = Some("linear fallback (cursor response unavailable)");
                    None
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };
        #[cfg(unix)]
        let bracketed_paste = mode_label
            .is_none()
            .then(BracketedPaste::acquire)
            .transpose()?;
        Ok(Self {
            terminal,
            waiting_tool: None,
            parked_terminal: None,
            mode,
            classic_pinned: false,
            alternate,
            transcript_open: false,
            native: NativeTranscript::default(),
            active_run: None,
            history: History::default(),
            classic_history: History::default(),
            input: Some(input),
            _raw_mode: raw_mode,
            mouse_capture: None,
            #[cfg(unix)]
            bracketed_paste,
            session_picker_area: None,
            agent_picker_area: None,
            linear: mode_label.map(LinearState::new),
            kept_tools: None,
            #[cfg(unix)]
            secret_echo: None,
            agent_panel: None,
            help_selected: None,
            clipboard: None,
            clipboard_paste: None,
            clipboard_preview: false,
            shutdown_clipboard_on_drop: true,
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

    pub fn mode(&self) -> TerminalMode {
        self.mode
    }

    pub fn pin_classic(&mut self) {
        self.classic_pinned = true;
    }

    #[must_use]
    pub fn classic_pinned(&self) -> bool {
        self.classic_pinned
    }

    pub fn user_run_recorded(&self, id: arany::RunId) -> bool {
        self.native.objectives.contains(&id)
    }

    pub fn finish_run_display(&mut self) {
        self.active_run = None;
        self.history.set_preview(None);
        self.classic_history.set_preview(None);
    }

    pub fn set_preview(&mut self, preview: Option<arany::DisplayUpdate>) {
        if !self.is_linear() {
            self.history.set_preview(preview.clone());
            self.classic_history.set_preview(preview);
        }
    }

    fn insert_native(
        &mut self,
        kind: history::MessageKind,
        text: &str,
    ) -> Result<(), TerminalError> {
        let palette = self.palette();
        let terminal = self.terminal.as_mut().expect("owned terminal");
        let width = terminal.get_frame().area().width;
        let rows = history::display_rows(kind, text, usize::from(width));
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

    fn sync_native(&mut self, session: &SessionView) -> Result<(), TerminalError> {
        if self.mode != TerminalMode::Classic || self.transcript_open {
            return Ok(());
        }
        if self.native.session != Some(session.id) {
            let width = usize::from(
                self.terminal
                    .as_mut()
                    .expect("owned terminal")
                    .get_frame()
                    .area()
                    .width,
            );
            let projection = history::classic_projection(session, width);
            self.native = NativeTranscript {
                session: Some(session.id),
                initial_run_count: Some(session.runs.len()),
                first_projected_run_index: projection.first_run_index,
                omitted_history: projection.omitted_history,
                ..NativeTranscript::default()
            };
        }
        for (index, run) in session.runs.iter().enumerate() {
            if self
                .native
                .initial_run_count
                .is_some_and(|count| index < count)
                && self
                    .native
                    .first_projected_run_index
                    .is_none_or(|first| index < first)
            {
                continue;
            }
            if !self.native.objectives.contains(&run.id) {
                if self.native.objectives.is_empty() {
                    let terminal = self.terminal.as_mut().expect("owned terminal");
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
                )?;
                self.native.objectives.insert(run.id);
            }
            for (range, summary) in crate::presentation::settled_tool_groups(run) {
                if !self.native.tools.contains(&(run.id, range.start)) {
                    self.insert_native(history::MessageKind::Tool, &summary)?;
                    for index in range {
                        self.native.tools.insert((run.id, index));
                    }
                }
            }
            if !matches!(
                run.status,
                arany::RunStatus::Pending | arany::RunStatus::Active
            ) && run.assistant_message.is_none()
                && !self.native.answers.contains(&run.id)
                && let Some(text) = crate::presentation::display_answer(run)
            {
                self.insert_native(history::MessageKind::Incomplete, &text)?;
                self.native.answers.insert(run.id);
            }
            if self.active_run != Some(run.id)
                && let Some(answer) = &run.assistant_message
                && !self.native.answers.contains(&run.id)
            {
                self.insert_native(history::MessageKind::Assistant, answer)?;
                self.native.answers.insert(run.id);
            }
        }
        Ok(())
    }

    pub fn toggle_transcript(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .write_transcript(&mut io::stderr(), view)
                .map_err(Into::into);
        }
        if self.transcript_open {
            if self.parked_terminal.is_some() {
                let mut panel = self.terminal.take().expect("detail viewport");
                panel.backend_mut().clear_owned()?;
                panel.backend_mut().detach();
                if let Some(mut alternate) = self.alternate.take() {
                    alternate.release()?;
                }
                self.terminal = self.parked_terminal.take();
                self.transcript_open = false;
                self.prepare_frame()?;
                self.terminal.as_mut().expect("classic viewport").clear()?;
            }
            self.transcript_open = false;
        } else {
            self.open_transcript_panel()?;
        }
        let details = self.transcript_open;
        self.history_mut().set_details(details);
        Ok(())
    }

    fn open_transcript_panel(&mut self) -> Result<(), TerminalError> {
        if self.mode == TerminalMode::Classic {
            self.prepare_frame()?;
            let terminal = self.terminal.as_mut().expect("classic viewport");
            terminal.backend_mut().clear_owned()?;
            let alternate = AlternateScreen::acquire()?;
            let panel = screen_terminal(
                TerminalMode::Fullscreen,
                self.input.as_ref().and_then(TerminalReader::cursor_query),
            )?;
            self.parked_terminal = self.terminal.replace(panel);
            self.alternate = Some(alternate);
        }
        self.transcript_open = true;
        Ok(())
    }

    pub fn handle_transcript_input(
        &mut self,
        input: TerminalInput,
        view: &SessionView,
    ) -> Result<bool, TerminalError> {
        if input == TerminalInput::Escape && self.history_mut().find_open() {
            return Ok(false);
        }
        if input == TerminalInput::Transcript
            || (self.transcript_open && input == TerminalInput::Escape)
        {
            self.toggle_transcript(view)?;
            return Ok(true);
        }
        if self.mode == TerminalMode::Classic
            && !self.transcript_open
            && matches!(input, TerminalInput::PageUp | TerminalInput::HistoryFind)
        {
            self.toggle_transcript(view)?;
            self.history_mut().handle(input, view);
            return Ok(true);
        }
        Ok(false)
    }

    pub fn set_mode(
        &mut self,
        mode: TerminalMode,
        draft_bytes: usize,
    ) -> Result<(), TerminalError> {
        if self.is_linear() {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "visual modes are unavailable in linear presentation",
            )));
        }
        if self.classic_pinned || mode == self.mode {
            return Ok(());
        }
        self.release()?;
        self.mode = mode;
        self.reacquire_after_output()?;
        self.restore_draft_input(draft_bytes)
    }

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

    fn history_mut(&mut self) -> &mut History {
        if self.mode == TerminalMode::Fullscreen {
            &mut self.history
        } else {
            &mut self.classic_history
        }
    }

    pub fn handle_history_input(&mut self, input: TerminalInput, view: &SessionView) -> bool {
        let handled = !self.is_linear()
            && (self.mode == TerminalMode::Fullscreen || self.transcript_open)
            && self.history_mut().handle(input, view);
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
        if self.input.is_some() {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal is still active",
            )));
        }
        let mut replacement = Self::acquire_with_mode(self.requested_screen_reader, self.mode)?;
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
        let reading_panel = self.transcript_open;
        self.release()?;
        kill_process(getpid(), Signal::STOP).map_err(io::Error::from)?;
        let mut resumed = Self::acquire_with_mode(screen_reader, self.mode)?;
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
        if reading_panel {
            self.open_transcript_panel()?;
        }
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

    fn prepare_frame(&mut self) -> Result<(), TerminalError> {
        let terminal = self.terminal.as_mut().expect("owned terminal");
        if self.mode == TerminalMode::Classic && !self.transcript_open {
            let offset = terminal.backend().cursor_offset();
            let resized = resize_classic(terminal, offset, || {
                let mut backend = TerminalBackend::new(TerminalMode::Classic, None);
                backend.detach();
                backend
            });
            if resized.is_err() {
                let _ = terminal.show_cursor();
                terminal.backend_mut().detach();
            }
            resized?;
        } else {
            terminal.autoresize()?;
        }
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        Ok(())
    }

    fn draw_frame(
        &mut self,
        render: impl FnOnce(&mut ratatui::Frame<'_>),
    ) -> Result<(), TerminalError> {
        self.prepare_frame()?;
        let terminal = self.terminal.as_mut().expect("owned terminal");
        terminal.draw(render)?;
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        Ok(())
    }

    fn draw_snapshot(
        &mut self,
        view: &SessionView,
        composer: &Composer,
        notice: Option<TerminalNotice<'_>>,
        state: SnapshotState,
        picker: Option<&ModelPicker<'_>>,
    ) -> Result<(), TerminalError> {
        let notice = notice.or_else(|| {
            crate::presentation::tool_capacity_notice(view, self.waiting_tool)
                .map(TerminalNotice::Transient)
        });
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
                .draw(
                    view,
                    notice.map(TerminalNotice::text),
                    composer.approval_mode(),
                )
                .map_err(TerminalError::Io);
        }
        let notice = notice.or_else(|| {
            self.clipboard
                .as_ref()
                .map(|_| TerminalNotice::Transient("Reading clipboard... draft retained"))
        });
        self.prepare_frame()?;
        self.sync_native(view)?;
        if self.mode == TerminalMode::Classic
            && !self.transcript_open
            && let Some(TerminalNotice::History(text)) = notice
        {
            let text = history::bounded_notice_text(text).into_owned();
            if self
                .native
                .last_notice
                .as_ref()
                .is_none_or(|(sequence, prior)| *sequence != view.last_sequence || prior != &text)
            {
                let (kind, body) = history::notice_kind(&text);
                self.insert_native(kind, body)?;
                self.native.last_notice = Some((view.last_sequence, text));
            }
        }
        let area = self
            .terminal
            .as_mut()
            .expect("inline terminal")
            .get_frame()
            .area();
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
                &format!("{} · {permissions}", model.status_line),
                usize::from(area.width),
            );
        }
        let composer_height = composer_height(composer, area);
        let above_composer = area.height.saturating_sub(1 + composer_height);
        let above_composer = above_composer - completion_height(composer, above_composer);
        let activity_height =
            above_composer.min(u16::try_from(model.activity_lines.len()).unwrap_or(u16::MAX));
        let history_height = above_composer - activity_height;
        let history_height = history_height - history_top_padding(history_height);
        self.history_mut()
            .sync(view, usize::from(area.width), usize::from(history_height));
        if let Some(notice) = notice {
            self.history_mut().record_notice(notice);
        }
        let history_rows = if self.mode == TerminalMode::Fullscreen || self.transcript_open {
            self.history_mut().visible(view)
        } else {
            self.classic_history.live_rows(view)
        };
        let native_transcript =
            self.mode == TerminalMode::Classic && !self.transcript_open && !view.runs.is_empty();
        let history_status = if native_transcript {
            let hint = classic_history_status(
                area.width,
                self.native.omitted_history,
                self.classic_pinned,
            );
            if self.native.omitted_history {
                Some(hint.to_owned())
            } else {
                // State and model lead; the mode hint is secondary and truncation drops it first.
                model.status_line = crate::presentation::safe_truncate(
                    &format!("{} · {hint}", model.status_line),
                    usize::from(area.width),
                );
                None
            }
        } else {
            self.history_mut().status()
        };
        let palette = self.palette();
        self.draw_frame(|frame| {
            draw_frame_with_history(
                frame,
                &model,
                composer,
                notice
                    .filter(|notice| {
                        matches!(notice, TerminalNotice::Transient(_)) || history_height == 0
                    })
                    .map(TerminalNotice::text),
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

    pub fn record_tool_rows(&mut self, run: &arany::RunView) -> Result<(), TerminalError> {
        if let Some(linear) = &mut self.linear {
            linear.write_tools(&mut io::stderr(), run)?;
        }
        Ok(())
    }

    pub fn record_user_message(&mut self, objective: &str) -> Result<(), TerminalError> {
        if let Some(linear) = &mut self.linear {
            return linear
                .write_user_message(&mut io::stderr(), objective)
                .map_err(TerminalError::Io);
        }
        Ok(())
    }

    pub fn record_user_run(&mut self, run: &arany::RunView) -> Result<(), TerminalError> {
        self.active_run = Some(run.id);
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

    pub fn print_user_run(run: &arany::RunView, inline: bool) -> Result<(), TerminalError> {
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

    pub fn print_assistant_run(run: &arany::RunView) -> Result<(), TerminalError> {
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
        self.draw_frame(|frame| {
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
        self.draw_frame(|frame| {
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
        self.draw_frame(|frame| {
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
        self.draw_frame(|frame| {
            visible = frame.area().width >= 16 && frame.area().height >= 8;
            has_more = draw_setup_warning_frame(
                frame,
                title,
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
                "Confirmation needs a visible terminal with at least 16 columns and 8 rows; nothing accepted",
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
        let result =
            self.draw_frame(|frame| draw_session_picker_frame(frame, items, selected, palette));
        if let Err(error) = result {
            let _ = self.close_sessions();
            return Err(error);
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
        self.draw_frame(|frame| draw_quick_actions_frame(frame, selected, has_draft, palette))?;
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
        self.draw_frame(|frame| draw_help_frame(frame, selected, palette))?;
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
            agent_detail_lines(&model, width)
                .len()
                .saturating_sub(agent_detail_rows(height, model.choices.len())),
        );
        let scroll = panel.scroll;
        if !model.choices.is_empty() && self.mouse_capture.is_none() {
            self.mouse_capture = Some(MouseCapture::acquire()?);
        }
        let palette = self.palette();
        self.draw_frame(|frame| draw_agent_inspector_frame(frame, &model, scroll, palette))?;
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
                // Cleanup must not query input after stopping its reader.
                terminal.backend_mut().clear_owned(),
                terminal.show_cursor(),
                terminal.backend_mut().flush(),
            ] {
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
            }
            // Ratatui's Drop prints to stderr if cursor restoration fails.
            terminal.backend_mut().detach();
        }
        if let Some(mut terminal) = self.parked_terminal.take() {
            terminal.backend_mut().detach();
        }
        if let Some(mut alternate) = self.alternate.take()
            && let Err(error) = alternate.release()
        {
            first_error.get_or_insert(error);
        }
        let linear = self.linear.take();
        self.kept_tools = linear.as_ref().and_then(LinearState::written_tools);
        if linear.is_some_and(|linear| linear.line_open)
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
        replacement.classic_history = std::mem::take(&mut self.classic_history);
        replacement.native = std::mem::take(&mut self.native);
        replacement.active_run = self.active_run;
        if let Some(new) = &mut replacement.linear {
            new.keep_written_tools(self.kept_tools);
        }
        replacement.classic_pinned = self.classic_pinned;
        #[cfg(unix)]
        {
            // Preserve notifications received before the new listeners were registered.
            std::mem::swap(&mut self.interrupt, &mut replacement.interrupt);
            std::mem::swap(&mut self.suspend, &mut replacement.suspend);
            std::mem::swap(&mut self.hangup, &mut replacement.hangup);
            std::mem::swap(&mut self.terminate, &mut replacement.terminate);
        }
        // The released owner is replaced during mode changes and suspension.
        self.shutdown_clipboard_on_drop = false;
        *self = replacement;
    }
}

fn resize_classic<B: Backend>(
    terminal: &mut Terminal<B>,
    cursor_offset: u16,
    detached: impl FnOnce() -> B,
) -> Result<(), B::Error> {
    let size = terminal.size()?;
    if size.width >= terminal.get_frame().area().width {
        return terminal.autoresize();
    }
    // Ratatui's Inline shrink clears from screen row zero, including settled transcript.
    let cursor = terminal.get_cursor_position()?;
    let backend = terminal.backend_mut();
    backend.set_cursor_position((0, cursor.y.saturating_sub(cursor_offset)))?;
    backend.clear_region(ratatui::backend::ClearType::AfterCursor)?;
    backend.show_cursor()?;
    let backend = std::mem::replace(backend, detached());
    let mut resized = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(size.height.min(12)),
        },
    )?;
    resized.clear()?;
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

impl Drop for AttachedTerminal {
    fn drop(&mut self) {
        let _ = self.release();
        if self.shutdown_clipboard_on_drop {
            let _ = clipboard::shutdown();
        }
    }
}

#[cfg(test)]
mod insert_rows_tests {
    use super::{insert_rows_scrolling_screen, resize_classic};
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
                resize_classic(&mut terminal, 2, || TestBackend::new(1, 1)).unwrap();
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
        resize_classic(&mut height_shrunk, 2, || TestBackend::new(1, 1)).unwrap();
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
        resize_classic(&mut short, 2, || TestBackend::new(1, 1)).unwrap();
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
            let error = resize_classic(&mut terminal, 0, || FailingResizeBackend {
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
