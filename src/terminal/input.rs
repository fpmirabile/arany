use super::TerminalError;
#[cfg(unix)]
use super::composer::MAX_DRAFT_BYTES;
#[cfg(not(unix))]
use crossterm::event;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
#[cfg(unix)]
use mio::{Events, Interest, Poll, Token, unix::SourceFd};
#[cfg(unix)]
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
#[cfg(unix)]
use std::collections::VecDeque;
use std::io;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;

const INPUT_QUEUE_CAPACITY: usize = 32;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(unix)]
mod raw;
// Stay below canonical buffers so discarded suffixes cannot become accepted input.
#[cfg(target_os = "linux")]
pub(super) const MAX_LINEAR_LINE_BYTES: usize = 4094;
#[cfg(all(unix, not(target_os = "linux")))]
pub(super) const MAX_LINEAR_LINE_BYTES: usize = 1023;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    Hangup,
    Terminate,
}

impl ShutdownSignal {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Terminate => "SIGTERM",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalInput {
    Character(char),
    Paste,
    ClipboardPaste,
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    WordLeft,
    WordRight,
    BackspaceWord,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
    CycleApprovalMode,
    QuickActions,
    HistoryFind,
    HistoryLive,
    Submit,
    Escape,
    Interrupt,
    EndOfInput,
    Suspend,
    Shutdown(ShutdownSignal),
    Resize,
    PointerMove { column: u16, row: u16 },
    PointerClick { column: u16, row: u16 },
    PointerScrollUp { column: u16, row: u16 },
    PointerScrollDown { column: u16, row: u16 },
    LineRejected,
    LineContinued,
}

pub(super) enum TerminalReader {
    Keys(InputReader),
    #[cfg(unix)]
    Lines(LineReader),
}

impl TerminalReader {
    pub(super) fn start(linear: bool) -> io::Result<Self> {
        #[cfg(unix)]
        if linear {
            return LineReader::start().map(Self::Lines);
        }
        let _ = linear;
        InputReader::start().map(Self::Keys)
    }

    pub(super) async fn recv(&mut self) -> Result<TerminalInput, TerminalError> {
        match self {
            Self::Keys(reader) => reader.recv().await,
            #[cfg(unix)]
            Self::Lines(reader) => reader.recv().await,
        }
    }

    pub(super) fn stop(&mut self) {
        match self {
            Self::Keys(reader) => reader.stop(),
            #[cfg(unix)]
            Self::Lines(reader) => reader.stop(),
        }
    }

    pub(super) fn reset_draft(&mut self) {
        #[cfg(unix)]
        if let Self::Lines(reader) = self {
            reader.reset_draft();
        }
    }

    pub(super) fn take_paste(&mut self) -> Option<Result<String, &'static str>> {
        match self {
            Self::Keys(reader) => reader.paste.take(),
            #[cfg(unix)]
            Self::Lines(_) => None,
        }
    }

    pub(super) fn paste_ready(&self) -> bool {
        match self {
            Self::Keys(_) => true,
            #[cfg(unix)]
            Self::Lines(reader) => reader.pending.is_empty(),
        }
    }

    pub(super) fn restore_draft_bytes(&mut self, bytes: usize) -> io::Result<()> {
        #[cfg(unix)]
        {
            if bytes > MAX_DRAFT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "draft too long",
                ));
            }
            if let Self::Lines(reader) = self {
                reader.draft_bytes = bytes;
            }
        }
        #[cfg(not(unix))]
        let _ = bytes;
        Ok(())
    }
}

#[cfg(unix)]
enum LineEvent {
    Submitted(Vec<u8>),
    Continued(Vec<u8>),
    Rejected,
    EndOfInput,
}

#[cfg(unix)]
pub(super) struct LineReader {
    receiver: mpsc::Receiver<io::Result<LineEvent>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pending: VecDeque<TerminalInput>,
    draft_bytes: usize,
    stdin_flags: Option<StdinFlags>,
}

#[cfg(unix)]
struct StdinFlags(OFlags);

#[cfg(unix)]
impl StdinFlags {
    fn acquire() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = fcntl_getfl(&stdin)?;
        fcntl_setfl(&stdin, original | OFlags::NONBLOCK)?;
        Ok(Self(original))
    }
}

#[cfg(unix)]
impl Drop for StdinFlags {
    fn drop(&mut self) {
        let _ = fcntl_setfl(io::stdin(), self.0);
    }
}

#[cfg(unix)]
impl LineReader {
    fn start() -> io::Result<Self> {
        let mut poll = Poll::new()?;
        let fd = io::stdin().as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
        let stdin_flags = StdinFlags::acquire()?;
        let (sender, receiver) = mpsc::channel(4);
        let shutdown = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("arany-terminal-line-input".into())
            .spawn(move || {
                let mut input = io::stdin();
                let mut events = Events::with_capacity(4);
                let mut bytes = [0; 8194];
                while !should_stop.load(Ordering::Acquire) {
                    match poll.poll(&mut events, Some(INPUT_POLL_INTERVAL)) {
                        Ok(()) if events.is_empty() => continue,
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            let _ = sender.blocking_send(Err(error));
                            break;
                        }
                    }
                    let event = match input.read(&mut bytes) {
                        Ok(0) => LineEvent::EndOfInput,
                        Ok(len) => decode_line(&bytes[..len]),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if let Err(error) = poll.registry().reregister(
                                &mut SourceFd(&fd),
                                Token(0),
                                Interest::READABLE,
                            ) {
                                let _ = sender.blocking_send(Err(error));
                                break;
                            }
                            continue;
                        }
                        Err(error) => {
                            let _ = sender.blocking_send(Err(error));
                            break;
                        }
                    };
                    if let Err(error) =
                        poll.registry()
                            .reregister(&mut SourceFd(&fd), Token(0), Interest::READABLE)
                    {
                        let _ = sender.blocking_send(Err(error));
                        break;
                    }
                    if sender.blocking_send(Ok(event)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            receiver,
            shutdown,
            thread: Some(thread),
            pending: VecDeque::new(),
            draft_bytes: 0,
            stdin_flags: Some(stdin_flags),
        })
    }

    async fn recv(&mut self) -> Result<TerminalInput, TerminalError> {
        if let Some(input) = self.pending.pop_front() {
            return Ok(input);
        }
        let event = self
            .receiver
            .recv()
            .await
            .ok_or(TerminalError::InputClosed)?
            .map_err(TerminalError::Io)?;
        match event {
            LineEvent::Submitted(bytes) => self.accept_segment(bytes, true),
            LineEvent::Continued(bytes) => self.accept_segment(bytes, false),
            LineEvent::Rejected => Ok(TerminalInput::LineRejected),
            LineEvent::EndOfInput => Ok(TerminalInput::EndOfInput),
        }
    }

    fn accept_segment(
        &mut self,
        bytes: Vec<u8>,
        submitted: bool,
    ) -> Result<TerminalInput, TerminalError> {
        let Some(line) = validate_segment(bytes, self.draft_bytes) else {
            return Ok(TerminalInput::LineRejected);
        };
        self.draft_bytes = if submitted {
            0
        } else {
            self.draft_bytes + line.len()
        };
        self.pending
            .extend(line.chars().map(TerminalInput::Character));
        self.pending.push_back(if submitted {
            TerminalInput::Submit
        } else {
            TerminalInput::LineContinued
        });
        Ok(self
            .pending
            .pop_front()
            .expect("line contains a boundary event"))
    }

    fn reset_draft(&mut self) {
        self.draft_bytes = 0;
        self.pending.clear();
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.receiver.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.stdin_flags.take();
    }
}

#[cfg(unix)]
impl Drop for LineReader {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(unix)]
fn decode_line(bytes: &[u8]) -> LineEvent {
    let submitted = bytes.ends_with(b"\n");
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > MAX_LINEAR_LINE_BYTES || line.contains(&0) {
        LineEvent::Rejected
    } else if submitted {
        LineEvent::Submitted(line.to_vec())
    } else {
        LineEvent::Continued(line.to_vec())
    }
}

#[cfg(unix)]
fn validate_segment(bytes: Vec<u8>, draft_bytes: usize) -> Option<String> {
    let line = String::from_utf8(bytes).ok()?;
    if line.chars().any(char::is_control) || draft_bytes + line.len() > MAX_DRAFT_BYTES {
        return None;
    }
    Some(line)
}

enum ReaderEvent {
    Event(Event),
    Paste(Result<String, &'static str>),
}

pub(super) struct InputReader {
    receiver: mpsc::Receiver<io::Result<ReaderEvent>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    paste: Option<Result<String, &'static str>>,
    #[cfg(unix)]
    stdin_flags: Option<StdinFlags>,
}

impl InputReader {
    pub(super) fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::clone(&shutdown);
        #[cfg(unix)]
        let (thread, stdin_flags) = raw::start(sender, should_stop)?;
        #[cfg(not(unix))]
        let thread = std::thread::Builder::new()
            .name("arany-terminal-input".into())
            .spawn(move || {
                while !should_stop.load(Ordering::Acquire) {
                    match event::poll(INPUT_POLL_INTERVAL) {
                        Ok(false) => continue,
                        Ok(true) => {
                            if sender
                                .blocking_send(event::read().map(ReaderEvent::Event))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.blocking_send(Err(error));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            receiver,
            shutdown,
            thread: Some(thread),
            paste: None,
            #[cfg(unix)]
            stdin_flags: Some(stdin_flags),
        })
    }

    pub(super) fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.receiver.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        #[cfg(unix)]
        self.stdin_flags.take();
    }

    pub(super) async fn recv(&mut self) -> Result<TerminalInput, TerminalError> {
        self.paste = None;
        loop {
            let event = self
                .receiver
                .recv()
                .await
                .ok_or(TerminalError::InputClosed)?
                .map_err(TerminalError::Io)?;
            match event {
                ReaderEvent::Event(event) => {
                    if let Some(input) = map_input(event) {
                        return Ok(input);
                    }
                }
                ReaderEvent::Paste(text) => {
                    self.paste = Some(text);
                    return Ok(TerminalInput::Paste);
                }
            }
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop();
    }
}

fn map_input(event: Event) -> Option<TerminalInput> {
    match event {
        Event::Resize(_, _) => Some(TerminalInput::Resize),
        Event::Key(key) if key.kind == KeyEventKind::Press => map_key(key),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Moved => Some(TerminalInput::PointerMove {
                column: mouse.column,
                row: mouse.row,
            }),
            MouseEventKind::Down(MouseButton::Left) => Some(TerminalInput::PointerClick {
                column: mouse.column,
                row: mouse.row,
            }),
            MouseEventKind::ScrollUp => Some(TerminalInput::PointerScrollUp {
                column: mouse.column,
                row: mouse.row,
            }),
            MouseEventKind::ScrollDown => Some(TerminalInput::PointerScrollDown {
                column: mouse.column,
                row: mouse.row,
            }),
            _ => None,
        },
        _ => None,
    }
}

fn map_key(key: KeyEvent) -> Option<TerminalInput> {
    if matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        return Some(TerminalInput::ClipboardPaste);
    }
    if key.modifiers == KeyModifiers::CONTROL {
        return match key.code {
            KeyCode::Char('c' | 'C') => Some(TerminalInput::Interrupt),
            KeyCode::Char('d' | 'D') => Some(TerminalInput::EndOfInput),
            KeyCode::Char('k' | 'K') => Some(TerminalInput::QuickActions),
            KeyCode::Char('f' | 'F') => Some(TerminalInput::HistoryFind),
            KeyCode::Char('l' | 'L') => Some(TerminalInput::HistoryLive),
            KeyCode::Char('o' | 'O') => Some(TerminalInput::Newline),
            KeyCode::Left => Some(TerminalInput::WordLeft),
            KeyCode::Right => Some(TerminalInput::WordRight),
            KeyCode::Char('w' | 'W') => Some(TerminalInput::BackspaceWord),
            KeyCode::Char('z' | 'Z') => Some(TerminalInput::Suspend),
            _ => None,
        };
    }
    if key.modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER
            | KeyModifiers::META,
    ) {
        return None;
    }
    match key.code {
        KeyCode::Char(character) if !character.is_control() => {
            Some(TerminalInput::Character(character))
        }
        KeyCode::Backspace => Some(TerminalInput::Backspace),
        KeyCode::Delete => Some(TerminalInput::Delete),
        KeyCode::Left => Some(TerminalInput::Left),
        KeyCode::Right => Some(TerminalInput::Right),
        KeyCode::Up => Some(TerminalInput::Up),
        KeyCode::Down => Some(TerminalInput::Down),
        KeyCode::PageUp => Some(TerminalInput::PageUp),
        KeyCode::PageDown => Some(TerminalInput::PageDown),
        KeyCode::Home => Some(TerminalInput::Home),
        KeyCode::End => Some(TerminalInput::End),
        KeyCode::BackTab => Some(TerminalInput::CycleApprovalMode),
        KeyCode::Tab if key.modifiers == KeyModifiers::SHIFT => {
            Some(TerminalInput::CycleApprovalMode)
        }
        KeyCode::Tab => Some(TerminalInput::Tab),
        KeyCode::Enter if key.modifiers == KeyModifiers::SHIFT => Some(TerminalInput::Newline),
        KeyCode::Enter => Some(TerminalInput::Submit),
        KeyCode::Esc => Some(TerminalInput::Escape),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn canonical_segments_distinguish_submit_continue_and_reject_before_egress() {
        assert!(
            matches!(decode_line(b"/status\n"), LineEvent::Submitted(line) if line == b"/status")
        );
        assert!(matches!(decode_line(b"hello"), LineEvent::Continued(line) if line == b"hello"));
        assert!(matches!(decode_line(b"\n"), LineEvent::Submitted(line) if line.is_empty()));
        assert!(matches!(
            decode_line(&vec![b'x'; MAX_LINEAR_LINE_BYTES]),
            LineEvent::Continued(_)
        ));
        assert!(matches!(
            decode_line(&vec![b'x'; MAX_LINEAR_LINE_BYTES + 1]),
            LineEvent::Rejected
        ));
        assert!(matches!(decode_line(b"a\0b\n"), LineEvent::Rejected));
        assert!(validate_segment(vec![b'x'; 1000], MAX_DRAFT_BYTES - 1000).is_some());
        assert!(validate_segment(vec![b'x'; 1001], MAX_DRAFT_BYTES - 1000).is_none());
        assert!(validate_segment(b"bad\x1b[31m".to_vec(), 0).is_none());
        assert!(validate_segment(vec![0xff], 0).is_none());

        let (_, receiver) = mpsc::channel(1);
        let mut reader = LineReader {
            receiver,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
            pending: VecDeque::new(),
            draft_bytes: 0,
            stdin_flags: None,
        };
        for expected in [3000, 6000] {
            let first = reader.accept_segment(vec![b'x'; 3000], false).unwrap();
            assert_eq!(first, TerminalInput::Character('x'));
            assert_eq!(reader.draft_bytes, expected);
            assert_eq!(reader.pending.back(), Some(&TerminalInput::LineContinued));
            let mut terminal_reader = TerminalReader::Lines(reader);
            let mut delivered = vec![first];
            while !terminal_reader.paste_ready() {
                delivered.push(
                    terminal_reader
                        .recv()
                        .await
                        .expect("accepted segment event"),
                );
            }
            assert_eq!(
                delivered.len(),
                3001,
                "handoff waits for the entire accepted segment"
            );
            assert!(
                delivered[..3000]
                    .iter()
                    .all(|event| *event == TerminalInput::Character('x'))
            );
            assert_eq!(delivered.last(), Some(&TerminalInput::LineContinued));
            let TerminalReader::Lines(drained) = terminal_reader else {
                unreachable!("canonical line reader")
            };
            reader = drained;
        }
        assert_eq!(
            reader.accept_segment(vec![b'x'; 2193], true).unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(reader.draft_bytes, 6000);
        assert!(reader.pending.is_empty());
        assert_eq!(
            reader.accept_segment(vec![b'x'; 2192], true).unwrap(),
            TerminalInput::Character('x')
        );
        assert_eq!(reader.draft_bytes, 0);
        assert_eq!(reader.pending.back(), Some(&TerminalInput::Submit));
        reader.pending.clear();
        let mut terminal_reader = TerminalReader::Lines(reader);
        terminal_reader
            .restore_draft_bytes(MAX_DRAFT_BYTES)
            .unwrap();
        let TerminalReader::Lines(mut reader) = terminal_reader else {
            unreachable!("Unix line reader")
        };
        assert_eq!(
            reader.accept_segment(b"x".to_vec(), true).unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(reader.draft_bytes, MAX_DRAFT_BYTES);
        reader.reset_draft();
        reader.accept_segment(b"draft".to_vec(), true).unwrap();
        reader.reset_draft();
        assert_eq!(reader.draft_bytes, 0);
        assert!(reader.pending.is_empty());

        let mut terminal_reader = TerminalReader::Lines(reader);
        assert!(
            matches!(
                terminal_reader.recv().await,
                Err(TerminalError::InputClosed)
            ),
            "cancelled accepted suffix must not deliver its Submit to a new owner"
        );
        assert!(
            terminal_reader
                .restore_draft_bytes(MAX_DRAFT_BYTES + 1)
                .is_err()
        );
        terminal_reader.restore_draft_bytes(6000).unwrap();
        let TerminalReader::Lines(mut resumed_reader) = terminal_reader else {
            unreachable!("Unix line reader")
        };
        assert_eq!(resumed_reader.draft_bytes, 6000);
        assert_eq!(
            resumed_reader
                .accept_segment(vec![b'x'; 2193], true)
                .unwrap(),
            TerminalInput::LineRejected
        );
    }

    #[test]
    fn key_mapping_keeps_control_input_local_and_ignores_unowned_events() {
        let cases = [
            (
                Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
                Some(TerminalInput::CycleApprovalMode),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
                Some(TerminalInput::CycleApprovalMode),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
                Some(TerminalInput::Character('/')),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Some(TerminalInput::Interrupt),
            ),
            (
                Event::Key(KeyEvent::new(
                    KeyCode::Char('C'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )),
                None,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
                Some(TerminalInput::EndOfInput),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
                Some(TerminalInput::QuickActions),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
                Some(TerminalInput::HistoryFind),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
                Some(TerminalInput::HistoryLive),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
                Some(TerminalInput::Newline),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
                Some(TerminalInput::ClipboardPaste),
            ),
            (
                Event::Key(KeyEvent::new(
                    KeyCode::Char('V'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )),
                Some(TerminalInput::ClipboardPaste),
            ),
            (
                Event::Key(KeyEvent::new(
                    KeyCode::Char('v'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                )),
                None,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
                Some(TerminalInput::WordLeft),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
                Some(TerminalInput::WordRight),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
                Some(TerminalInput::BackspaceWord),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::CONTROL)),
                Some(TerminalInput::BackspaceWord),
            ),
            (
                Event::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )),
                None,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
                Some(TerminalInput::Suspend),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
                Some(TerminalInput::Backspace),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                Some(TerminalInput::Tab),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Some(TerminalInput::Submit),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
                Some(TerminalInput::Newline),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                Some(TerminalInput::Down),
            ),
            (
                Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Repeat,
                )),
                None,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
                None,
            ),
            (Event::Resize(40, 8), Some(TerminalInput::Resize)),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                Some(TerminalInput::PointerMove { column: 7, row: 9 }),
            ),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                Some(TerminalInput::PointerClick { column: 7, row: 9 }),
            ),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                Some(TerminalInput::PointerScrollDown { column: 7, row: 9 }),
            ),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Right),
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                None,
            ),
            (Event::FocusGained, None),
        ];
        for (event, expected) in cases {
            assert_eq!(map_input(event), expected);
        }

        #[cfg(unix)]
        {
            use std::time::Instant;
            let now = Instant::now();
            let decode = |bytes: &[u8]| {
                let mut decoder = raw::Decoder::default();
                bytes
                    .iter()
                    .filter_map(|&byte| decoder.feed(byte, now))
                    .collect::<Vec<_>>()
            };
            for (bytes, expected) in [
                (
                    b"\x1b[Z\x1b[9;2u".as_slice(),
                    vec![TerminalInput::CycleApprovalMode; 2],
                ),
                (
                    b"/\r\x0f\x03\x17".as_slice(),
                    vec![
                        TerminalInput::Character('/'),
                        TerminalInput::Submit,
                        TerminalInput::Newline,
                        TerminalInput::Interrupt,
                        TerminalInput::BackspaceWord,
                    ],
                ),
                (
                    "中🙂".as_bytes(),
                    vec![
                        TerminalInput::Character('中'),
                        TerminalInput::Character('🙂'),
                    ],
                ),
                (
                    b"\x1b[A\x1bOB\x1b[1;5D\x1b[1;5C".as_slice(),
                    vec![
                        TerminalInput::Up,
                        TerminalInput::Down,
                        TerminalInput::WordLeft,
                        TerminalInput::WordRight,
                    ],
                ),
                (
                    b"\x1b[H\x1bOF\x1b[3~\x1b[5~\x1b[6~".as_slice(),
                    vec![
                        TerminalInput::Home,
                        TerminalInput::End,
                        TerminalInput::Delete,
                        TerminalInput::PageUp,
                        TerminalInput::PageDown,
                    ],
                ),
                (
                    b"\x1b[13;2u\x1b[27;2;13~\x1b[99;5u".as_slice(),
                    vec![
                        TerminalInput::Newline,
                        TerminalInput::Newline,
                        TerminalInput::Interrupt,
                    ],
                ),
                (
                    b"\x16\x1b[118;5u\x1b[86;6u\x1b[27;6;118~".as_slice(),
                    vec![TerminalInput::ClipboardPaste; 4],
                ),
                (
                    b"\x1b[99;5:2u\x1b[99;5:3u\x1b[120;3u\x1b[999~\x1b[?1h\x1b[57344u".as_slice(),
                    vec![],
                ),
                (
                    b"\x1b[<35;8;10M\x1b[<0;8;10M\x1b[<64;8;10M\x1b[<65;8;10M".as_slice(),
                    vec![
                        TerminalInput::PointerMove { column: 7, row: 9 },
                        TerminalInput::PointerClick { column: 7, row: 9 },
                        TerminalInput::PointerScrollUp { column: 7, row: 9 },
                        TerminalInput::PointerScrollDown { column: 7, row: 9 },
                    ],
                ),
                (
                    b"\x1b[M (*".as_slice(),
                    vec![TerminalInput::PointerClick { column: 7, row: 9 }],
                ),
                (
                    b"\x1b[<0;0;10M\x1b[<0;65536;10M\x1b[<0;8;10m\x1b[<2;8;10M".as_slice(),
                    vec![],
                ),
                (
                    b"\x1b]52;c;/exit\r\x07\x1bP/quit\r\x1b\\x".as_slice(),
                    vec![TerminalInput::Character('x')],
                ),
            ] {
                let events = decode(bytes)
                    .into_iter()
                    .map(|event| match event {
                        ReaderEvent::Event(event) => map_input(event).expect("admitted raw key"),
                        ReaderEvent::Paste(_) => panic!("ordinary sequence became a paste"),
                    })
                    .collect::<Vec<_>>();
                assert_eq!(events, expected, "raw keyboard/pointer sequence");
            }
            for (body, expected) in [
                (
                    b"/exit\r\n/setup\x03\t".to_vec(),
                    Ok("/exit\r\n/setup\x03\t".to_owned()),
                ),
                (vec![b'x'; MAX_DRAFT_BYTES], Ok("x".repeat(MAX_DRAFT_BYTES))),
                (
                    vec![b'x'; MAX_DRAFT_BYTES + 1],
                    Err("paste is too large; paste a smaller section"),
                ),
                (vec![0xff], Err("paste is not valid UTF-8")),
                (
                    b"\x1b[20x\x1b\x1b[200~literal".to_vec(),
                    Ok("\x1b[20x\x1b\x1b[200~literal".to_owned()),
                ),
            ] {
                let mut bytes = b"\x1b[200~".to_vec();
                bytes.extend(body);
                bytes.extend_from_slice(b"\x1b[201~");
                let events = decode(&bytes);
                assert_eq!(events.len(), 1, "paste body escaped into control events");
                let ReaderEvent::Paste(result) = &events[0] else {
                    panic!("complete paste became a key");
                };
                assert!(result == &expected, "whole-record paste validation");
            }
            let mut oversized = b"\x1b[".to_vec();
            oversized.extend([b'1'; 100]);
            oversized.extend_from_slice(b";5Dx");
            assert!(matches!(
                decode(&oversized).as_slice(),
                [ReaderEvent::Event(Event::Key(KeyEvent {
                    code: KeyCode::Char('x'),
                    ..
                }))]
            ));
            let mut decoder = raw::Decoder::default();
            assert!(decoder.feed(b'\x1b', now).is_none());
            assert!(
                decoder
                    .expire(now + Duration::from_millis(99))
                    .unwrap()
                    .is_none()
            );
            let Some(ReaderEvent::Event(event)) =
                decoder.expire(now + INPUT_POLL_INTERVAL).unwrap()
            else {
                panic!("lone Escape was not delivered");
            };
            assert_eq!(map_input(event), Some(TerminalInput::Escape));
            for bytes in [
                b"\x1b[200~unfinished".as_slice(),
                b"\x1b[201",
                b"\x1b]unclosed",
                b"\xf0\x9f",
            ] {
                let mut decoder = raw::Decoder::default();
                for &byte in bytes {
                    assert!(decoder.feed(byte, now).is_none());
                }
                assert!(
                    decoder
                        .expire(now + Duration::from_secs(9))
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    decoder
                        .expire(now + Duration::from_secs(10))
                        .err()
                        .unwrap()
                        .kind(),
                    io::ErrorKind::TimedOut
                );
            }
        }
    }
}
