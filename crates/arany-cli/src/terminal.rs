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
#[cfg(unix)]
use rustix::process::{Signal, getpid, kill_process};
#[cfg(unix)]
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
mod view;
use agent_chat::AgentChat;
pub use agent_chat::{AgentChatFollow, AgentChatInput};
use agents::{AgentPanel, PanelUpdate, window_start};
use backend::TerminalBackend;
use clipboard::{
    COPY_TOO_LARGE, ClipboardContent, ClipboardRead, MAX_COPY_BYTES, display_bytes, display_text,
};

fn read_pasted_workspace_image(
    input: &str,
    workspace: &std::path::Path,
    identity: arany::WorkspaceIdentity,
) -> Result<Option<arany::ImageAttachment>, &'static str> {
    use std::io::Read;
    let Some(path) = pasted_png_path(input, workspace)? else {
        return Ok(None);
    };
    let Some(file) = open_pinned_workspace_png(workspace, &path, identity)? else {
        return Ok(None);
    };
    let initial_capacity = file
        .metadata()
        .map_err(|_| "PNG file is unavailable")?
        .len()
        .min(arany::MAX_IMAGE_BYTES as u64) as usize;
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.take(arany::MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "PNG file could not be read")?;
    if bytes.len() > arany::MAX_IMAGE_BYTES {
        return Err("image is too large; choose a smaller PNG");
    }
    arany::ImageAttachment::from_png(&bytes)
        .map(Some)
        .map_err(|_| "invalid or unsupported PNG image")
}

const MAX_PASTED_PNG_PATH_BYTES: usize = 512;

fn pasted_png_path(
    input: &str,
    workspace: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, &'static str> {
    let path = input.trim().trim_matches(['"', '\'']);
    if path.len() > MAX_PASTED_PNG_PATH_BYTES
        || path.contains(char::is_control)
        || path.contains("://")
        || !path.to_ascii_lowercase().ends_with(".png")
    {
        return Ok(None);
    }
    let pasted_path = std::path::Path::new(path);
    let relative = if pasted_path.is_absolute() {
        let admitted_root = workspace
            .canonicalize()
            .map_err(|_| "Workspace is unavailable")?;
        pasted_path
            .strip_prefix(admitted_root)
            .map_err(|_| "PNG path must be inside the admitted Workspace")?
            .to_str()
            .ok_or("PNG path must be valid UTF-8")?
    } else {
        path.strip_prefix("./").unwrap_or(path)
    };
    let mention = serde_json::to_string(relative).map_err(|_| "invalid PNG path")?;
    let paths = arany::mention_paths(&format!("@{mention}"))
        .map_err(|_| "PNG path must name a file inside the admitted Workspace")?;
    let [relative_path] = paths.as_slice() else {
        return Err("PNG path must name a file inside the admitted Workspace");
    };
    Ok(Some(relative_path.clone()))
}

fn open_pinned_workspace_png(
    workspace: &std::path::Path,
    relative: &std::path::Path,
    identity: arany::WorkspaceIdentity,
) -> Result<Option<std::fs::File>, &'static str> {
    use rustix::fs::{Mode, OFlags, fstat, open};
    use std::os::unix::fs::MetadataExt;

    let workspace = workspace
        .canonicalize()
        .map_err(|_| "Workspace is unavailable")?;
    let root = open(
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| "Workspace is unavailable")?;
    let root_stat = fstat(&root).map_err(|_| "Workspace is unavailable")?;
    if identity.device != root_stat.st_dev as u64 || identity.inode != root_stat.st_ino as u64 {
        return Err("Workspace identity changed; PNG was not read");
    }
    let file = open_workspace_png(root, relative, root_stat.st_dev as u64)?;
    let Some(file) = file else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(|_| "PNG file is unavailable")?;
    if !metadata.is_file() || metadata.dev() != root_stat.st_dev as u64 || metadata.nlink() != 1 {
        return Err("PNG path must be a regular Workspace file without hard links");
    }
    if metadata.len() > arany::MAX_IMAGE_BYTES as u64 {
        return Err("image is too large; choose a smaller PNG");
    }
    Ok(Some(file))
}

fn open_workspace_png(
    root: rustix::fd::OwnedFd,
    relative: &std::path::Path,
    device: u64,
) -> Result<Option<std::fs::File>, &'static str> {
    use rustix::fs::{Mode, OFlags};
    let flags =
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{ResolveFlags, openat2};
        let _ = device;
        match openat2(
            &root,
            relative,
            flags,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV,
        ) {
            Ok(file) => Ok(Some(std::fs::File::from(file))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => Err("PNG path is unavailable or unsafe"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        use rustix::fs::openat;
        use std::os::unix::fs::MetadataExt;
        let relative = relative.to_str().ok_or("PNG path must be valid UTF-8")?;
        let components = relative.split('/').collect::<Vec<_>>();
        let mut file = std::fs::File::from(root);
        for (index, component) in components.iter().enumerate() {
            let flags = if index + 1 < components.len() {
                flags | OFlags::DIRECTORY
            } else {
                flags
            };
            file = match openat(&file, *component, flags, Mode::empty()) {
                Ok(file) => std::fs::File::from(file),
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(_) => return Err("PNG path is unavailable or unsafe"),
            };
            if file
                .metadata()
                .map_err(|_| "PNG path is unavailable")?
                .dev()
                != device
            {
                return Err("PNG path crosses a filesystem boundary");
            }
        }
        Ok(Some(file))
    }
}

#[cfg(test)]
mod workspace_image_tests {
    use super::read_pasted_workspace_image;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::os::unix::fs::MetadataExt;

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(not(arany_ci), ignore = "native Linux PNG bind-mount gate")]
    fn pasted_png_rejects_same_device_directory_and_file_mounts() {
        use arany::test_support::process::{BoundedOutput, assert_single_test_completed};
        const CHILD: &str = "terminal::workspace_image_tests::mounted_workspace_png_child";
        let _gate = arany::test_support::process::NATIVE_GATE.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(workspace.join("mounted")).unwrap();
        let bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .unwrap();
        std::fs::write(outside.join("photo.png"), &bytes).unwrap();
        std::fs::write(workspace.join("photo.png"), &bytes).unwrap();
        std::fs::write(workspace.join("leaf.png"), &bytes).unwrap();
        let output = std::process::Command::new("/usr/bin/bwrap")
            .env_clear()
            .env("ARANY_TEST_PNG_WORKSPACE", &workspace)
            .current_dir(&workspace)
            .args([
                "--unshare-user",
                "--unshare-pid",
                "--unshare-net",
                "--die-with-parent",
                "--ro-bind",
                "/",
                "/",
                "--bind",
            ])
            .arg(&outside)
            .arg(workspace.join("mounted"))
            .arg("--bind")
            .arg(outside.join("photo.png"))
            .arg(workspace.join("leaf.png"))
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .bounded_output()
            .unwrap();
        assert!(
            output.status.success(),
            "mount fixture: {:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_single_test_completed(&output.stdout, CHILD);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "helper-only: activated by the PNG bind-mount gate"]
    fn mounted_workspace_png_child() {
        let workspace =
            std::path::PathBuf::from(std::env::var_os("ARANY_TEST_PNG_WORKSPACE").unwrap());
        let metadata = std::fs::metadata(&workspace).unwrap();
        let identity = arany::WorkspaceIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        assert!(
            read_pasted_workspace_image("photo.png", &workspace, identity)
                .unwrap()
                .is_some()
        );
        for path in ["mounted/photo.png", "mounted/missing.png", "leaf.png"] {
            let mounted = workspace.join(if path == "mounted/missing.png" {
                "mounted"
            } else {
                path
            });
            assert_eq!(std::fs::metadata(mounted).unwrap().dev(), identity.device);
            assert_eq!(
                read_pasted_workspace_image(path, &workspace, identity),
                Err("PNG path is unavailable or unsafe"),
                "{path}"
            );
        }
    }

    #[test]
    fn pasted_png_reads_only_a_pinned_regular_workspace_file() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .unwrap();
        std::fs::create_dir(workspace.join("sub")).unwrap();
        for path in ["photo.png", "sub/photo.png", "PHOTO.PNG"] {
            std::fs::write(workspace.join(path), &bytes).unwrap();
        }
        let metadata = std::fs::metadata(&workspace).unwrap();
        let identity = arany::WorkspaceIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let absolute = workspace.join("photo.png");
        for path in [
            "photo.png",
            "sub/photo.png",
            "PHOTO.PNG",
            absolute.to_str().unwrap(),
        ] {
            let image = read_pasted_workspace_image(path, &workspace, identity)
                .unwrap()
                .unwrap();
            assert_eq!(image.byte_len(), bytes.len());
        }
        assert!(read_pasted_workspace_image("../photo.png", &workspace, identity).is_err());
        assert!(read_pasted_workspace_image("/tmp/photo.png", &workspace, identity).is_err());
        for path in [
            "missing.png",
            "missing/photo.png",
            "Please resize logo.png",
            "https://example.com/a.png",
            "http://example.com/a.PNG",
            "'https://example.com/a.png'",
            "file:///tmp/photo.png",
        ] {
            assert_eq!(
                read_pasted_workspace_image(path, &workspace, identity),
                Ok(None)
            );
        }
        assert!(
            read_pasted_workspace_image(
                "photo.png",
                &workspace,
                arany::WorkspaceIdentity {
                    inode: identity.inode + 1,
                    ..identity
                }
            )
            .is_err()
        );
        std::os::unix::fs::symlink("photo.png", workspace.join("linked.png")).unwrap();
        std::os::unix::fs::symlink("missing.png", workspace.join("dangling.png")).unwrap();
        std::os::unix::fs::symlink("sub", workspace.join("linked-sub")).unwrap();
        std::fs::hard_link(
            workspace.join("sub/photo.png"),
            workspace.join("hard-linked.png"),
        )
        .unwrap();
        for path in [
            "linked.png",
            "dangling.png",
            "linked-sub/photo.png",
            "hard-linked.png",
            "sub/photo.png",
        ] {
            assert!(
                read_pasted_workspace_image(path, &workspace, identity).is_err(),
                "{path}"
            );
        }
        std::fs::write(workspace.join("malformed.png"), b"not a PNG").unwrap();
        assert_eq!(
            read_pasted_workspace_image("malformed.png", &workspace, identity),
            Err("invalid or unsupported PNG image")
        );
        std::fs::File::create(workspace.join("oversized.png"))
            .unwrap()
            .set_len(arany::MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(
            read_pasted_workspace_image("oversized.png", &workspace, identity),
            Err("image is too large; choose a smaller PNG")
        );
        let replaced = temp.path().join("replaced-workspace");
        std::fs::rename(&workspace, replaced).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("photo.png"), &bytes).unwrap();
        assert!(read_pasted_workspace_image("photo.png", &workspace, identity).is_err());
        assert!(read_pasted_workspace_image("missing.png", &workspace, identity).is_err());
    }
}
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
use view::{
    AGENT_PICKER_ROWS, Palette, agent_detail_lines, agent_detail_rows, agent_picker_index,
    bottom_area, completion_height, composer_height, draw_agent_inspector_frame, draw_help_frame,
    draw_model_catalog_frame, draw_quick_actions_frame, draw_session_picker_frame,
    draw_settings_frame, draw_setup_choices_frame, draw_setup_frame, draw_setup_text_frame,
    draw_setup_warning_frame, history_top_padding, screen_terminal, session_picker_index,
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
    #[cfg(unix)]
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

async fn wait_panel_copy(panel_copy: &mut Option<PanelCopy>) -> PanelCopyResult {
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
    fn clear(&mut self) {
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

    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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
#[cfg(unix)]
const BRACKETED_PASTE: [&[u8]; 2] = [b"\x1b[?2004h", b"\x1b[?2004l"];
#[cfg(unix)]
const KEYBOARD_DISAMBIGUATION: [&[u8]; 2] = [b"\x1b[>1u", b"\x1b[<u"];
#[cfg(unix)]
const KEYBOARD_ONLY_QUERY: &[u8] = b"\x1b[?u\x1b[c";
#[cfg(unix)]
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

fn mouse_capture_enabled() -> bool {
    !std::env::var_os("ARANY_DISABLE_MOUSE").is_some_and(|value| value != "0")
}

#[cfg(unix)]
fn graphics_probe_allowed(mode: TerminalMode) -> bool {
    mode == TerminalMode::Fullscreen
        && mouse_capture_enabled()
        && std::env::var_os("TMUX").is_none()
}

#[cfg(unix)]
fn graphics_protocol(
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

#[cfg(unix)]
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

fn hovered_image_at(chips: &[(usize, Rect)], column: u16, row: u16) -> Option<usize> {
    chips.iter().find_map(|(index, area)| {
        area.contains(ratatui::layout::Position::new(column, row))
            .then_some(*index)
    })
}

fn pointer_position(input: TerminalInput) -> Option<(u16, u16)> {
    match input {
        TerminalInput::PointerMove { column, row }
        | TerminalInput::PointerDrag { column, row }
        | TerminalInput::PointerRelease { column, row } => Some((column, row)),
        _ => None,
    }
}

fn transition_hover(current: &mut Option<usize>, next: Option<usize>) -> bool {
    if *current == next {
        return false;
    }
    *current = next;
    true
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
    #[cfg(unix)]
    bracketed_paste: Option<ModeGuard>,
    #[cfg(unix)]
    keyboard: Option<ModeGuard>,
    graphics_protocol: Option<GraphicsProtocol>,
    pointer_modes: Option<ModeGuard>,
}

impl OwnedTerminal {
    fn new() -> Self {
        Self {
            raw_mode: None,
            input: None,
            alternate: None,
            terminal: None,
            #[cfg(unix)]
            bracketed_paste: None,
            #[cfg(unix)]
            keyboard: None,
            graphics_protocol: None,
            pointer_modes: None,
        }
    }

    fn terminal(&mut self) -> &mut Terminal<TerminalBackend> {
        self.terminal_with_context("owned terminal")
    }

    fn terminal_with_context(&mut self, context: &'static str) -> &mut Terminal<TerminalBackend> {
        self.terminal.as_mut().expect(context)
    }

    fn reader(&mut self) -> &mut TerminalReader {
        self.input.as_mut().expect("active terminal input")
    }

    fn active(&self) -> bool {
        self.input.is_some()
    }

    fn stop_reader(&mut self) {
        if let Some(input) = &mut self.input {
            input.stop();
        }
    }

    fn release(
        &mut self,
        scrollback_dump: &mut bool,
        first_error: &mut Option<io::Error>,
        after_reader: impl FnOnce(&mut Option<io::Error>),
        close_linear: impl FnOnce() -> io::Result<()>,
    ) {
        self.stop_reader();
        self.input.take();
        after_reader(first_error);
        if let Some(mut modes) = self.pointer_modes.take() {
            attempt_cleanup(first_error, || modes.release());
        }
        #[cfg(unix)]
        if let Some(mut keyboard) = self.keyboard.take() {
            attempt_cleanup(first_error, || keyboard.release());
        }
        #[cfg(unix)]
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
}

#[cfg(unix)]
struct TerminalSignals {
    interrupt: tokio::signal::unix::Signal,
    suspend: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(not(unix))]
struct TerminalSignals;

impl AttachedTerminal {
    fn from_acquired(
        owned: OwnedTerminal,
        mode_label: Option<&'static str>,
        mode: TerminalMode,
        pointer: PointerSettings,
        screen_reader: bool,
        _signals: TerminalSignals,
    ) -> Self {
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
            #[cfg(unix)]
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
            #[cfg(unix)]
            interrupt: _signals.interrupt,
            #[cfg(unix)]
            suspend: _signals.suspend,
            #[cfg(unix)]
            hangup: _signals.hangup,
            #[cfg(unix)]
            terminate: _signals.terminate,
        }
    }
}

impl OwnedTerminal {
    fn acquire_in_order(
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
        #[cfg(unix)]
        self.keyboard.take();
        #[cfg(unix)]
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

fn classic_cursor_timeout_fallback<T>(
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
    fn acquire(mode: TerminalMode, mode_label: &mut Option<&'static str>) -> io::Result<Self> {
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
                    #[cfg(unix)]
                    {
                        resources.bracketed_paste = mode_label
                            .is_none()
                            .then(|| ModeGuard::acquire(BRACKETED_PASTE))
                            .transpose()?;
                    }
                }
                AcquisitionStep::KeyboardMode => {
                    #[cfg(unix)]
                    {
                        let graphics_probe = mode_label.is_none() && graphics_probe_allowed(mode);
                        let reader = resources.input.as_ref().expect("terminal reader acquired");
                        resources.keyboard = keyboard_mode(reader, graphics_probe)?;
                        resources.graphics_protocol =
                            detected_graphics_protocol(reader, graphics_probe);
                    }
                }
                AcquisitionStep::PointerModes => {
                    resources.pointer_modes = pointer_modes(
                        mode,
                        mode_label.is_some(),
                        mouse_capture_enabled(),
                        resources.graphics_protocol.is_some(),
                    )?;
                }
            }
            Ok(())
        })?;
        Ok(resources)
    }
}

fn attempt_cleanup(first_error: &mut Option<io::Error>, cleanup: impl FnOnce() -> io::Result<()>) {
    if let Err(error) = cleanup() {
        first_error.get_or_insert(error);
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

impl AlternateScreen {
    fn acquire() -> io::Result<Self> {
        let guard = Self {
            active: true,
            #[cfg(test)]
            test_reset: None,
        };
        execute!(io::stderr(), EnterAlternateScreen)?;
        Ok(guard)
    }

    fn release(&mut self) -> io::Result<()> {
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
    fn tracked_for_test(
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
        Ok(Self {
            active: true,
            #[cfg(test)]
            test_reset: None,
        })
    }

    fn release(&mut self) -> io::Result<()> {
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
    fn tracked_for_test(
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
    fn acquire([enable, disable]: [&'static [u8]; 2]) -> io::Result<Self> {
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

    fn finish_acquire(guard: Self, enable: impl FnOnce() -> io::Result<()>) -> io::Result<Self> {
        enable()?;
        Ok(guard)
    }

    fn release(&mut self) -> io::Result<()> {
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
    fn tracked_for_test(
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

#[cfg(unix)]
fn keyboard_mode(input: &TerminalReader, graphics: bool) -> io::Result<Option<ModeGuard>> {
    match input.probe_keyboard(&mut io::stderr(), graphics)? {
        input::KeyboardSupport::Disambiguated => {
            ModeGuard::acquire(KEYBOARD_DISAMBIGUATION).map(Some)
        }
        input::KeyboardSupport::Legacy => Ok(None),
    }
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    suspend: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
    #[cfg(unix)]
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
        let width = self.owned().size()?.width;
        let height = view::permission_height(&lines, width, false, profile_start);
        self.draw_frame(height, |frame, area| {
            if area.width >= 16 && area.height >= 8 {
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
    pub fn acquire_with_mode(
        screen_reader: bool,
        mode: TerminalMode,
        pointer: PointerSettings,
    ) -> Result<Self, TerminalError> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(TerminalError::Ineligible);
        }
        #[cfg(unix)]
        let signals = TerminalSignals {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            suspend: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
                Signal::TSTP.as_raw(),
            ))?,
            hangup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        };
        #[cfg(not(unix))]
        let signals = TerminalSignals;
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

    fn keyboard_support(&self) -> input::KeyboardSupport {
        #[cfg(unix)]
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
        let clipboard_ready = self
            .owned
            .input
            .as_ref()
            .is_some_and(TerminalReader::paste_ready);
        #[cfg(unix)]
        let input = tokio::select! {
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
        };
        #[cfg(not(unix))]
        let input = tokio::select! {
            input = self.owned.reader().recv() => InputRead::Input(input?),
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
        };
        let input = match input {
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
        if chat
            && self.owned.graphics_protocol.is_some()
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
        if let Some(result) = self
            .panel_pointer_input(input, mouse_capture_enabled())
            .await
        {
            return Ok(result);
        }
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

    fn insert_native(
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

    fn sync_native(&mut self, session: &SessionView) -> Result<(), TerminalError> {
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

    // `[` in the pager: leave capture and the alternate screen, then append the bounded transcript.
    fn dump_to_scrollback(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        let text = history::scrollback_transcript(view);
        self.clear_graphics_hover();
        self.delete_graphics_placement()?;
        if let Some(mut modes) = self.owned.pointer_modes.take() {
            modes.release()?;
        }
        #[cfg(unix)]
        if let Some(mut keyboard) = self.owned.keyboard.take() {
            keyboard.release()?;
        }
        self.transcript.scrollback_dump = true;
        if let Some(mut alternate) = self.owned.alternate.take() {
            alternate.release()?;
        }
        self.owned().show_cursor()?;
        // Raw mode turns off output processing, so the text carries its own carriage returns.
        let mut output = io::stderr().lock();
        output.write_all(text.as_bytes())?;
        output.flush()?;
        Ok(())
    }

    fn end_scrollback_dump(&mut self) -> Result<(), TerminalError> {
        if !self.transcript.scrollback_dump {
            return Ok(());
        }
        self.owned.alternate = Some(AlternateScreen::acquire()?);
        self.transcript.scrollback_dump = false;
        #[cfg(unix)]
        {
            let graphics_probe = graphics_probe_allowed(self.mode);
            self.owned.keyboard = keyboard_mode(self.reader(), graphics_probe)?;
            self.owned.graphics_protocol =
                detected_graphics_protocol(self.reader(), graphics_probe);
        }
        self.owned.pointer_modes = pointer_modes(
            self.mode,
            false,
            mouse_capture_enabled(),
            self.owned.graphics_protocol.is_some(),
        )?;
        // A Fullscreen resize clears and invalidates the frame; `Terminal::clear` would query the cursor.
        let terminal = self.owned();
        let area = Rect::from((ratatui::layout::Position::ORIGIN, terminal.size()?));
        terminal.resize(area)?;
        terminal.backend_mut().set_area(area);
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

    // Mouse gestures select History text; only they, a copy key or `/copy` reach the clipboard writer.
    async fn pointer_input(
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

    async fn panel_pointer_input(
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

    fn finish_panel_copy(
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
                !selects || !std::mem::take(&mut self.graphics_hover_changed)
            }
            TerminalInput::PointerClick { column, row, .. } => {
                !selects
                    || (self.history_cell(column, row).is_none()
                        && !self.transcript.history.selecting())
            }
            TerminalInput::PointerDrag { .. } => {
                let graphics_changed = std::mem::take(&mut self.graphics_hover_changed);
                !selects || (!self.transcript.history.selecting() && !graphics_changed)
            }
            TerminalInput::PointerRelease { .. } => {
                if !self.transcript.history.has_selection() {
                    self.transcript.history.clear_selection();
                }
                let graphics_changed = std::mem::take(&mut self.graphics_hover_changed);
                !selects
                    || (!graphics_changed
                        && (!self.pointer.copy_on_select
                            || !self.transcript.history.has_selection()))
            }
            _ => false,
        }
    }

    pub fn take_graphics_hover_changed(&mut self) -> bool {
        std::mem::take(&mut self.graphics_hover_changed)
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

    pub fn paste_into(&mut self, composer: &mut Composer) -> Result<ComposerEdit, &'static str> {
        self.paste_into_workspace(composer, None)
    }

    pub fn paste_into_admitted_workspace(
        &mut self,
        composer: &mut Composer,
        workspace: &std::path::Path,
        identity: arany::WorkspaceIdentity,
    ) -> Result<ComposerEdit, &'static str> {
        self.paste_into_workspace(composer, Some((workspace, identity)))
    }

    fn paste_into_workspace(
        &mut self,
        composer: &mut Composer,
        workspace: Option<(&std::path::Path, arany::WorkspaceIdentity)>,
    ) -> Result<ComposerEdit, &'static str> {
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
        if let Some((workspace, identity)) = workspace
            && let Some(image) = read_pasted_workspace_image(&text, workspace, identity)?
        {
            composer.attach_image(image)?;
            self.clipboard_preview = self.is_linear();
            return Ok(ComposerEdit::Changed);
        }
        match composer.insert_paste(&text)? {
            ComposerEdit::AtCapacity => Err("paste is too large; paste a smaller section"),
            edit => {
                self.reader()
                    .restore_input_capacity(composer.input_capacity())
                    .expect("admitted Composer input capacity");
                self.clipboard_preview = self.is_linear() && edit == ComposerEdit::Changed;
                Ok(edit)
            }
        }
    }

    pub fn restore_draft_input(&mut self, capacity: usize) -> Result<(), TerminalError> {
        self.reader()
            .restore_input_capacity(capacity)
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
        self.reacquire_with(|screen_reader, mode, pointer| {
            Self::acquire_with_mode(screen_reader, mode, pointer)
        })
    }

    fn reacquire_with(
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

    #[cfg(unix)]
    pub fn suspend_and_resume(&mut self, input_capacity: usize) -> Result<(), TerminalError> {
        let screen_reader = self.requested_screen_reader;
        let secret_input = self.secret_echo.is_some();
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
        if secret_input {
            resumed.begin_secret_input()?;
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

    #[cfg(not(unix))]
    pub fn suspend_and_resume(&mut self, _input_capacity: usize) -> Result<(), TerminalError> {
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
        let height = self.owned().get_frame().area().height;
        self.prepare_frame_height(height)
    }

    fn prepare_frame_height(&mut self, height: u16) -> Result<(), TerminalError> {
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

    fn draw_frame(
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
        if self.hovered_image.is_none() {
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

    fn clear_graphics_hover(&mut self) {
        self.sixel_preview.clear();
        transition_hover(&mut self.hovered_image, None);
        self.pointer_position = None;
        self.hovered_image_digest = None;
        self.image_chip_areas.clear();
        self.graphics_hover_changed = false;
    }

    fn delete_graphics_placement(&mut self) -> io::Result<()> {
        if self.graphics_placement.take().is_none() {
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

    fn draw_dialog(
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

    fn draw_snapshot(
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
        // The primary screen shows the dump until Enter; the Run keeps going without redraws.
        if self.transcript.scrollback_dump {
            if let Some(notice) = notice {
                self.transcript.history.record_notice(notice);
            }
            return Ok(());
        }
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
        let cue = self.panels.selection_cue.clone();
        let notice = notice.or_else(|| cue.as_deref().map(TerminalNotice::Transient));
        let screen = Rect::from((ratatui::layout::Position::ORIGIN, self.owned().size()?));
        if self
            .panels
            .frame_area
            .is_some_and(|previous| previous != screen)
        {
            self.clear_graphics_hover();
        }
        self.panels.frame_area = Some(screen);
        if let Some(index) = self.hovered_image
            && self.hovered_image_digest.is_some_and(|digest| {
                composer.images().get(index).map(ImageAttachment::digest) != Some(digest)
            })
        {
            self.clear_graphics_hover();
        }
        if let Some((label, text, scroll)) = composer.paste_detail() {
            let palette = self.palette();
            return self.draw_frame(screen.height, |frame, area| {
                view::draw_paste_detail(frame, area, &label, text, scroll, palette);
            });
        }
        if self.mode == TerminalMode::Fullscreen && self.transcript.transcript_open {
            return self.draw_pager(view, notice, screen);
        }
        let mut model = match state {
            SnapshotState::Preparing => PresentationModel::preparing(view, screen.width),
            _ => PresentationModel::from_session(view, screen.width),
        };
        let height = if let Some(picker) = picker {
            composer_height(composer, screen)
                .saturating_add(1)
                .saturating_add(
                    u16::try_from(picker.items.len().max(1).saturating_add(3))
                        .unwrap_or(u16::MAX)
                        .min(12),
                )
                .max(8)
                .min(screen.height)
        } else if self.mode == TerminalMode::Classic {
            self.transcript.classic_history.sync(
                view,
                usize::from(screen.width),
                usize::from(screen.height),
            );
            view::classic_height(
                &model,
                composer,
                screen,
                self.transcript.classic_history.live_rows(view).len(),
            )
        } else {
            screen.height
        };
        self.prepare_frame_height(height)?;
        self.sync_native(view)?;
        if self.mode == TerminalMode::Classic
            && let Some(TerminalNotice::History(text)) = notice
        {
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
        let history_rows = if self.mode == TerminalMode::Fullscreen {
            self.history_mut().visible(view)
        } else {
            self.transcript.classic_history.live_rows(view)
        };
        let native_transcript = self.mode == TerminalMode::Classic && !view.runs.is_empty();
        let history_status = if native_transcript {
            let hint = classic_history_status(
                area.width,
                self.transcript.native.omitted_history,
                self.classic_pinned,
            );
            if self.transcript.native.omitted_history {
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
        let status_notice = notice
            .filter(|notice| {
                matches!(notice, TerminalNotice::Transient(_))
                    || (self.mode == TerminalMode::Fullscreen && history_height == 0)
            })
            .map(TerminalNotice::text);
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        let mut history_area = Rect::default();
        let mut image_chip_areas = Vec::new();
        let mut preview_inner = None;
        let pointer_position = self.pointer_position;
        let mut hovered_image = self.hovered_image;
        let graphics_protocol = self.owned.graphics_protocol;
        let published_graphics = self.graphics_placement;
        let ready_graphics = self.sixel_preview.ready_placement();
        let newline_hint = composer.newline_hint(self.keyboard_support());
        self.draw_frame(height, |frame, _| {
            let geometry = view::draw_frame_with_history_and_chips(
                frame,
                &model,
                composer,
                status_notice,
                view::ComposerHints::new(palette, newline_hint),
                &history_rows,
                history_status.as_deref(),
            );
            history_area = geometry.history_area;
            image_chip_areas = geometry.image_chips;
            if let Some(position) = pointer_position {
                hovered_image = hovered_image_at(&image_chip_areas, position.x, position.y);
            }
            if let Some(index) = hovered_image
                && let Some(image) = composer.images().get(index)
                && let Some(chip) = image_chip_areas
                    .iter()
                    .find_map(|(chip_index, area)| (*chip_index == index).then_some(*area))
                && let Some((balloon, inner)) =
                    graphics::preview_geometry(frame.area(), chip, image)
            {
                let placement = GraphicsPlacement {
                    image_index: index,
                    digest: image.digest(),
                    inner,
                };
                let interior = graphics::preview_interior(
                    graphics_protocol,
                    published_graphics,
                    ready_graphics,
                    placement,
                );
                view::draw_image_preview(
                    frame,
                    balloon,
                    &image_description(image, index),
                    interior,
                );
                preview_inner = Some(inner);
            }
            if let Some(picker) = picker {
                draw_model_catalog_frame(
                    frame,
                    picker,
                    armed,
                    composer,
                    &model,
                    view::ComposerHints::new(palette, newline_hint),
                );
            }
        })?;
        self.panels.history_area = (self.mode == TerminalMode::Fullscreen).then_some(history_area);
        self.image_chip_areas = image_chip_areas;
        self.hovered_image = hovered_image;
        self.hovered_image_digest = self
            .hovered_image
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
            .hovered_image
            .and_then(|index| composer.images().get(index).map(|image| (index, image)));
        let preview = image.and_then(|(index, image)| inner.map(|inner| (index, image, inner)));
        let placement = preview.map(|(index, image, inner)| GraphicsPlacement {
            image_index: index,
            digest: image.digest(),
            inner,
        });
        if placement.is_none() {
            self.sixel_preview.clear();
        }
        let unchanged = placement.is_some() && self.graphics_placement == placement;
        if unchanged {
            return Ok(());
        }
        let mut bytes = Vec::new();
        if self.graphics_placement.take().is_some()
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
                        self.sixel_preview.request(placement, image, cell);
                    }
                    let Some(bytes) = self.sixel_preview.take_ready() else {
                        return Ok(());
                    };
                    bytes
                }
            });
            self.graphics_placement = placement;
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
        let width = self.owned().size()?.width;
        self.draw_frame(if width < 20 { 8 } else { 7 }, |frame, area| {
            draw_setup_text_frame(frame, area, title, instruction, draft, notice, palette)
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
        self.draw_frame(8, |frame, area| {
            draw_setup_choices_frame(
                frame,
                area,
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
        let width = self.owned().size()?.width;
        let height = view::setup_warning_height(warning, width);
        self.draw_frame(height, |frame, area| {
            visible = area.width >= 16 && area.height >= 8;
            has_more = draw_setup_warning_frame(
                frame,
                area,
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
            |frame, area| draw_session_picker_frame(frame, area, items, selected, armed, palette),
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
        rows: &[String],
        selected: usize,
        notice: Option<&str>,
        session: &SessionView,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .draw_settings(rows, notice)
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
            |frame, area| draw_settings_frame(frame, area, rows, selected, armed, notice, palette),
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
            draw_quick_actions_frame(frame, area, selected, armed, has_draft, palette);
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
        self.begin_dialog();
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
        self.draw_help(0, session)?;
        self.arm_dialog().await?;
        self.draw_help(0, session)
    }

    pub async fn open_help(&mut self, session: &SessionView) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            return linear
                .write_help(&mut io::stderr())
                .map_err(TerminalError::Io);
        }
        self.begin_dialog();
        self.panels.help_selected = Some(0);
        self.draw_help(0, session)?;
        self.arm_dialog().await?;
        self.draw_help(0, session)
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
                TerminalInput::Up | TerminalInput::PointerScrollUp { .. } => {
                    *selected = selected.saturating_sub(1)
                }
                TerminalInput::Down | TerminalInput::PointerScrollDown { .. } => {
                    *selected = (*selected + 1).min(max_scroll)
                }
                TerminalInput::PageUp => *selected = selected.saturating_sub(10),
                TerminalInput::PageDown => *selected = (*selected + 10).min(max_scroll),
                TerminalInput::Home => *selected = 0,
                TerminalInput::End => *selected = max_scroll,
                TerminalInput::Escape
                | TerminalInput::Submit
                | TerminalInput::Interrupt
                | TerminalInput::EndOfInput => self.close_help(),
                _ => {}
            }
            return;
        }
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

    fn draw_help(&mut self, selected: usize, session: &SessionView) -> Result<(), TerminalError> {
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
                    &lines,
                    selected,
                    (page, pages),
                    armed,
                    palette,
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

    pub async fn open_agents(
        &mut self,
        view: &SessionView,
        locked: bool,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        self.begin_dialog();
        self.agent_panel = Some(AgentPanel::new(locked));
        self.draw_agents(view)?;
        if let Err(error) = self.arm_dialog().await {
            let _ = self.close_agents();
            return Err(error);
        }
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
        #[cfg(unix)]
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
            PanelUpdate::Rearm => {
                self.begin_dialog();
                self.draw_agents(view)?;
                if let Err(error) = self.arm_dialog().await {
                    let _ = self.close_agents();
                    return Err(error);
                }
                self.draw_agents(view).map(|()| None)
            }
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

    fn draw_agents(&mut self, view: &SessionView) -> Result<(), TerminalError> {
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
                let start = window_start(model.selected, MAX_AGENT_ROWS);
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
            draw_agent_inspector_frame(frame, area, &model, scroll, armed, notice, palette);
        })?;
        self.panels.agent_picker_area = Some(area);
        Ok(())
    }

    fn release(&mut self) -> io::Result<()> {
        self.clear_graphics_hover();
        self.cancel_clipboard_paste();
        let mut first_error = None;
        attempt_cleanup(&mut first_error, || self.delete_graphics_placement());
        let owned = &mut self.owned;
        let scrollback_dump = &mut self.transcript.scrollback_dump;
        #[cfg(unix)]
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
                #[cfg(unix)]
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

fn rendered_panel_lines(buffer: &ratatui::buffer::Buffer, area: Rect) -> Vec<String> {
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

fn draw_panel_feedback(
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

// A Classic viewport is exactly its dialog; Fullscreen dialogs sit at the bottom under live
// history.
fn dialog_area(frame: Rect, height: u16, classic: bool) -> Rect {
    if classic {
        frame
    } else {
        bottom_area(frame, height)
    }
}

// The escaped detail text bounded like `/transcript`: 16 Runs, 64 KiB and a limit cue.
fn bounded_transcript(view: &SessionView) -> io::Result<String> {
    let mut text = Vec::new();
    LinearState::new("classic").write_transcript(&mut text, view)?;
    Ok(String::from_utf8(text).expect("escaped transcript is UTF-8"))
}

// History keeps a rectangle disjoint from bottom-area chrome, so it cannot overwrite a dialog.
fn draw_history_above(
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
fn classic_keeps_top(area: Rect, history_adjacent: bool) -> bool {
    history_adjacent && area.y > 0
}

fn resize_classic<B: Backend>(
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
        TerminalBackend, TerminalMode, classic_cursor_timeout_fallback,
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
            #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

#[cfg(all(test, unix))]
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

#[cfg(test)]
mod panel_selection_tests {
    use super::{
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

    #[cfg(unix)]
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
                    Some(super::COPY_TOO_LARGE)
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
