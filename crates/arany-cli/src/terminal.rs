use crate::presentation::{
    AgentInspectorModel, DraftAction, MAX_AGENT_ROWS, PresentationModel, image_description,
    image_message_lines, render_answer, user_message_lines,
};
use arany::{Effort, ImageAttachment, ModelEntry};
use arany::{RunStatus, SessionListItem, SessionView};
use crossterm::{
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    },
};
use ratatui::{Terminal, backend::Backend, buffer::CellWidth, layout::Rect};
use rustix::process::{Signal, getpid, kill_process};
use rustix::termios::{
    LocalModes, OptionalActions, QueueSelector, Termios, tcflush, tcgetattr, tcsetattr,
};
use std::collections::HashSet;
use std::io::{self, IsTerminal, Stderr, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod agent_chat;
mod agents;
mod backend;
mod clipboard;
mod commands;
mod composer;
mod graphics;
pub const SIXEL_LIMITS: &[arany::limits::Limit] = graphics::sixel::LIMITS;
mod history;
mod input;
mod linear;
mod local_report;
mod markdown;
pub const MARKDOWN_LIMITS: &[arany::limits::Limit] = markdown::LIMITS;
mod rewind;
pub use rewind::RewindPanelError;
mod frame;
mod native;
mod paste;
mod pickers;
mod selection;
mod session;
mod setup;
use frame::dialog_area;
use native::{bounded_transcript, classic_keeps_top, draw_history_above, resize_classic};
use selection::{
    draw_panel_feedback, hovered_image_at, pointer_position, rendered_panel_lines,
    transition_hover, wait_panel_copy,
};
use session::{attempt_cleanup, mouse_capture_enabled};
#[cfg(test)]
use session::{classic_cursor_timeout_fallback, graphics_protocol};
pub use setup::setup_choices_need_paging;
mod view;
use agent_chat::AgentChat;
pub use agent_chat::{AgentChatFollow, AgentChatInput};
use agents::{AgentPanel, PanelUpdate};
use backend::TerminalBackend;
use clipboard::{
    COPY_TOO_LARGE, ClipboardContent, ClipboardRead, MAX_COPY_BYTES, display_bytes, display_text,
};

use commands::help_len;
pub use commands::{
    CommandAvailability, CommandParseError, InteractiveCommand, Submission, command_completions,
    parse_submission, skill_invocation, transcript_agents,
};
pub use composer::{
    CompletionLayout, Composer, ComposerDraft, ComposerEdit, MAX_DRAFT_BYTES,
    MAX_PRIMARY_DRAFT_BYTES, NewlineShortcut,
};
use history::History;
use input::TerminalReader;
pub use input::{ShutdownSignal, TerminalInput};
use linear::LinearState;
pub use view::SELECTOR_ROWS;
use view::{
    AGENT_PICKER_ROWS, AgentInspectorFrame, ModelCatalogFrame, Palette, PasteDetailFrame,
    QuickActionsFrame, SessionPickerFrame, SettingsFrame, SetupChoicesFrame, SetupFrame,
    SetupTextFrame, SetupWarningFrame, UsageFrame, agent_detail_lines, agent_detail_rows,
    agent_picker_index, bottom_area, composer_height, draw_agent_inspector_frame, draw_help_frame,
    draw_model_catalog_frame, draw_quick_actions_frame, draw_session_picker_frame,
    draw_settings_frame, draw_setup_choices_frame, draw_setup_frame, draw_setup_text_frame,
    draw_setup_warning_frame, history_top_padding, screen_terminal, session_picker_index,
    setup_choices_height,
};

struct UsagePanel {
    pages: Vec<String>,
    page: usize,
    sequence: u64,
    status: bool,
    starting_sequence: Option<u64>,
    rows: usize,
    visible: usize,
}

#[derive(Default)]
struct PanelSelection {
    enabled: bool,
    area: Option<Rect>,
    lines: Vec<String>,
    anchor: Option<(usize, usize)>,
    head: Option<(usize, usize)>,
    feedback: Option<String>,
}

struct PanelCopy {
    task: tokio::task::JoinHandle<Result<Option<String>, String>>,
    selection: String,
    request: clipboard::CopyRequest,
}

struct PanelCopyResult {
    selection: String,
    request: clipboard::CopyRequest,
    result: Result<Option<String>, String>,
}

#[derive(Default)]
struct Panels {
    dialog_armed: bool,
    frame_area: Option<Rect>,
    history_area: Option<Rect>,
    panel_selection: PanelSelection,
    panel_copy: Option<PanelCopy>,
    selection_cue: Option<String>,
    session_picker_area: Option<Rect>,
    agent_picker_area: Option<Rect>,
    help_selected: Option<usize>,
    usage_panel: Option<UsagePanel>,
}

struct Transcript {
    history: History,
    classic_history: History,
    native: NativeTranscript,
    transcript_open: bool,
    // The pager's `[` left the alternate screen and capture; Enter reacquires them.
    scrollback_dump: bool,
    // The row above the Classic viewport holds history or shell output, not rows it vacated.
    history_adjacent: bool,
    kept_tools: Option<(arany::RunId, usize)>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            history: History::default(),
            classic_history: History::default(),
            native: NativeTranscript::default(),
            transcript_open: false,
            scrollback_dump: false,
            history_adjacent: true,
            kept_tools: None,
        }
    }
}

enum InputRead {
    Input(TerminalInput),
    PanelCopy(PanelCopyResult),
    GraphicsReady,
}

const NEWLINE_SHORTCUT_GUIDANCE: &str = "\\ then Enter adds a newline. Widen for setup. Visual sessions negotiate modifier support. Terminals or tmux may lose/intercept modifiers; plain CR submits. Manual setup: send CSI-u ESC [ 13 ; M u (2 Shift, 3 Alt, 5 Ctrl); enable extended keys in tmux.";

const MIN_INTERACTIVE_HEIGHT: u16 = 8;
// Button, drag and SGR reporting; any-motion is enabled only after a graphics probe succeeds.
const MOUSE_CAPTURE: [&[u8]; 2] = [
    b"\x1b[?1000h\x1b[?1002h\x1b[?1006h",
    b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
];
const MOUSE_CAPTURE_WITH_MOTION: [&[u8]; 2] = [
    b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h",
    b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
];
const ALTERNATE_SCROLL: [&[u8]; 2] = [b"\x1b[?1007h", b"\x1b[?1003l\x1b[?1007l"];
const BRACKETED_PASTE: [&[u8]; 2] = [b"\x1b[?2004h", b"\x1b[?2004l"];
const KEYBOARD_DISAMBIGUATION: [&[u8]; 2] = [b"\x1b[>1u", b"\x1b[<u"];
const KEYBOARD_ONLY_QUERY: &[u8] = b"\x1b[?u\x1b[c";
const KEYBOARD_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[?u\x1b[c";
static SELECTION_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

pub fn report_error(message: impl std::fmt::Display) {
    let _ = writeln!(io::stderr().lock(), "{}", error_line(&message.to_string()));
}

// Errors can quote untrusted repository data such as lock paths.
pub(crate) fn error_line(message: &str) -> String {
    format!("error: {}", crate::presentation::escape_terminal(message))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    Classic,
    #[default]
    Fullscreen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerSettings {
    pub copy_on_select: bool,
}

impl Default for PointerSettings {
    fn default() -> Self {
        Self {
            copy_on_select: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcquisitionStep {
    RawMode,
    Reader,
    AlternateScreen,
    VisualTerminal,
    BracketedPaste,
    KeyboardMode,
    PointerModes,
}

const ACQUISITION_STEPS: [AcquisitionStep; 7] = [
    AcquisitionStep::RawMode,
    AcquisitionStep::Reader,
    AcquisitionStep::AlternateScreen,
    AcquisitionStep::VisualTerminal,
    AcquisitionStep::BracketedPaste,
    AcquisitionStep::KeyboardMode,
    AcquisitionStep::PointerModes,
];

struct OwnedTerminal {
    raw_mode: Option<RawMode>,
    input: Option<TerminalReader>,
    alternate: Option<AlternateScreen>,
    terminal: Option<Terminal<TerminalBackend>>,
    bracketed_paste: Option<ModeGuard>,
    keyboard: Option<ModeGuard>,
    graphics_protocol: Option<GraphicsProtocol>,
    pointer_modes: Option<ModeGuard>,
}

struct TerminalSignals {
    interrupt: tokio::signal::unix::Signal,
    suspend: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl AttachedTerminal {
    fn from_acquired(
        owned: OwnedTerminal,
        mode_label: Option<&'static str>,
        settings: AttachedSettings,
        _signals: TerminalSignals,
    ) -> Self {
        let AttachedSettings {
            mode,
            pointer,
            screen_reader,
        } = settings;
        Self {
            owned,
            newline_shortcut: NewlineShortcut::default(),
            waiting_tool: None,
            mode,
            classic_pinned: false,
            panels: Panels::default(),
            transcript: Transcript::default(),
            active_run: None,
            pointer,
            image_chip_areas: Vec::new(),
            hovered_image: None,
            pointer_position: None,
            graphics_hover_changed: false,
            graphics_placement: None,
            hovered_image_digest: None,
            sixel_preview: graphics::SixelPreview::default(),
            linear: mode_label.map(LinearState::new),
            secret_echo: None,
            agent_panel: None,
            agent_chat: None,
            child_drafts: Vec::new(),
            primary_input_capacity: crate::terminal::MAX_PRIMARY_DRAFT_BYTES,
            clipboard: None,
            clipboard_paste: None,
            deferred_input: None,
            clipboard_preview: false,
            shutdown_clipboard_on_drop: true,
            requested_screen_reader: screen_reader,
            color_enabled: std::env::var_os("NO_COLOR").is_none(),
            interrupt: _signals.interrupt,
            suspend: _signals.suspend,
            hangup: _signals.hangup,
            terminate: _signals.terminate,
        }
    }
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
    #[cfg(test)]
    test_reset: Option<std::rc::Rc<std::cell::RefCell<ModeResetProbe>>>,
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
    pub refresh_error: Option<&'a str>,
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
            "arany show"
        } else {
            "More history · arany show"
        }
    } else if pinned {
        "Classic (--classic) · Ctrl+O details"
    } else {
        "Classic · Ctrl+O details · /settings"
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
    #[cfg(test)]
    test_reset: Option<std::rc::Rc<std::cell::RefCell<ModeResetProbe>>>,
}

struct TerminalOutput(Option<Stderr>);

struct SecretEcho {
    original: Termios,
    active: bool,
}

// Owns terminal modes enabled by one write and writes their reset on release or drop.
struct ModeGuard {
    disable: &'static [u8],
    active: bool,
    #[cfg(test)]
    test_reset: Option<std::rc::Rc<std::cell::RefCell<ModeResetProbe>>>,
}

#[cfg(test)]
struct ModeResetProbe {
    name: &'static str,
    steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    fail_next: bool,
}

pub struct AttachedTerminal {
    newline_shortcut: NewlineShortcut,
    waiting_tool: Option<uuid::Uuid>,
    owned: OwnedTerminal,
    mode: TerminalMode,
    classic_pinned: bool,
    panels: Panels,
    transcript: Transcript,
    active_run: Option<arany::RunId>,
    pointer: PointerSettings,
    image_chip_areas: Vec<(usize, Rect)>,
    hovered_image: Option<usize>,
    pointer_position: Option<ratatui::layout::Position>,
    graphics_hover_changed: bool,
    graphics_placement: Option<GraphicsPlacement>,
    hovered_image_digest: Option<[u8; 32]>,
    sixel_preview: graphics::SixelPreview,
    linear: Option<LinearState>,
    secret_echo: Option<SecretEcho>,
    agent_panel: Option<AgentPanel>,
    agent_chat: Option<AgentChat>,
    child_drafts: Vec<agent_chat::ChildDraft>,
    primary_input_capacity: usize,
    clipboard: Option<ClipboardRead>,
    clipboard_paste: Option<Result<ClipboardContent, &'static str>>,
    deferred_input: Option<TerminalInput>,
    clipboard_preview: bool,
    shutdown_clipboard_on_drop: bool,
    requested_screen_reader: bool,
    color_enabled: bool,
    interrupt: tokio::signal::unix::Signal,
    suspend: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphicsProtocol {
    Kitty,
    ITerm2,
    Sixel(graphics::sixel::CellPixels),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GraphicsPlacement {
    image_index: usize,
    digest: [u8; 32],
    inner: Rect,
}

pub struct SetupChoices<'a> {
    pub step: u8,
    pub title: &'a str,
    pub instruction: &'a str,
    pub choices: &'a [(u8, &'a str)],
    pub selected: usize,
    pub notice: Option<&'a str>,
}

struct AttachedSettings {
    mode: TerminalMode,
    pointer: PointerSettings,
    screen_reader: bool,
}

pub struct TrustDialog<'a> {
    pub id: uuid::Uuid,
    pub workspace: &'a std::path::Path,
    pub profiles: ProfileSelection<'a>,
    pub page: usize,
    pub trust: bool,
    pub configured: bool,
}

pub struct ProfileSelection<'a> {
    pub options: &'a [arany::RuntimeProfileOption],
    pub selected: &'a [bool],
    pub focused: Option<usize>,
    pub enabled: bool,
    pub discovery_error: Option<&'a str>,
}

impl AttachedTerminal {
    fn owned(&mut self) -> &mut Terminal<TerminalBackend> {
        self.owned.terminal()
    }

    fn reader(&mut self) -> &mut TerminalReader {
        self.owned.reader()
    }

    pub fn set_waiting_tool(&mut self, id: Option<uuid::Uuid>) {
        self.waiting_tool = id;
    }

    fn keyboard_support(&self) -> input::KeyboardSupport {
        if self.owned.keyboard.is_some() {
            return input::KeyboardSupport::Disambiguated;
        }
        input::KeyboardSupport::Legacy
    }

    pub fn newline_shortcut_guidance(&self) -> String {
        if self.is_linear() {
            return NEWLINE_SHORTCUT_GUIDANCE.to_owned();
        }
        let status = match self.keyboard_support() {
            input::KeyboardSupport::Legacy => "Enhanced keys unavailable.",
            input::KeyboardSupport::Disambiguated => "Enhanced keys negotiated.",
        };
        format!("{status} {NEWLINE_SHORTCUT_GUIDANCE}")
    }

    pub fn set_newline_shortcut(&mut self, shortcut: NewlineShortcut) {
        self.newline_shortcut = shortcut;
    }

    pub fn defer_input(&mut self, input: TerminalInput) {
        self.deferred_input = Some(input);
    }

    pub async fn next_input(&mut self) -> Result<TerminalInput, TerminalError> {
        self.read_input(false).await
    }

    pub async fn next_chat_input(&mut self) -> Result<TerminalInput, TerminalError> {
        self.read_input(
            !self.help_open()
                && !self.transcript.transcript_open
                && !self.transcript.scrollback_dump
                && !self.agents_open(),
        )
        .await
    }

    async fn read_input(&mut self, chat: bool) -> Result<TerminalInput, TerminalError> {
        if let Some(input) = &mut self.owned.input {
            input.set_chat_input(chat, self.agent_chat.is_some(), self.primary_input_capacity);
        }
        self.clipboard_paste = None;
        if let Some(input) = self.deferred_input.take() {
            return Ok(input);
        }
        let input = match self.next_read().await? {
            InputRead::Input(input) => input,
            InputRead::GraphicsReady => return Ok(TerminalInput::PanelSelectionChanged),
            InputRead::PanelCopy(result) => {
                let mut stderr = io::stderr();
                let terminal_output = self
                    .owned
                    .input
                    .is_some()
                    .then_some(&mut stderr as &mut dyn Write);
                self.finish_panel_copy(result, terminal_output);
                return Ok(TerminalInput::PanelSelectionChanged);
            }
        };
        if chat {
            self.track_pointer_hover(input);
        }
        if let Some(result) = self
            .panel_pointer_input(input, mouse_capture_enabled())
            .await
        {
            return Ok(result);
        }
        if let Some(linear) = &mut self.linear {
            linear.note_input(input);
        }
        Ok(input)
    }

    async fn next_read(&mut self) -> Result<InputRead, TerminalError> {
        let clipboard_ready = self
            .owned
            .input
            .as_ref()
            .is_some_and(TerminalReader::paste_ready);
        Ok(tokio::select! {
            input = self.owned.reader().recv() => InputRead::Input(input?),
            signal = self.interrupt.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                InputRead::Input(TerminalInput::Interrupt)
            },
            signal = self.suspend.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                InputRead::Input(TerminalInput::Suspend)
            },
            signal = self.hangup.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                InputRead::Input(TerminalInput::Shutdown(ShutdownSignal::Hangup))
            },
            signal = self.terminate.recv() => {
                signal.ok_or(TerminalError::InputClosed)?;
                InputRead::Input(TerminalInput::Shutdown(ShutdownSignal::Terminate))
            },
            paste = async {
                match self.clipboard.as_mut() {
                    Some(read) => read.recv().await,
                    None => std::future::pending().await,
                }
            }, if clipboard_ready => {
                self.clipboard.take();
                self.clipboard_paste = Some(paste);
                InputRead::Input(TerminalInput::Paste)
            },
            () = self.sixel_preview.wait() => InputRead::GraphicsReady,
            result = wait_panel_copy(&mut self.panels.panel_copy) => {
                InputRead::PanelCopy(result)
            },
        })
    }

    // Chat pointer motion over an image chip moves the hover and resets its preview.
    fn track_pointer_hover(&mut self, input: TerminalInput) {
        if self.owned.graphics_protocol.is_some()
            && mouse_capture_enabled()
            && let Some((column, row)) = pointer_position(input)
        {
            self.pointer_position = Some(ratatui::layout::Position::new(column, row));
            let hovered = hovered_image_at(&self.image_chip_areas, column, row);
            if transition_hover(&mut self.hovered_image, hovered) {
                self.sixel_preview.clear();
                self.hovered_image_digest = None;
                self.graphics_hover_changed = true;
            } else {
                self.graphics_hover_changed = false;
            }
        }
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
        self.transcript.native.objectives.contains(&id)
    }

    pub fn finish_run_display(&mut self) {
        self.active_run = None;
        self.transcript.history.set_preview(None);
        self.transcript.classic_history.set_preview(None);
    }

    pub fn set_preview(&mut self, preview: Option<arany::DisplayUpdate>) -> bool {
        if !self.is_linear() {
            let changed = self.transcript.history.set_preview(preview.clone());
            self.transcript.classic_history.set_preview(preview);
            return changed;
        }
        false
    }

    pub fn toggle_transcript(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        self.clear_graphics_hover();
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .write_transcript(&mut io::stderr(), view)
                .map_err(Into::into);
        }
        if self.mode == TerminalMode::Classic {
            let text = bounded_transcript(view)?;
            self.prepare_frame()?;
            return self.insert_native(history::MessageKind::ToolDetails, &text, None);
        }
        self.transcript.transcript_open = !self.transcript.transcript_open;
        // Details change the rows a selection anchors to.
        self.transcript.history.clear_selection();
        self.transcript
            .history
            .set_details(self.transcript.transcript_open);
        Ok(())
    }

    #[must_use]
    pub fn ignores_page_key(&self, input: TerminalInput) -> bool {
        self.mode == TerminalMode::Classic
            && !self.is_linear()
            && matches!(input, TerminalInput::PageUp | TerminalInput::PageDown)
    }

    pub fn handle_draft_detail(&mut self, composer: &mut Composer, input: TerminalInput) -> bool {
        if self.is_linear()
            || (self.mode == TerminalMode::Fullscreen && self.transcript.transcript_open)
            || self.transcript.scrollback_dump
            || self.history_mut().find_open()
        {
            return false;
        }
        let limit = composer.paste_detail().and_then(|(_, text, _)| {
            let size = self.owned.terminal.as_mut()?.size().ok()?;
            Some(view::paste_detail_scroll_limit(
                text,
                usize::from(size.width),
                usize::from(size.height),
            ))
        });
        if let Some(limit) = limit {
            composer.clamp_detail_scroll(limit);
        }
        let handled = composer.inspect_input(input);
        if let Some(limit) = limit {
            composer.clamp_detail_scroll(limit);
        }
        if handled {
            self.cancel_clipboard_paste();
            if input == TerminalInput::Paste {
                let _ = self.take_paste();
            }
        }
        handled
    }

    pub fn handle_transcript_input(
        &mut self,
        input: TerminalInput,
        view: &SessionView,
    ) -> Result<bool, TerminalError> {
        let pager = self.mode == TerminalMode::Fullscreen
            && self.transcript.transcript_open
            && !self.is_linear();
        // The hidden composer must not change, so a paste or clipboard result is discarded.
        if input == TerminalInput::Paste && (pager || self.transcript.scrollback_dump) {
            let _ = self.take_paste();
            return Ok(true);
        }
        if self.transcript.scrollback_dump {
            return match input {
                TerminalInput::Submit => self.end_scrollback_dump().map(|()| true),
                // Ctrl+C returns to Arany so its cancellation or exit hint is visible.
                TerminalInput::Interrupt => self.end_scrollback_dump().map(|()| false),
                TerminalInput::Suspend | TerminalInput::Shutdown(_) => Ok(false),
                _ => Ok(true),
            };
        }
        if pager {
            // Escape clears a selection before it closes the pager.
            if input == TerminalInput::Escape
                && self.transcript.history.has_selection()
                && !self.transcript.history.find_open()
            {
                return Ok(false);
            }
            return match self.transcript.history.pager_input(input, view) {
                history::Pager::Close => self.toggle_transcript(view).map(|()| true),
                history::Pager::Dump => self.dump_to_scrollback(view).map(|()| true),
                history::Pager::Handled => Ok(true),
                history::Pager::Pass => Ok(false),
            };
        }
        if input == TerminalInput::Escape && self.history_mut().find_open() {
            return Ok(false);
        }
        if input == TerminalInput::Transcript {
            self.toggle_transcript(view)?;
            return Ok(true);
        }
        if self.mode == TerminalMode::Classic
            && !self.is_linear()
            && input == TerminalInput::HistoryFind
        {
            self.prepare_frame()?;
            self.insert_native(
                history::MessageKind::Notice,
                "Use your terminal's find; Ctrl+O writes details to scrollback",
                None,
            )?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn set_mode(
        &mut self,
        mode: TerminalMode,
        input_capacity: usize,
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
        self.restore_draft_input(input_capacity)
    }

    #[must_use]
    pub fn input_boundary_ready(&self) -> bool {
        self.owned
            .input
            .as_ref()
            .is_some_and(TerminalReader::paste_ready)
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
            &mut self.transcript.history
        } else {
            &mut self.transcript.classic_history
        }
    }

    // `working` marks an active Run or compaction, where Ctrl+C always reaches cancellation.
    pub async fn handle_history_input(
        &mut self,
        input: TerminalInput,
        view: &SessionView,
        composer: &Composer,
        working: bool,
    ) -> bool {
        if self.is_linear() || self.mode != TerminalMode::Fullscreen {
            return false;
        }
        self.panels.selection_cue = None;
        if working && input == TerminalInput::Interrupt {
            return false;
        }
        if let Some(handled) = self.pointer_input(input, view, composer).await {
            return handled;
        }
        let handled = self.transcript.history.handle(input, view);
        if handled && input == TerminalInput::HistoryFind {
            self.cancel_clipboard_paste();
        }
        handled
    }

    #[must_use]
    pub fn empty_interrupt_hint(&self) -> String {
        if !self.is_linear() && self.mode == TerminalMode::Fullscreen && mouse_capture_enabled() {
            format!(
                "Select text with the mouse to copy; {} for terminal selection.\nCtrl+C again exits.",
                clipboard::native_selection_gesture(&|name| std::env::var_os(name))
            )
        } else {
            "Ctrl+Shift+C copies. Ctrl+C again exits.".to_owned()
        }
    }

    pub fn set_pointer(&mut self, pointer: PointerSettings) {
        self.pointer = pointer;
    }

    pub fn discard_draft_input(&mut self) {
        self.reader().reset_draft();
    }

    pub fn take_paste(&mut self) -> Result<String, &'static str> {
        if let Some(paste) = self.clipboard_paste.take() {
            return match paste? {
                ClipboardContent::Text(text) => Ok(text),
                ClipboardContent::Image(_) => Err("image paste is only available in chat"),
            };
        }
        self.owned
            .input
            .as_mut()
            .and_then(TerminalReader::take_paste)
            .ok_or("paste is unavailable")?
    }

    pub fn request_clipboard_paste(&mut self) -> Result<(), &'static str> {
        if self.owned.input.is_none() {
            return Err("terminal input is not active");
        }
        if self.clipboard.is_some() {
            return Err("clipboard read already in progress");
        }
        self.clipboard = Some(ClipboardRead::start()?);
        Ok(())
    }

    pub async fn copy_latest_answer(&self, view: &SessionView) -> Result<String, String> {
        let request = clipboard::CopyRequest::latest_answer(view)?;
        let mut stderr = io::stderr();
        clipboard::copy(
            request,
            (self.owned.active() && !self.is_linear())
                .then_some(&mut stderr as &mut (dyn Write + Send)),
        )
        .await
    }

    pub fn clipboard_loading(&self) -> bool {
        self.clipboard.is_some()
    }

    pub fn cancel_clipboard_paste(&mut self) {
        self.clipboard.take();
        self.clipboard_paste = None;
        self.clipboard_preview = false;
    }

    pub fn restore_draft_input(&mut self, capacity: usize) -> Result<(), TerminalError> {
        self.reader()
            .restore_input_capacity(capacity)
            .map_err(TerminalError::Io)
    }

    fn release(&mut self) -> io::Result<()> {
        self.clear_graphics_hover();
        self.cancel_clipboard_paste();
        let mut first_error = None;
        attempt_cleanup(&mut first_error, || self.delete_graphics_placement());
        let owned = &mut self.owned;
        let scrollback_dump = &mut self.transcript.scrollback_dump;
        let secret_echo = &mut self.secret_echo;
        let session_picker_area = &mut self.panels.session_picker_area;
        let agent_picker_area = &mut self.panels.agent_picker_area;
        let history_area = &mut self.panels.history_area;
        let selection_cue = &mut self.panels.selection_cue;
        let linear = &mut self.linear;
        let kept_tools = &mut self.transcript.kept_tools;
        owned.release(
            scrollback_dump,
            &mut first_error,
            |first_error| {
                if let Some(mut echo) = secret_echo.take() {
                    attempt_cleanup(first_error, || echo.release());
                }
                *session_picker_area = None;
                *agent_picker_area = None;
                *history_area = None;
                *selection_cue = None;
            },
            || {
                let linear = linear.take();
                *kept_tools = linear.as_ref().and_then(LinearState::written_tools);
                if linear.is_some_and(|linear| linear.line_open) {
                    writeln!(io::stderr())
                } else {
                    Ok(())
                }
            },
        );
        first_error.map_or(Ok(()), Err)
    }

    fn replace_released(&mut self, mut replacement: Self) {
        replacement.newline_shortcut = self.newline_shortcut;
        replacement.transcript.history = std::mem::take(&mut self.transcript.history);
        replacement.transcript.classic_history =
            std::mem::take(&mut self.transcript.classic_history);
        replacement.panels.panel_selection = std::mem::take(&mut self.panels.panel_selection);
        replacement.panels.panel_copy = self.panels.panel_copy.take();
        self.sixel_preview.clear();
        replacement.sixel_preview = std::mem::take(&mut self.sixel_preview);
        replacement.panels.dialog_armed = self.panels.dialog_armed;
        replacement.agent_chat = self.agent_chat.take();
        replacement.child_drafts = std::mem::take(&mut self.child_drafts);
        replacement.primary_input_capacity = self.primary_input_capacity;
        replacement.transcript.native = std::mem::take(&mut self.transcript.native);
        replacement.active_run = self.active_run;
        if let Some(new) = &mut replacement.linear {
            new.keep_written_tools(self.transcript.kept_tools);
        }
        replacement.classic_pinned = self.classic_pinned;
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

impl Drop for AttachedTerminal {
    fn drop(&mut self) {
        let _ = self.release();
        if self.shutdown_clipboard_on_drop {
            let _ = clipboard::shutdown();
        }
    }
}

#[cfg(test)]
mod graphics_lifecycle_tests {
    use super::{
        GraphicsPlacement, GraphicsProtocol, TerminalBackend, graphics_protocol, hovered_image_at,
        transition_hover,
    };
    use ratatui::{Terminal, layout::Rect};

    #[test]
    fn graphics_detection_prefers_kitty_and_suppresses_inline_in_tmux() {
        for (kitty, tmux, program, lc_terminal, expected) in [
            (
                true,
                false,
                Some("iTerm.app"),
                None,
                Some(GraphicsProtocol::Kitty),
            ),
            (
                false,
                false,
                Some("iTerm.app"),
                None,
                Some(GraphicsProtocol::ITerm2),
            ),
            (
                false,
                false,
                Some("WezTerm"),
                None,
                Some(GraphicsProtocol::ITerm2),
            ),
            (
                false,
                false,
                None,
                Some("iTerm2"),
                Some(GraphicsProtocol::ITerm2),
            ),
            (false, true, Some("iTerm.app"), Some("iTerm2"), None),
            (false, false, Some("iTerm"), None, None),
        ] {
            assert_eq!(
                graphics_protocol(kitty, None, tmux, program, lc_terminal),
                expected
            );
        }
    }

    #[test]
    fn hover_enter_leave_request_one_frame_and_motion_inside_is_suppressed() {
        let chip = [(2, Rect::new(10, 4, 12, 1))];
        let mut hovered = None;
        let mut frames = 0;

        for next in [
            hovered_image_at(&chip, 10, 4),
            hovered_image_at(&chip, 15, 4),
            hovered_image_at(&chip, 30, 4),
        ] {
            if transition_hover(&mut hovered, next) {
                frames += 1;
            }
        }

        assert_eq!(frames, 2);
        assert_eq!(hovered, None);
    }

    #[test]
    fn stationary_pointer_is_revalidated_after_chip_geometry_moves() {
        let position = (10, 4);
        let old_chip = [(0, Rect::new(10, 4, 12, 1))];
        let new_chip = [(0, Rect::new(20, 4, 12, 1))];
        assert_eq!(hovered_image_at(&old_chip, position.0, position.1), Some(0));
        assert_eq!(hovered_image_at(&new_chip, position.0, position.1), None);
    }

    #[test]
    fn drop_after_failed_delete_does_not_expect_an_owned_terminal() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let backend = TerminalBackend::failing_graphics_writer(80, 24);
                let mut attached = super::clipboard::cleanup_tests::released_terminal(None);
                attached.owned.graphics_protocol = Some(GraphicsProtocol::Kitty);
                attached.owned.terminal = Some(Terminal::new(backend).expect("test terminal"));
                attached.graphics_placement = Some(GraphicsPlacement {
                    image_index: 0,
                    digest: [1; 32],
                    inner: Rect::new(1, 2, 3, 4),
                });
                assert!(attached.release().is_err());
                assert!(attached.graphics_placement.is_none());
                attached
                    .delete_graphics_placement()
                    .expect("repeated cleanup is idempotent");
                drop(attached);
            });
    }

    #[tokio::test]
    async fn opening_agent_chat_clears_preview_and_propagates_delete_failure() {
        let backend = TerminalBackend::failing_graphics_writer(80, 24);
        let mut attached = super::clipboard::cleanup_tests::released_terminal(None);
        attached.owned.graphics_protocol = Some(GraphicsProtocol::Kitty);
        attached.owned.terminal = Some(Terminal::new(backend).expect("test terminal"));
        attached.hovered_image = Some(0);
        attached.graphics_placement = Some(GraphicsPlacement {
            image_index: 0,
            digest: [1; 32],
            inner: Rect::new(1, 2, 3, 4),
        });
        let failure = attached
            .open_agent_chats(&crate::presentation::team_view(), None)
            .await
            .err()
            .expect("graphics delete failure");
        assert!(matches!(
            failure,
            super::TerminalError::Io(error)
                if error.to_string() == "injected graphics writer failure"
        ));
        assert!(attached.hovered_image.is_none());
        assert!(attached.graphics_placement.is_none());
        attached
            .release()
            .expect("graphics cleanup is not repeated during release");
    }
}

#[cfg(test)]
mod terminal_lifecycle_tests {
    use super::{
        AcquisitionStep, AlternateScreen, GraphicsPlacement, GraphicsProtocol, ModeGuard, RawMode,
        TerminalBackend, TerminalError, TerminalMode, classic_cursor_timeout_fallback,
    };
    use ratatui::{Terminal, TerminalOptions, Viewport, layout::Rect};
    use std::{cell::RefCell, io, rc::Rc};

    #[test]
    fn classic_cursor_timeout_stops_reader_releases_raw_and_starts_linear_in_order() {
        let steps = RefCell::new(Vec::new());
        let reader = classic_cursor_timeout_fallback(
            || steps.borrow_mut().push("stop"),
            || {
                steps.borrow_mut().push("release");
                Ok(())
            },
            || {
                steps.borrow_mut().push("start linear");
                Ok(())
            },
        );

        assert!(reader.is_ok());
        assert_eq!(*steps.borrow(), ["stop", "release", "start linear"]);
    }

    #[test]
    fn classic_cursor_timeout_does_not_restart_reader_when_raw_release_fails() {
        let steps = RefCell::new(Vec::new());
        let error = classic_cursor_timeout_fallback(
            || steps.borrow_mut().push("stop"),
            || {
                steps.borrow_mut().push("release");
                Err(io::Error::other("raw release failed"))
            },
            || {
                steps.borrow_mut().push("start linear");
                Ok(())
            },
        )
        .expect_err("raw release failure stops fallback");

        assert_eq!(error.to_string(), "raw release failed");
        assert_eq!(*steps.borrow(), ["stop", "release"]);
    }

    #[test]
    fn acquisition_failure_unwinds_every_earlier_step_in_reverse_order() {
        assert_eq!(
            super::ACQUISITION_STEPS,
            [
                AcquisitionStep::RawMode,
                AcquisitionStep::Reader,
                AcquisitionStep::AlternateScreen,
                AcquisitionStep::VisualTerminal,
                AcquisitionStep::BracketedPaste,
                AcquisitionStep::KeyboardMode,
                AcquisitionStep::PointerModes,
            ]
        );

        for (failed_index, failed_step) in super::ACQUISITION_STEPS.iter().enumerate() {
            let dropped = Rc::new(RefCell::new(Vec::new()));
            let error = {
                let mut resources = super::OwnedTerminal::new();
                resources
                    .acquire_in_order(|step, resources| {
                        install_acquisition_fixture(step, resources, dropped.clone());
                        if step == *failed_step {
                            Err(io::Error::other("injected acquisition-stage failure"))
                        } else {
                            Ok(())
                        }
                    })
                    .expect_err("injected acquisition-stage failure")
            };

            assert_eq!(error.kind(), io::ErrorKind::Other);
            let expected = super::ACQUISITION_STEPS[..=failed_index]
                .iter()
                .rev()
                .map(|step| match step {
                    AcquisitionStep::RawMode => "raw",
                    AcquisitionStep::Reader => "reader",
                    AcquisitionStep::AlternateScreen => "alternate",
                    AcquisitionStep::VisualTerminal => "visual",
                    AcquisitionStep::BracketedPaste => "paste",
                    AcquisitionStep::KeyboardMode => "keyboard",
                    AcquisitionStep::PointerModes => "pointer",
                })
                .collect::<Vec<_>>();
            assert_eq!(*dropped.borrow(), expected);
        }
    }

    #[test]
    fn partially_enabled_mode_is_reset_when_acquisition_fails() {
        let steps = Rc::new(RefCell::new(Vec::new()));
        let error = ModeGuard::finish_acquire(
            ModeGuard::tracked_for_test("partial mode", steps.clone(), false),
            || {
                steps.borrow_mut().push("enable");
                Err(io::Error::other("injected enable failure"))
            },
        )
        .err()
        .expect("failed mode enable");

        assert_eq!(error.to_string(), "injected enable failure");
        assert_eq!(*steps.borrow(), ["enable", "partial mode"]);
    }

    #[tokio::test]
    async fn release_continues_after_each_terminal_reset_failure() {
        for (failure, expected_error) in [
            ("graphics", "injected graphics writer failure"),
            ("pointer", "injected pointer reset failure"),
            ("keyboard", "injected keyboard reset failure"),
            ("paste", "injected paste reset failure"),
            ("clear", "injected clear failure"),
            ("cursor", "injected cursor failure"),
            ("flush", "injected flush failure"),
            ("alternate", "injected alternate reset failure"),
            ("raw", "injected raw reset failure"),
        ] {
            let steps = Rc::new(RefCell::new(Vec::new()));
            let backend =
                TerminalBackend::failing_lifecycle_writer(80, 24, vec![failure], steps.clone());
            let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);
            terminal.owned.terminal = Some(Terminal::new(backend).expect("test terminal"));
            terminal.owned.graphics_protocol = Some(GraphicsProtocol::Kitty);
            terminal.graphics_placement = Some(GraphicsPlacement {
                image_index: 0,
                digest: [1; 32],
                inner: Rect::new(1, 2, 3, 4),
            });
            terminal.owned.pointer_modes = Some(super::ModeGuard::tracked_for_test(
                "pointer",
                steps.clone(),
                failure == "pointer",
            ));
            {
                terminal.owned.keyboard = Some(super::ModeGuard::tracked_for_test(
                    "keyboard",
                    steps.clone(),
                    failure == "keyboard",
                ));
                terminal.owned.bracketed_paste = Some(super::ModeGuard::tracked_for_test(
                    "paste",
                    steps.clone(),
                    failure == "paste",
                ));
            }
            terminal.owned.alternate = Some(super::AlternateScreen::tracked_for_test(
                "alternate",
                steps.clone(),
                failure == "alternate",
            ));
            terminal.owned.raw_mode = Some(super::RawMode::tracked_for_test(
                "raw",
                steps.clone(),
                failure == "raw",
            ));

            let error = terminal.release().expect_err("injected reset failure");

            assert_eq!(error.to_string(), expected_error);
            let steps = steps.borrow();
            let order = [
                "graphics",
                "pointer",
                "keyboard",
                "paste",
                "clear",
                "cursor",
                "flush",
                "alternate",
                "raw",
            ];
            let positions = order
                .iter()
                .filter_map(|step| steps.iter().position(|seen| seen == step))
                .collect::<Vec<_>>();
            assert_eq!(positions.len(), order.len());
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[tokio::test]
    async fn help_and_usage_close_when_dialog_admission_fails() {
        let view = crate::presentation::team_view();
        for usage in [false, true] {
            let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);
            terminal.mode = TerminalMode::Fullscreen;
            let backend =
                TerminalBackend::failing_lifecycle_writer(80, 24, Vec::new(), Rc::default());
            terminal.owned.terminal = Some(Terminal::new(backend).expect("test terminal"));
            terminal.owned.input = Some(super::input::TerminalReader::closed_for_test());

            let opened = if usage {
                terminal.open_usage(&view, false, None).await
            } else {
                terminal.open_help(&view).await
            };

            assert!(
                matches!(opened, Err(TerminalError::InputClosed)),
                "usage: {usage}"
            );
            assert!(!terminal.help_open(), "usage: {usage}");
            assert!(terminal.panels.usage_panel.is_none(), "usage: {usage}");
        }
    }

    #[tokio::test]
    async fn scrollback_dump_leaves_the_screen_past_each_reset_failure() {
        for (failure, expected_error) in [
            ("pointer", "injected pointer reset failure"),
            ("keyboard", "injected keyboard reset failure"),
            ("alternate", "injected alternate reset failure"),
            ("cursor", "injected cursor failure"),
        ] {
            let steps = Rc::new(RefCell::new(Vec::new()));
            let backend =
                TerminalBackend::failing_lifecycle_writer(80, 24, vec![failure], steps.clone());
            let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);
            terminal.owned.terminal = Some(Terminal::new(backend).expect("test terminal"));
            terminal.owned.pointer_modes = Some(ModeGuard::tracked_for_test(
                "pointer",
                steps.clone(),
                failure == "pointer",
            ));
            terminal.owned.keyboard = Some(ModeGuard::tracked_for_test(
                "keyboard",
                steps.clone(),
                failure == "keyboard",
            ));
            terminal.owned.alternate = Some(AlternateScreen::tracked_for_test(
                "alternate",
                steps.clone(),
                failure == "alternate",
            ));

            let error = terminal.dump_to_scrollback(&crate::presentation::team_view());

            let Err(TerminalError::Io(error)) = error else {
                panic!("{failure}: expected the injected reset failure");
            };
            assert_eq!(error.to_string(), expected_error);
            assert!(terminal.transcript.scrollback_dump, "{failure}");
            assert!(terminal.owned.pointer_modes.is_none(), "{failure}");
            assert!(terminal.owned.keyboard.is_none(), "{failure}");
            assert!(terminal.owned.alternate.is_none(), "{failure}");
            let steps = steps.borrow();
            let positions = ["pointer", "keyboard", "alternate", "cursor"]
                .iter()
                .filter_map(|step| steps.iter().position(|seen| seen == step))
                .collect::<Vec<_>>();
            assert_eq!(positions.len(), 4, "{failure}: {steps:?}");
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[tokio::test]
    async fn release_keeps_first_error_and_retries_failed_mode_reset_on_drop() {
        let steps = Rc::new(RefCell::new(Vec::new()));
        let backend = TerminalBackend::failing_lifecycle_writer(
            80,
            24,
            vec!["clear", "cursor", "flush"],
            steps.clone(),
        );
        let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);
        terminal.owned.terminal = Some(Terminal::new(backend).expect("test terminal"));
        terminal.owned.pointer_modes = Some(super::ModeGuard::tracked_for_test(
            "pointer",
            steps.clone(),
            true,
        ));

        let error = terminal.release().expect_err("injected reset failures");

        assert_eq!(error.to_string(), "injected pointer reset failure");
        assert_eq!(
            *steps.borrow(),
            ["pointer", "pointer", "clear", "cursor", "flush"]
        );
    }

    #[tokio::test]
    async fn failed_reacquisition_keeps_terminal_released() {
        let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);

        let error = terminal
            .reacquire_with(|_, _, _| Err(super::TerminalError::Ineligible))
            .expect_err("injected acquisition failure");

        assert!(matches!(error, super::TerminalError::Ineligible));
        assert!(terminal.owned.input.is_none());
        assert!(terminal.owned.terminal.is_none());
    }

    fn install_acquisition_fixture(
        step: AcquisitionStep,
        resources: &mut super::OwnedTerminal,
        dropped: Rc<RefCell<Vec<&'static str>>>,
    ) {
        match step {
            AcquisitionStep::RawMode => {
                resources.raw_mode = Some(RawMode::tracked_for_test("raw", dropped, false));
            }
            AcquisitionStep::Reader => {
                resources.input = Some(super::input::TerminalReader::Test(dropped));
            }
            AcquisitionStep::AlternateScreen => {
                resources.alternate = Some(AlternateScreen::tracked_for_test(
                    "alternate",
                    dropped,
                    false,
                ));
            }
            AcquisitionStep::VisualTerminal => {
                let mut backend = TerminalBackend::new(TerminalMode::Fullscreen, None);
                backend.track_drop_for_test("visual", dropped);
                resources.terminal = Some(
                    Terminal::with_options(
                        backend,
                        TerminalOptions {
                            viewport: Viewport::Fullscreen,
                        },
                    )
                    .expect("test terminal"),
                );
            }
            AcquisitionStep::BracketedPaste => {
                resources.bracketed_paste =
                    Some(ModeGuard::tracked_for_test("paste", dropped, false));
            }
            AcquisitionStep::KeyboardMode => {
                resources.keyboard = Some(ModeGuard::tracked_for_test("keyboard", dropped, false));
                resources.graphics_protocol = None;
            }
            AcquisitionStep::PointerModes => {
                resources.pointer_modes =
                    Some(ModeGuard::tracked_for_test("pointer", dropped, false));
            }
        }
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::{ClipboardContent, Composer, TerminalInput, TerminalMode, TerminalNotice};

    #[test]
    fn pager_and_scrollback_dump_consume_pastes_before_the_hidden_composer() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let view = arany::SessionView {
                    usage: Default::default(),
                    rewinds: Default::default(),
                    id: arany::SessionId::new(),
                    title: "Pager".into(),
                    title_is_explicit: true,
                    inherited_title: None,
                    workspace_identity: None,
                    defaults: Default::default(),
                    host_epoch_floor: 0,
                    created_sequence: 1,
                    last_sequence: 1,
                    lineage: None,
                    runs: Vec::new(),
                    compactions: Vec::new(),
                    goals: Vec::new(),
                    command_tool_probe: None,
                    pending_provider_setup_edit: None,
                    provider_setup_uncertain: false,
                    environment_capture: None,
                };
                let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);
                terminal.mode = TerminalMode::Fullscreen;
                let mut composer = Composer::default();
                let payload = "paste ".repeat(200);
                composer.insert_paste(&payload).unwrap();
                composer.apply(TerminalInput::Home);
                // Handled input never reaches the composer, so the hidden draft cannot change.
                for (pager, dump) in [(true, false), (false, true)] {
                    terminal.transcript.transcript_open = pager;
                    terminal.transcript.scrollback_dump = dump;
                    for input in [TerminalInput::Tab, TerminalInput::Submit] {
                        assert!(!terminal.handle_draft_detail(&mut composer, input));
                        assert!(composer.paste_detail().is_none());
                        assert_eq!(composer.text(), payload);
                    }
                    terminal.clipboard_paste = Some(Ok(ClipboardContent::Text("pasted".into())));
                    assert!(
                        terminal
                            .handle_transcript_input(TerminalInput::Paste, &view)
                            .unwrap(),
                        "pager {pager}, dump {dump}"
                    );
                    assert!(
                        terminal.clipboard_paste.is_none(),
                        "the payload is consumed"
                    );
                }
                terminal.transcript.scrollback_dump = false;
                assert!(
                    terminal
                        .transcript
                        .history
                        .handle(TerminalInput::HistoryFind, &view)
                );
                assert!(!terminal.handle_draft_detail(&mut composer, TerminalInput::Tab));
                assert!(terminal.transcript.history.find_open());
                assert!(composer.paste_detail().is_none());
                assert!(
                    terminal
                        .transcript
                        .history
                        .handle(TerminalInput::Escape, &view)
                );
                terminal.mode = TerminalMode::Classic;
                terminal.transcript.transcript_open = true;
                assert!(terminal.handle_draft_detail(&mut composer, TerminalInput::Tab));
                assert!(composer.paste_detail().is_some());
                assert!(terminal.handle_draft_detail(&mut composer, TerminalInput::Escape));
                assert_eq!(composer.text(), payload);
            });
    }

    #[test]
    fn ctrl_c_with_a_selection_reaches_cancellation_during_work() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let view = arany::SessionView {
                    usage: Default::default(),
                    rewinds: Default::default(),
                    id: arany::SessionId::new(),
                    title: "Selection".into(),
                    title_is_explicit: true,
                    inherited_title: None,
                    workspace_identity: None,
                    defaults: Default::default(),
                    host_epoch_floor: 0,
                    created_sequence: 1,
                    last_sequence: 1,
                    lineage: None,
                    runs: Vec::new(),
                    compactions: Vec::new(),
                    goals: Vec::new(),
                    command_tool_probe: None,
                    pending_provider_setup_edit: None,
                    provider_setup_uncertain: false,
                    environment_capture: None,
                };
                let mut terminal = super::clipboard::cleanup_tests::released_terminal(None);
                terminal.mode = TerminalMode::Fullscreen;
                terminal.transcript.history.sync(&view, 80, 20);
                terminal
                    .transcript
                    .history
                    .record_notice(TerminalNotice::History("selected notice"));
                terminal.transcript.history.select_at(&view, 0, 0, 1);
                terminal.transcript.history.extend_selection(&view, 1, 79);
                assert!(terminal.transcript.history.has_selection());
                let composer = Composer::default();
                assert!(
                    !terminal
                        .handle_history_input(TerminalInput::Interrupt, &view, &composer, true)
                        .await,
                    "an active Run or compaction receives Ctrl+C"
                );
                assert!(
                    terminal.transcript.history.has_selection(),
                    "nothing was copied"
                );
                assert!(
                    terminal
                        .handle_history_input(TerminalInput::Escape, &view, &composer, true)
                        .await
                );
                assert!(
                    !terminal.transcript.history.has_selection(),
                    "Escape clears the selection"
                );
            });
    }
}

#[cfg(test)]
mod error_line_tests {
    #[test]
    fn errors_quoting_repository_data_are_escaped_at_the_terminal_boundary() {
        assert_eq!(
            super::error_line("locks: .git/refs/heads/\u{202e}kcol.x\u{1b}[2J; remove them"),
            "error: locks: .git/refs/heads/\\u{202e}kcol.x\\u{001b}[2J; remove them"
        );
    }
}

fn usage_now() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}
