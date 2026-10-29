use super::composer::{MAX_DRAFT_BYTES, MAX_PASTE_BYTES, MAX_PRIMARY_DRAFT_BYTES};
use super::{NewlineShortcut, TerminalError};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};
use rustix::fs::{Mode, OFlags, fstat, open};
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::oneshot;

// The reader's admission epoch when it read the bytes behind this result.
type Observed<T> = (u64, io::Result<T>);

const INPUT_QUEUE_CAPACITY: usize = 32;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const KEYBOARD_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
mod raw;
// Stay below canonical buffers so discarded suffixes cannot become accepted input.
#[cfg(target_os = "linux")]
pub(super) const MAX_LINEAR_LINE_BYTES: usize = 4094;
#[cfg(target_os = "macos")]
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
    IgnoredKey,
    Paste,
    ClipboardPaste,
    Newline,
    ModifiedEnter(NewlineShortcut),
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
    HalfPageUp,
    Home,
    End,
    Tab,
    CycleApprovalMode,
    QuickActions,
    HistoryFind,
    HistoryLive,
    Transcript,
    AgentPrevious,
    AgentNext,
    Submit,
    Escape,
    Interrupt,
    EndOfInput,
    Suspend,
    Shutdown(ShutdownSignal),
    Resize,
    CopySelection,
    PanelSelectionChanged,
    PointerMove { column: u16, row: u16 },
    PointerClick { column: u16, row: u16, clicks: u8 },
    PointerDrag { column: u16, row: u16 },
    PointerRelease { column: u16, row: u16 },
    PointerScrollUp { column: u16, row: u16 },
    PointerScrollDown { column: u16, row: u16 },
    LineRejected,
    LineContinued,
}

impl TerminalInput {
    // A digit picks the matching 1-based row of a numbered dialog.
    #[must_use]
    pub fn choice_index(self, count: usize) -> Option<usize> {
        let Self::Character(character) = self else {
            return None;
        };
        let index = usize::try_from(character.to_digit(10)?.checked_sub(1)?).ok()?;
        (index < count).then_some(index)
    }
}

pub(super) enum TerminalReader {
    Keys(InputReader),
    Lines(LineReader),
    #[cfg(test)]
    Test(std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>),
}

impl TerminalReader {
    pub(super) fn probe_keyboard(
        &self,
        output: &mut impl io::Write,
        graphics: bool,
    ) -> io::Result<KeyboardSupport> {
        match self {
            Self::Keys(reader) => reader.queries.probe_keyboard_within(
                output,
                KEYBOARD_PROBE_TIMEOUT,
                graphics,
                graphics
                    .then(super::graphics::sixel::CellPixels::from_terminal)
                    .flatten(),
            ),
            Self::Lines(_) => Ok(KeyboardSupport::Legacy),
            #[cfg(test)]
            Self::Test(_) => Ok(KeyboardSupport::Legacy),
        }
    }

    pub(super) fn kitty_graphics_supported(&self) -> bool {
        matches!(self, Self::Keys(reader) if reader.queries.graphics_supported())
    }

    pub(super) fn sixel_cell_pixels(&self) -> Option<super::graphics::sixel::CellPixels> {
        let Self::Keys(reader) = self else {
            return None;
        };
        reader.queries.sixel_cell_pixels()
    }

    pub(super) fn set_chat_input(&mut self, chat: bool, child: bool, primary_capacity: usize) {
        if let Self::Lines(reader) = self {
            let leaving_child = reader.child && !child;
            reader.chat = chat;
            reader.child = child;
            if child {
                reader.remaining_bytes = reader.remaining_bytes.min(MAX_DRAFT_BYTES);
            } else if leaving_child {
                reader.remaining_bytes = primary_capacity;
            }
        }
    }

    pub(super) fn cursor_query(&self) -> Option<TerminalQueries> {
        match self {
            Self::Keys(reader) => Some(reader.queries.clone()),
            Self::Lines(_) => None,
            #[cfg(test)]
            Self::Test(_) => None,
        }
    }
    pub(super) fn start(linear: bool) -> io::Result<Self> {
        if linear {
            return LineReader::start().map(Self::Lines);
        }
        let _ = linear;
        InputReader::start().map(Self::Keys)
    }

    pub(super) async fn recv(&mut self) -> Result<TerminalInput, TerminalError> {
        match self {
            Self::Keys(reader) => reader.recv().await,
            Self::Lines(reader) => reader.recv().await,
            #[cfg(test)]
            Self::Test(_) => Err(TerminalError::Io(io::Error::other("test reader"))),
        }
    }

    pub(super) fn stop(&mut self) {
        match self {
            Self::Keys(reader) => reader.stop(),
            Self::Lines(reader) => reader.stop(),
            #[cfg(test)]
            Self::Test(steps) => steps.borrow_mut().push("reader"),
        }
    }

    pub(super) async fn discard_typeahead(&mut self) -> Result<(), TerminalError> {
        match self {
            Self::Keys(reader) => {
                reader.epoch = reader.admission.barrier().await?;
                reader.paste = None;
            }
            Self::Lines(reader) => {
                reader.epoch = reader.admission.barrier().await?;
                reader.reset_draft();
            }
            #[cfg(test)]
            Self::Test(_) => {}
        }
        Ok(())
    }

    pub(super) fn reset_draft(&mut self) {
        if let Self::Lines(reader) = self {
            reader.reset_draft();
        }
    }

    pub(super) fn take_paste(&mut self) -> Option<Result<String, &'static str>> {
        match self {
            Self::Keys(reader) => reader.paste.take(),
            Self::Lines(_) => None,
            #[cfg(test)]
            Self::Test(_) => None,
        }
    }

    pub(super) fn paste_ready(&self) -> bool {
        match self {
            Self::Keys(_) => true,
            Self::Lines(reader) => reader.pending.is_empty(),
            #[cfg(test)]
            Self::Test(_) => true,
        }
    }

    pub(super) fn restore_input_capacity(&mut self, capacity: usize) -> io::Result<()> {
        {
            if capacity > MAX_PRIMARY_DRAFT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "input capacity too large",
                ));
            }
            if let Self::Lines(reader) = self {
                reader.remaining_bytes = capacity;
            }
        }
        Ok(())
    }
}

enum LineEvent {
    Submitted(Vec<u8>),
    Continued(Vec<u8>),
    Rejected,
    EndOfInput,
    Resize,
}

pub(super) struct LineReader {
    receiver: mpsc::Receiver<Observed<LineEvent>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pending: VecDeque<TerminalInput>,
    remaining_bytes: usize,
    chat: bool,
    child: bool,
    epoch: u64,
    admission: InputAdmission,
}

fn terminal_input() -> io::Result<File> {
    let stdin = io::stdin();
    let admitted = fstat(&stdin)?;
    let name = rustix::termios::ttyname(&stdin, Vec::new())?;
    // A dup shares O_NONBLOCK with inherited terminal output.
    let input = open(
        name.as_c_str(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NOCTTY,
        Mode::empty(),
    )?;
    let opened = fstat(&input)?;
    if (opened.st_dev, opened.st_ino, opened.st_rdev)
        != (admitted.st_dev, admitted.st_ino, admitted.st_rdev)
        || !rustix::termios::isatty(&input)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal input identity changed",
        ));
    }
    Ok(File::from(input))
}

impl LineReader {
    fn start() -> io::Result<Self> {
        let mut input = terminal_input()?;
        let mut poll = Poll::new()?;
        let fd = input.as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
        let (sender, receiver) = mpsc::channel(4);
        let (admission, mut requests) = InputAdmission::new(&poll)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("arany-terminal-line-input".into())
            .spawn(move || {
                if let Err(error) =
                    read_lines(&mut poll, &mut input, &sender, &should_stop, &mut requests)
                {
                    let _ = sender.blocking_send((0, Err(error)));
                }
            })?;
        Ok(Self {
            receiver,
            shutdown,
            thread: Some(thread),
            pending: VecDeque::new(),
            remaining_bytes: MAX_PRIMARY_DRAFT_BYTES,
            chat: false,
            child: false,
            epoch: 0,
            admission,
        })
    }

    async fn recv(&mut self) -> Result<TerminalInput, TerminalError> {
        if let Some(input) = self.pending.pop_front() {
            return Ok(input);
        }
        let (observed, event) = self
            .receiver
            .recv()
            .await
            .ok_or(TerminalError::InputClosed)?;
        let event = event.map_err(TerminalError::Io)?;
        if observed < self.epoch && !matches!(event, LineEvent::EndOfInput | LineEvent::Resize) {
            return Ok(TerminalInput::LineRejected);
        }
        match event {
            LineEvent::Submitted(bytes) => self.accept_segment(bytes, true),
            LineEvent::Continued(bytes) => self.accept_segment(bytes, false),
            LineEvent::Rejected => Ok(TerminalInput::LineRejected),
            LineEvent::EndOfInput => Ok(TerminalInput::EndOfInput),
            LineEvent::Resize => Ok(TerminalInput::Resize),
        }
    }

    fn accept_segment(
        &mut self,
        bytes: Vec<u8>,
        submitted: bool,
    ) -> Result<TerminalInput, TerminalError> {
        let Some(mut line) = validate_segment(bytes, self.remaining_bytes) else {
            return Ok(TerminalInput::LineRejected);
        };
        let continued = submitted
            && self.chat
            && line.ends_with('\\')
            && (!self.child
                || self.remaining_bytes < MAX_DRAFT_BYTES
                || line.starts_with("/message "));
        if continued {
            line.pop();
            line.push('\n');
        }
        let submitted = submitted && !continued;
        self.remaining_bytes = if submitted {
            if self.child {
                MAX_DRAFT_BYTES
            } else {
                MAX_PRIMARY_DRAFT_BYTES
            }
        } else {
            self.remaining_bytes - line.len()
        };
        self.pending.extend(line.chars().map(|character| {
            if character == '\n' {
                TerminalInput::Newline
            } else {
                TerminalInput::Character(character)
            }
        }));
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
        self.remaining_bytes = if self.child {
            MAX_DRAFT_BYTES
        } else {
            MAX_PRIMARY_DRAFT_BYTES
        };
        self.pending.clear();
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.receiver.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LineReader {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) struct InputAdmission {
    sender: mpsc::Sender<oneshot::Sender<u64>>,
    wake: mio::Waker,
}

impl InputAdmission {
    fn new(poll: &Poll) -> io::Result<(Self, mpsc::Receiver<oneshot::Sender<u64>>)> {
        let (sender, requests) = mpsc::channel(1);
        let wake = mio::Waker::new(poll.registry(), Token(1))?;
        Ok((Self { sender, wake }, requests))
    }

    async fn barrier(&self) -> Result<u64, TerminalError> {
        let (reply, acknowledged) = oneshot::channel();
        tokio::time::timeout(Duration::from_secs(1), async {
            self.sender
                .send(reply)
                .await
                .map_err(|_| TerminalError::InputClosed)?;
            self.wake.wake()?;
            acknowledged.await.map_err(|_| TerminalError::InputClosed)
        })
        .await
        .map_err(|_| {
            TerminalError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal input admission timed out",
            ))
        })?
    }
}

fn admit_input(
    input: &File,
    requests: &mut mpsc::Receiver<oneshot::Sender<u64>>,
    epoch: &mut u64,
    reset: impl FnOnce(),
) -> io::Result<()> {
    let Ok(reply) = requests.try_recv() else {
        return Ok(());
    };
    reset();
    rustix::termios::tcflush(input, rustix::termios::QueueSelector::IFlush)?;
    *epoch += 1;
    let _ = reply.send(*epoch);
    Ok(())
}

fn read_lines(
    poll: &mut Poll,
    input: &mut File,
    sender: &mpsc::Sender<Observed<LineEvent>>,
    shutdown: &AtomicBool,
    requests: &mut mpsc::Receiver<oneshot::Sender<u64>>,
) -> io::Result<()> {
    let fd = input.as_raw_fd();
    let mut events = Events::with_capacity(4);
    let mut bytes = [0; 8194];
    let mut pending = None;
    let mut resize_pending = false;
    let mut epoch = 0;
    let mut geometry = crossterm::terminal::size().ok();
    // Keep paste framing across admission; a flushed terminator must fail closed.
    let mut decoder = LineDecoder::default();
    while !shutdown.load(Ordering::Acquire) {
        decoder.expire(Instant::now())?;
        admit_input(input, requests, &mut epoch, || pending = None)?;
        let resized = crossterm::terminal::size().ok();
        if resized != geometry {
            geometry = resized;
            if pending.is_some() {
                resize_pending = true;
            } else {
                pending = Some((epoch, Ok(LineEvent::Resize)));
            }
        }
        if let Some(event) = pending.take() {
            match sender.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(event)) => {
                    pending = Some(event);
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(_) => return Ok(()),
            }
        }
        if resize_pending {
            pending = Some((epoch, Ok(LineEvent::Resize)));
            resize_pending = false;
            continue;
        }
        match poll.poll(&mut events, Some(INPUT_POLL_INTERVAL)) {
            Ok(()) if events.is_empty() => continue,
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        admit_input(input, requests, &mut epoch, || pending = None)?;
        let event = match input.read(&mut bytes) {
            Ok(0) if decoder.started.is_some() => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete terminal paste",
                ));
            }
            Ok(0) => LineEvent::EndOfInput,
            Ok(len) => decoder.decode(&bytes[..len], Instant::now()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                poll.registry()
                    .reregister(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        poll.registry()
            .reregister(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
        pending = Some((epoch, Ok(event)));
    }
    Ok(())
}

#[derive(Default)]
struct LineDecoder {
    started: Option<Instant>,
    draining: bool,
    delimiter: usize,
}

impl LineDecoder {
    fn expire(&self, now: Instant) -> io::Result<()> {
        if self
            .started
            .is_some_and(|started| now.duration_since(started) >= Duration::from_secs(10))
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "incomplete terminal paste",
            ));
        }
        Ok(())
    }

    fn decode(&mut self, bytes: &[u8], now: Instant) -> LineEvent {
        let mut rejected = self.started.is_some();
        for &byte in bytes {
            let marker = if self.draining {
                b"\x1b[201~"
            } else {
                b"\x1b[200~"
            };
            if byte == marker[self.delimiter] {
                self.started.get_or_insert(now);
                self.delimiter += 1;
                rejected = true;
                if self.delimiter == marker.len() {
                    self.delimiter = 0;
                    self.draining = !self.draining;
                    if !self.draining {
                        self.started = None;
                    }
                }
            } else {
                self.delimiter = usize::from(byte == marker[0]);
                if !self.draining {
                    self.started = (self.delimiter > 0).then_some(now);
                }
            }
        }
        if rejected || self.draining {
            LineEvent::Rejected
        } else {
            decode_line(bytes)
        }
    }
}

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

fn validate_segment(bytes: Vec<u8>, remaining_bytes: usize) -> Option<String> {
    let line = String::from_utf8(bytes).ok()?;
    if line.chars().any(char::is_control) || line.len() > remaining_bytes {
        return None;
    }
    Some(line)
}

enum ReaderEvent {
    Event(Event),
    Pointer(TerminalInput),
    Paste(Result<String, &'static str>),
    CursorPosition(u16, u16),
    KeyboardReply(KeyboardReply),
    GraphicsReply,
    CellPixels(super::graphics::sixel::CellPixels),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeyboardSupport {
    Legacy,
    Disambiguated,
}

#[derive(Clone)]
enum KeyboardReply {
    Flags,
    DeviceAttributes(Vec<u32>),
}

#[derive(Default, PartialEq)]
enum KeyboardProbe {
    #[default]
    Idle,
    Waiting(Instant),
    Unsupported,
}

#[derive(Clone, Default)]
pub(super) struct TerminalQueries(Arc<(Mutex<QueryState>, Condvar)>);

#[derive(Default)]
struct QueryState {
    keyboard: KeyboardProbe,
    keyboard_supported: bool,
    graphics: bool,
    kitty_graphics: bool,
    device_attributes: Option<Vec<u32>>,
    cell_pixels: Option<super::graphics::sixel::CellPixels>,
    waiting: bool,
    position: Option<(u16, u16)>,
    closed: bool,
}

impl TerminalQueries {
    fn probe_keyboard_within(
        &self,
        output: &mut impl io::Write,
        timeout: Duration,
        graphics: bool,
        cell_pixels: Option<super::graphics::sixel::CellPixels>,
    ) -> io::Result<KeyboardSupport> {
        let (state, ready) = &*self.0;
        let mut state = state
            .lock()
            .map_err(|_| io::Error::other("input owner failed"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input closed",
            ));
        }
        let deadline = Instant::now() + timeout;
        state.keyboard = KeyboardProbe::Waiting(deadline);
        state.keyboard_supported = false;
        state.graphics = graphics;
        state.kitty_graphics = false;
        state.device_attributes = None;
        state.cell_pixels = cell_pixels.filter(|_| graphics);
        let query = if graphics {
            super::KEYBOARD_QUERY
        } else {
            super::KEYBOARD_ONLY_QUERY
        };
        let mut write_query = || {
            if graphics && state.cell_pixels.is_none() {
                output.write_all(b"\x1b[16t")?;
            }
            output.write_all(query).and_then(|()| output.flush())
        };
        if let Err(error) = write_query() {
            state.keyboard = KeyboardProbe::Idle;
            return Err(error);
        }
        let (mut state, _) = ready
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |state| matches!(state.keyboard, KeyboardProbe::Waiting(_)) && !state.closed,
            )
            .map_err(|_| io::Error::other("input owner failed"))?;
        let support = if state.keyboard_supported {
            KeyboardSupport::Disambiguated
        } else {
            KeyboardSupport::Legacy
        };
        state.keyboard = KeyboardProbe::Idle;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input closed",
            ));
        }
        Ok(support)
    }

    fn keyboard_reply(&self, reply: KeyboardReply) {
        let (state, ready) = &*self.0;
        if let Ok(mut state) = state.lock()
            && let KeyboardProbe::Waiting(deadline) = state.keyboard
        {
            if Instant::now() < deadline {
                match reply {
                    KeyboardReply::Flags => state.keyboard_supported = true,
                    KeyboardReply::DeviceAttributes(attributes) => {
                        state.device_attributes = Some(attributes);
                        state.keyboard = KeyboardProbe::Unsupported;
                        ready.notify_one();
                    }
                }
            } else {
                state.keyboard = KeyboardProbe::Unsupported;
                ready.notify_one();
            }
        }
    }

    fn graphics_reply(&self) {
        if let Ok(mut state) = self.0.0.lock()
            && state.graphics
            && matches!(state.keyboard, KeyboardProbe::Waiting(_))
        {
            state.kitty_graphics = true;
        }
    }

    fn sixel_cell_pixels(&self) -> Option<super::graphics::sixel::CellPixels> {
        let state = self.0.0.lock().ok()?;
        (state.graphics
            && state
                .device_attributes
                .as_ref()?
                .iter()
                .skip(1)
                .any(|attribute| *attribute == 4))
        .then_some(state.cell_pixels)
        .flatten()
    }

    fn cell_pixels_reply(&self, cell: super::graphics::sixel::CellPixels) {
        if let Ok(mut state) = self.0.0.lock()
            && state.graphics
            && let KeyboardProbe::Waiting(deadline) = state.keyboard
            && Instant::now() < deadline
        {
            state.cell_pixels = Some(cell);
        }
    }

    pub(super) fn graphics_supported(&self) -> bool {
        self.0
            .0
            .lock()
            .is_ok_and(|state| state.kitty_graphics && state.device_attributes.is_some())
    }

    fn waiting(&self) -> bool {
        let (state, _) = &*self.0;
        state.lock().is_ok_and(|state| {
            (state.waiting && state.position.is_none())
                || matches!(state.keyboard, KeyboardProbe::Waiting(_))
        })
    }

    pub(super) fn query(&self, output: &mut impl io::Write) -> io::Result<(u16, u16)> {
        let (state, ready) = &*self.0;
        let mut state = state
            .lock()
            .map_err(|_| io::Error::other("cursor owner failed"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input closed",
            ));
        }
        state.waiting = true;
        state.position = None;
        if let Err(error) = output.write_all(b"\x1b[6n").and_then(|()| output.flush()) {
            state.waiting = false;
            return Err(error);
        }
        let (mut state, _) = ready
            .wait_timeout_while(state, Duration::from_secs(1), |state| {
                state.position.is_none() && !state.closed
            })
            .map_err(|_| io::Error::other("cursor owner failed"))?;
        state.waiting = false;
        state.position.take().ok_or_else(|| {
            io::Error::new(
                if state.closed {
                    io::ErrorKind::UnexpectedEof
                } else {
                    io::ErrorKind::TimedOut
                },
                "terminal cursor response unavailable",
            )
        })
    }

    pub(super) fn respond(&self, column: u16, row: u16) {
        let (state, ready) = &*self.0;
        if let Ok(mut state) = state.lock()
            && state.waiting
        {
            state.position = Some((column, row));
            ready.notify_one();
        }
    }

    pub(super) fn close(&self) {
        let (state, ready) = &*self.0;
        if let Ok(mut state) = state.lock() {
            state.closed = true;
            ready.notify_one();
        }
    }
}

pub(super) struct InputReader {
    receiver: mpsc::Receiver<Observed<ReaderEvent>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    paste: Option<Result<String, &'static str>>,
    queries: TerminalQueries,
    epoch: u64,
    admission: InputAdmission,
}

impl InputReader {
    pub(super) fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::clone(&shutdown);
        let queries = TerminalQueries::default();
        let (thread, admission) = raw::start(sender, should_stop, queries.clone())?;
        Ok(Self {
            receiver,
            shutdown,
            thread: Some(thread),
            paste: None,
            queries,
            epoch: 0,
            admission,
        })
    }

    pub(super) fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.queries.close();
        self.receiver.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub(super) async fn recv(&mut self) -> Result<TerminalInput, TerminalError> {
        self.paste = None;
        loop {
            let (observed, event) = self
                .receiver
                .recv()
                .await
                .ok_or(TerminalError::InputClosed)?;
            let event = event.map_err(TerminalError::Io)?;
            let stale = observed < self.epoch;
            match event {
                ReaderEvent::Event(event) => {
                    if let Some(input) = map_input(event) {
                        if stale
                            && !matches!(
                                input,
                                TerminalInput::Interrupt
                                    | TerminalInput::Shutdown(_)
                                    | TerminalInput::EndOfInput
                                    | TerminalInput::Suspend
                                    | TerminalInput::Resize
                            )
                        {
                            continue;
                        }
                        return Ok(input);
                    }
                }
                ReaderEvent::Pointer(_) if stale => continue,
                ReaderEvent::Pointer(input) => return Ok(input),
                ReaderEvent::Paste(_) if stale => continue,
                ReaderEvent::Paste(text) => {
                    self.paste = Some(text);
                    return Ok(TerminalInput::Paste);
                }
                ReaderEvent::CursorPosition(_, _) => {}
                ReaderEvent::KeyboardReply(_)
                | ReaderEvent::GraphicsReply
                | ReaderEvent::CellPixels(_) => {}
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
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            Some(map_key(key).unwrap_or(TerminalInput::IgnoredKey))
        }
        Event::FocusGained | Event::FocusLost => Some(TerminalInput::IgnoredKey),
        Event::Mouse(mouse) => pointer_input(mouse.kind, mouse.column, mouse.row, 1),
        _ => None,
    }
}

// Only the left button selects; other buttons and unowned motion are ignored.
fn pointer_input(kind: MouseEventKind, column: u16, row: u16, clicks: u8) -> Option<TerminalInput> {
    Some(match kind {
        MouseEventKind::Moved => TerminalInput::PointerMove { column, row },
        MouseEventKind::Down(MouseButton::Left) => TerminalInput::PointerClick {
            column,
            row,
            clicks,
        },
        MouseEventKind::Drag(MouseButton::Left) => TerminalInput::PointerDrag { column, row },
        MouseEventKind::Up(MouseButton::Left) => TerminalInput::PointerRelease { column, row },
        MouseEventKind::ScrollUp => TerminalInput::PointerScrollUp { column, row },
        MouseEventKind::ScrollDown => TerminalInput::PointerScrollDown { column, row },
        _ => return None,
    })
}

fn map_key(key: KeyEvent) -> Option<TerminalInput> {
    if key.code == KeyCode::Enter && key.modifiers != KeyModifiers::NONE {
        return match key.modifiers {
            KeyModifiers::SHIFT => Some(TerminalInput::ModifiedEnter(NewlineShortcut::ShiftEnter)),
            KeyModifiers::ALT => Some(TerminalInput::ModifiedEnter(NewlineShortcut::AltEnter)),
            KeyModifiers::CONTROL => Some(TerminalInput::ModifiedEnter(NewlineShortcut::CtrlEnter)),
            _ => None,
        };
    }
    if matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        return Some(TerminalInput::ClipboardPaste);
    }
    if matches!(key.code, KeyCode::Char('c' | 'C'))
        && key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT
    {
        return Some(TerminalInput::CopySelection);
    }
    if key.modifiers == KeyModifiers::CONTROL {
        return match key.code {
            KeyCode::Char('c' | 'C') => Some(TerminalInput::Interrupt),
            KeyCode::Char('d' | 'D') => Some(TerminalInput::EndOfInput),
            KeyCode::Char('u' | 'U') => Some(TerminalInput::HalfPageUp),
            KeyCode::Char('k' | 'K') => Some(TerminalInput::QuickActions),
            KeyCode::Char('f' | 'F') => Some(TerminalInput::HistoryFind),
            KeyCode::Char('l' | 'L') => Some(TerminalInput::HistoryLive),
            KeyCode::Char('o' | 'O') => Some(TerminalInput::Transcript),
            KeyCode::Char('j' | 'J') => Some(TerminalInput::Newline),
            KeyCode::Left => Some(TerminalInput::WordLeft),
            KeyCode::Right => Some(TerminalInput::WordRight),
            KeyCode::Char('w' | 'W') => Some(TerminalInput::BackspaceWord),
            KeyCode::Char('z' | 'Z') => Some(TerminalInput::Suspend),
            _ => None,
        };
    }
    if key.modifiers == KeyModifiers::ALT {
        return match key.code {
            KeyCode::Left => Some(TerminalInput::AgentPrevious),
            KeyCode::Right => Some(TerminalInput::AgentNext),
            // Option+Left/Right arrive as `ESC b`/`ESC f` without enhanced key reporting, as in Codex.
            #[cfg(target_os = "macos")]
            KeyCode::Char('b') => Some(TerminalInput::AgentPrevious),
            #[cfg(target_os = "macos")]
            KeyCode::Char('f') => Some(TerminalInput::AgentNext),
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
        KeyCode::Enter => Some(TerminalInput::Submit),
        KeyCode::Esc => Some(TerminalInput::Escape),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_probe_requires_both_exact_graphics_reply_and_da1() {
        let queries = TerminalQueries::default();
        {
            let mut state = queries.0.0.lock().unwrap();
            state.graphics = true;
            state.keyboard = KeyboardProbe::Waiting(Instant::now() + Duration::from_secs(1));
        }
        queries.graphics_reply();
        queries.keyboard_reply(KeyboardReply::DeviceAttributes(vec![1, 4]));
        assert!(queries.graphics_supported());
        assert_eq!(
            queries.0.0.lock().unwrap().device_attributes,
            Some(vec![1, 4])
        );

        let mut query = Vec::new();
        assert_eq!(
            queries
                .probe_keyboard_within(&mut query, Duration::ZERO, true, None)
                .unwrap(),
            KeyboardSupport::Legacy
        );
        let expected = [b"\x1b[16t".as_slice(), super::super::KEYBOARD_QUERY].concat();
        assert_eq!(query, expected);
    }

    #[test]
    fn leaving_child_chat_restores_saved_primary_capacity() {
        let (_, receiver) = mpsc::channel(1);
        let mut reader = TerminalReader::Lines(LineReader {
            receiver,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
            pending: VecDeque::new(),
            remaining_bytes: MAX_DRAFT_BYTES,
            chat: true,
            child: true,
            epoch: 0,
            admission: InputAdmission::new(&Poll::new().unwrap()).unwrap().0,
        });

        reader.set_chat_input(false, false, 7);

        let TerminalReader::Lines(mut reader) = reader else {
            unreachable!("Unix line reader")
        };
        assert_eq!(reader.remaining_bytes, 7);
        assert_eq!(
            reader.accept_segment(b"12345678".to_vec(), true).unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(
            reader.accept_segment(b"1234567".to_vec(), true).unwrap(),
            TerminalInput::Character('1')
        );
        assert_eq!(reader.pending.back(), Some(&TerminalInput::Submit));
    }

    #[tokio::test]
    async fn approval_typeahead_discards_old_enter_and_digits_but_preserves_cancellation() {
        let (sender, receiver) = mpsc::channel(8);
        let old = 0;
        let mut reader = TerminalReader::Keys(InputReader {
            receiver,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
            paste: None,
            queries: TerminalQueries::default(),
            epoch: 1,
            admission: InputAdmission::new(&Poll::new().unwrap()).unwrap().0,
        });
        for code in [KeyCode::Enter, KeyCode::Char('1')] {
            sender
                .send((
                    old,
                    Ok(ReaderEvent::Event(Event::Key(KeyEvent::new(
                        code,
                        KeyModifiers::NONE,
                    )))),
                ))
                .await
                .unwrap();
        }
        sender
            .send((
                old,
                Ok(ReaderEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )))),
            ))
            .await
            .unwrap();
        sender
            .send((
                1,
                Ok(ReaderEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('1'),
                    KeyModifiers::NONE,
                )))),
            ))
            .await
            .unwrap();
        assert_eq!(reader.recv().await.unwrap(), TerminalInput::Interrupt);
        assert_eq!(reader.recv().await.unwrap(), TerminalInput::Character('1'));

        {
            let (sender, receiver) = mpsc::channel(4);
            let mut reader = TerminalReader::Lines(LineReader {
                receiver,
                shutdown: Arc::new(AtomicBool::new(false)),
                thread: None,
                pending: VecDeque::new(),
                remaining_bytes: MAX_PRIMARY_DRAFT_BYTES,
                chat: false,
                child: false,
                epoch: 1,
                admission: InputAdmission::new(&Poll::new().unwrap()).unwrap().0,
            });
            sender
                .send((old, Ok(LineEvent::Submitted(b"1".to_vec()))))
                .await
                .unwrap();
            sender
                .send((1, Ok(LineEvent::Submitted(b"no".to_vec()))))
                .await
                .unwrap();
            assert_eq!(reader.recv().await.unwrap(), TerminalInput::LineRejected);
            for expected in [
                TerminalInput::Character('n'),
                TerminalInput::Character('o'),
                TerminalInput::Submit,
            ] {
                assert_eq!(reader.recv().await.unwrap(), expected);
            }
        }
    }

    #[test]
    fn canonical_text_beside_chips_rejects_whole_segments_at_either_limit() {
        use crate::terminal::{Composer, ComposerEdit};
        for (paste_bytes, prefix_bytes, capacity, continued) in [
            (9600, 0, MAX_PRIMARY_DRAFT_BYTES - 9600, false),
            (9600, 0, MAX_PRIMARY_DRAFT_BYTES - 9600, true),
            (9600, MAX_PRIMARY_DRAFT_BYTES - 9600 - 5, 5, false),
            (9600, MAX_PRIMARY_DRAFT_BYTES - 9600 - 5, 5, true),
            (MAX_PASTE_BYTES - 5, 0, 5, false),
            (MAX_PASTE_BYTES - 5, 0, 5, true),
        ] {
            let mut composer = Composer::default();
            composer.insert_paste(&"p".repeat(paste_bytes)).unwrap();
            if prefix_bytes > 0 {
                assert_eq!(
                    composer.insert_literal(&"t".repeat(prefix_bytes)),
                    Ok(ComposerEdit::Changed)
                );
            }
            assert_eq!(composer.input_capacity(), capacity);
            let (_, receiver) = mpsc::channel(1);
            let mut reader = LineReader {
                receiver,
                shutdown: Arc::new(AtomicBool::new(false)),
                thread: None,
                pending: VecDeque::new(),
                remaining_bytes: composer.input_capacity(),
                chat: true,
                child: false,
                epoch: 0,
                admission: InputAdmission::new(&Poll::new().unwrap()).unwrap().0,
            };
            let before = composer.text().to_owned();
            let suffix = if continued {
                format!("{}\\", "x".repeat(capacity - 1))
            } else {
                format!("{}🙂", "x".repeat(capacity - 4))
            };
            assert_eq!(
                reader
                    .accept_segment(format!("{suffix}x").into_bytes(), true)
                    .unwrap(),
                TerminalInput::LineRejected
            );
            assert!(reader.pending.is_empty());
            assert_eq!(reader.remaining_bytes, capacity);
            let first = reader
                .accept_segment(suffix.as_bytes().to_vec(), true)
                .unwrap();
            for event in std::iter::once(first).chain(reader.pending.drain(..)) {
                if !matches!(event, TerminalInput::Submit | TerminalInput::LineContinued) {
                    assert_eq!(composer.apply(event), ComposerEdit::Changed);
                }
            }
            let expected = if continued {
                format!("{}\n", &suffix[..suffix.len() - 1])
            } else {
                suffix
            };
            assert_eq!(composer.text(), format!("{before}{expected}"));
            reader.remaining_bytes = composer.input_capacity();
            assert_eq!(
                reader.accept_segment(Vec::new(), true).unwrap(),
                TerminalInput::Submit
            );
            assert_eq!(composer.input_capacity(), 0);
        }
    }

    #[tokio::test]
    async fn canonical_segments_distinguish_submit_continue_and_reject_before_egress() {
        let now = Instant::now();
        for parts in [
            vec![
                b"\x1b[200~\\\n".as_slice(),
                b"/exit\n",
                b"1\n",
                b"\x1b[201~\n",
            ],
            vec![b"\x1b[20".as_slice(), b"0~first\n", b"\x1b[20", b"1~\n"],
            vec![b"\x1b[200~literal\x1b[13;2u\r\n\x1b[201~\n".as_slice()],
        ] {
            let mut decoder = LineDecoder::default();
            for part in parts {
                assert!(matches!(decoder.decode(part, now), LineEvent::Rejected));
            }
            assert!(decoder.started.is_none());
            assert!(
                matches!(decoder.decode(b"safe\n", now), LineEvent::Submitted(line) if line == b"safe")
            );
        }
        let mut decoder = LineDecoder::default();
        assert!(matches!(
            decoder.decode(b"\x1b[200~unfinished\n", now),
            LineEvent::Rejected
        ));
        assert!(decoder.expire(now + Duration::from_secs(9)).is_ok());
        assert_eq!(
            decoder
                .expire(now + Duration::from_secs(10))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
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
        assert!(validate_segment(vec![b'x'; 1000], 1000).is_some());
        assert!(validate_segment(vec![b'x'; 1001], 1000).is_none());
        assert!(validate_segment(b"bad\x1b[31m".to_vec(), 0).is_none());
        assert!(validate_segment(vec![0xff], 0).is_none());

        let (_, receiver) = mpsc::channel(1);
        let mut reader = LineReader {
            receiver,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
            pending: VecDeque::new(),
            remaining_bytes: MAX_PRIMARY_DRAFT_BYTES,
            chat: false,
            child: false,
            epoch: 0,
            admission: InputAdmission::new(&Poll::new().unwrap()).unwrap().0,
        };
        for (chat, child, segment, expected, submitted) in [
            (true, false, "a\\", "a\n", false),
            (true, false, "\\\\", "\\\n", false),
            (true, false, "/paste", "/paste", true),
            (false, false, "answer\\", "answer\\", true),
            (true, true, "1\\", "1\\", true),
            (true, true, "/message a\\", "/message a\n", false),
        ] {
            reader.reset_draft();
            reader.chat = chat;
            reader.child = child;
            let first = reader
                .accept_segment(segment.as_bytes().to_vec(), true)
                .unwrap();
            let mut events = vec![first];
            events.extend(reader.pending.drain(..));
            assert_eq!(
                events.pop(),
                Some(if submitted {
                    TerminalInput::Submit
                } else {
                    TerminalInput::LineContinued
                })
            );
            let text: String = events
                .into_iter()
                .map(|event| match event {
                    TerminalInput::Character(character) => character,
                    TerminalInput::Newline => '\n',
                    _ => panic!("segment became an action"),
                })
                .collect();
            assert_eq!(text, expected);
            let limit = if child {
                MAX_DRAFT_BYTES
            } else {
                MAX_PRIMARY_DRAFT_BYTES
            };
            assert_eq!(
                reader.remaining_bytes,
                if submitted {
                    limit
                } else {
                    limit - expected.len()
                }
            );
        }
        reader.chat = false;
        reader.child = true;
        reader.reset_draft();
        let child_boundary = MAX_DRAFT_BYTES % MAX_LINEAR_LINE_BYTES;
        while reader.remaining_bytes > child_boundary {
            let segment_bytes =
                (reader.remaining_bytes - child_boundary).min(MAX_LINEAR_LINE_BYTES);
            reader
                .accept_segment(vec![b'x'; segment_bytes], false)
                .unwrap();
            reader.pending.clear();
        }
        assert_eq!(reader.remaining_bytes, child_boundary);
        assert_eq!(
            reader
                .accept_segment(vec![b'x'; child_boundary + 1], true)
                .unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(
            reader
                .accept_segment(vec![b'x'; child_boundary], true)
                .unwrap(),
            TerminalInput::Character('x')
        );
        assert_eq!(reader.remaining_bytes, MAX_DRAFT_BYTES);
        reader.reset_draft();
        reader.chat = true;
        reader.child = false;
        reader.remaining_bytes = 2;
        assert_eq!(
            reader.accept_segment(b"ab\\".to_vec(), true).unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(reader.remaining_bytes, 2);
        assert_eq!(
            reader.accept_segment(b"a\\".to_vec(), true).unwrap(),
            TerminalInput::Character('a')
        );
        assert_eq!(reader.remaining_bytes, 0);
        reader.reset_draft();
        reader.chat = false;
        let handoff_segment_bytes = MAX_LINEAR_LINE_BYTES.min(3000);
        for expected in [
            MAX_PRIMARY_DRAFT_BYTES - handoff_segment_bytes,
            MAX_PRIMARY_DRAFT_BYTES - 2 * handoff_segment_bytes,
        ] {
            let first = reader
                .accept_segment(vec![b'x'; handoff_segment_bytes], false)
                .unwrap();
            assert_eq!(first, TerminalInput::Character('x'));
            assert_eq!(reader.remaining_bytes, expected);
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
                handoff_segment_bytes + 1,
                "handoff waits for the entire accepted segment"
            );
            assert!(
                delivered[..handoff_segment_bytes]
                    .iter()
                    .all(|event| *event == TerminalInput::Character('x'))
            );
            assert_eq!(delivered.last(), Some(&TerminalInput::LineContinued));
            let TerminalReader::Lines(drained) = terminal_reader else {
                unreachable!("canonical line reader")
            };
            reader = drained;
        }
        let primary_boundary = MAX_LINEAR_LINE_BYTES / 2;
        while reader.remaining_bytes > primary_boundary {
            let segment_bytes =
                (reader.remaining_bytes - primary_boundary).min(MAX_LINEAR_LINE_BYTES);
            reader
                .accept_segment(vec![b'x'; segment_bytes], false)
                .unwrap();
            reader.pending.clear();
        }
        assert_eq!(reader.remaining_bytes, primary_boundary);
        assert_eq!(
            reader
                .accept_segment(vec![b'x'; primary_boundary + 1], true)
                .unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(reader.remaining_bytes, primary_boundary);
        assert!(reader.pending.is_empty());
        assert_eq!(
            reader
                .accept_segment(vec![b'x'; primary_boundary], true)
                .unwrap(),
            TerminalInput::Character('x')
        );
        assert_eq!(reader.remaining_bytes, MAX_PRIMARY_DRAFT_BYTES);
        assert_eq!(reader.pending.back(), Some(&TerminalInput::Submit));
        reader.pending.clear();
        let mut terminal_reader = TerminalReader::Lines(reader);
        terminal_reader.restore_input_capacity(0).unwrap();
        let TerminalReader::Lines(mut reader) = terminal_reader else {
            unreachable!("Unix line reader")
        };
        assert_eq!(
            reader.accept_segment(b"x".to_vec(), true).unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(reader.remaining_bytes, 0);
        reader.reset_draft();
        reader.accept_segment(b"draft".to_vec(), true).unwrap();
        reader.reset_draft();
        assert_eq!(reader.remaining_bytes, MAX_PRIMARY_DRAFT_BYTES);
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
                .restore_input_capacity(MAX_PRIMARY_DRAFT_BYTES + 1)
                .is_err()
        );
        terminal_reader
            .restore_input_capacity(primary_boundary)
            .unwrap();
        let TerminalReader::Lines(mut resumed_reader) = terminal_reader else {
            unreachable!("Unix line reader")
        };
        assert_eq!(resumed_reader.remaining_bytes, primary_boundary);
        assert_eq!(
            resumed_reader
                .accept_segment(vec![b'x'; primary_boundary + 1], true)
                .unwrap(),
            TerminalInput::LineRejected
        );
        assert_eq!(resumed_reader.remaining_bytes, primary_boundary);
    }

    #[test]
    fn key_mapping_keeps_control_input_local_and_ignores_unowned_events() {
        for (modifiers, shortcut) in [
            (KeyModifiers::SHIFT, NewlineShortcut::ShiftEnter),
            (KeyModifiers::ALT, NewlineShortcut::AltEnter),
            (KeyModifiers::CONTROL, NewlineShortcut::CtrlEnter),
        ] {
            assert_eq!(
                map_input(Event::Key(KeyEvent::new(KeyCode::Enter, modifiers))),
                Some(TerminalInput::ModifiedEnter(shortcut))
            );
        }
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
                Some(TerminalInput::CopySelection),
            ),
            (
                Event::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                )),
                Some(TerminalInput::IgnoredKey),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
                Some(TerminalInput::EndOfInput),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
                Some(TerminalInput::HalfPageUp),
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
                Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
                Some(TerminalInput::Newline),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
                Some(TerminalInput::Transcript),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
                Some(TerminalInput::IgnoredKey),
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
                Some(TerminalInput::IgnoredKey),
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
                Some(TerminalInput::IgnoredKey),
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
                Some(TerminalInput::ModifiedEnter(NewlineShortcut::ShiftEnter)),
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
                Some(TerminalInput::IgnoredKey),
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
                Some(TerminalInput::PointerClick {
                    column: 7,
                    row: 9,
                    clicks: 1,
                }),
            ),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Drag(MouseButton::Left),
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                Some(TerminalInput::PointerDrag { column: 7, row: 9 }),
            ),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                Some(TerminalInput::PointerRelease { column: 7, row: 9 }),
            ),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Drag(MouseButton::Right),
                    column: 7,
                    row: 9,
                    modifiers: KeyModifiers::NONE,
                }),
                None,
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
            (Event::FocusGained, Some(TerminalInput::IgnoredKey)),
        ];
        for (event, expected) in cases {
            assert_eq!(map_input(event), expected);
        }

        {
            use std::time::Instant;
            let now = Instant::now();
            let click = |column, row, clicks| TerminalInput::PointerClick {
                column,
                row,
                clicks,
            };
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
                    b"/\r\x0f\x14\n\x03\x17".as_slice(),
                    vec![
                        TerminalInput::Character('/'),
                        TerminalInput::Submit,
                        TerminalInput::Transcript,
                        TerminalInput::IgnoredKey,
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
                // Alt+Left/Right switch agent chats; plain and other modified arrows keep their owners.
                (
                    b"\x1b[1;3D\x1b[1;3C\x1b[D\x1b[C\x1b[1;3A\x1b[1;7D\x1b[1;4C".as_slice(),
                    vec![
                        TerminalInput::AgentPrevious,
                        TerminalInput::AgentNext,
                        TerminalInput::Left,
                        TerminalInput::Right,
                        TerminalInput::IgnoredKey,
                        TerminalInput::IgnoredKey,
                        TerminalInput::IgnoredKey,
                    ],
                ),
                // Option+Left/Right as `ESC b`/`ESC f` navigate only on macOS; other Alt letters stay inert.
                (
                    b"\x1bb\x1bf\x1bx\x1b[98;3u".as_slice(),
                    if cfg!(target_os = "macos") {
                        vec![
                            TerminalInput::AgentPrevious,
                            TerminalInput::AgentNext,
                            TerminalInput::IgnoredKey,
                            TerminalInput::AgentPrevious,
                        ]
                    } else {
                        vec![TerminalInput::IgnoredKey; 4]
                    },
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
                        TerminalInput::ModifiedEnter(NewlineShortcut::ShiftEnter),
                        TerminalInput::ModifiedEnter(NewlineShortcut::ShiftEnter),
                        TerminalInput::Interrupt,
                    ],
                ),
                (
                    b"\x1b\r\x1b[13;3u\x1b[27;3;13~\x1b[13;5u\x1b[27;5;13~\x1b[13;5:3u\x1b[13;6u".as_slice(),
                    vec![
                        TerminalInput::ModifiedEnter(NewlineShortcut::AltEnter),
                        TerminalInput::ModifiedEnter(NewlineShortcut::AltEnter),
                        TerminalInput::ModifiedEnter(NewlineShortcut::AltEnter),
                        TerminalInput::ModifiedEnter(NewlineShortcut::CtrlEnter),
                        TerminalInput::ModifiedEnter(NewlineShortcut::CtrlEnter),
                        TerminalInput::IgnoredKey,
                    ],
                ),
                (b"\x1bOP\x1bOS\x1b[15~\x1b[24~".as_slice(), vec![TerminalInput::IgnoredKey; 4]),
                (
                    b"\x16\x1b[118;5u\x1b[86;6u\x1b[27;6;118~".as_slice(),
                    vec![TerminalInput::ClipboardPaste; 4],
                ),
                (
                    b"\x1b[99;5:2u\x1b[99;5:3u\x1b[120;3u\x1b[999~\x1b[?1h\x1b[57344u".as_slice(),
                    vec![TerminalInput::IgnoredKey; 2],
                ),
                (
                    b"\x1b[<35;8;10M\x1b[<0;8;10M\x1b[<64;8;10M\x1b[<65;8;10M".as_slice(),
                    vec![
                        TerminalInput::PointerMove { column: 7, row: 9 },
                        click(7, 9, 1),
                        TerminalInput::PointerScrollUp { column: 7, row: 9 },
                        TerminalInput::PointerScrollDown { column: 7, row: 9 },
                    ],
                ),
                (b"\x1b[M (*".as_slice(), vec![click(7, 9, 1)]),
                (
                    b"\x1b[<0;8;10M\x1b[<32;9;10M\x1b[<36;9;11M\x1b[<0;9;11m".as_slice(),
                    vec![
                        click(7, 9, 1),
                        TerminalInput::PointerDrag { column: 8, row: 9 },
                        TerminalInput::PointerDrag { column: 8, row: 10 },
                        TerminalInput::PointerRelease { column: 8, row: 10 },
                    ],
                ),
                (
                    b"\x1b[<0;8;10M\x1b[<0;8;10m\x1b[<0;8;10M\x1b[<0;8;10M\x1b[<0;8;10M\x1b[<0;9;10M\x1b[<0;8;10M"
                        .as_slice(),
                    vec![
                        click(7, 9, 1),
                        TerminalInput::PointerRelease { column: 7, row: 9 },
                        click(7, 9, 2),
                        click(7, 9, 3),
                        click(7, 9, 1),
                        click(8, 9, 1),
                        click(7, 9, 1),
                    ],
                ),
                (
                    b"\x1b[<0;65535;65535M\x1b[<64;1;1M".as_slice(),
                    vec![
                        click(65534, 65534, 1),
                        TerminalInput::PointerScrollUp { column: 0, row: 0 },
                    ],
                ),
                (
                    b"\x1b[<0;0;10M\x1b[<0;8;0M\x1b[<0;65536;10M\x1b[<2;8;10M\x1b[<2;8;10m\x1b[<1;8;10M\x1b[<34;8;10M\x1b[<32;8;10m\x1b[<64;8;10m\x1b[<66;8;10M\x1b[<128;8;10M\x1b[<0;8M\x1b[<0;8;10;1M"
                        .as_slice(),
                    vec![],
                ),
                (
                    b"\x1b[99;6u\x1b[27;6;99~\x1b[67;6u".as_slice(),
                    vec![TerminalInput::CopySelection; 3],
                ),
                // A malformed record is consumed through its terminator; its digits never become keys.
                (
                    b"\x1b[<0;1;x1M\x1b[<0;1;1;2M\x1b[<0;-1;1M\x1b[<0;1;1~1M\x1b[<0;1;1\x031m\x1b[<0;1;1\x1b[A2M"
                        .as_slice(),
                    vec![],
                ),
                (
                    b"a\x1b[<0;2;3Mb\x1b[<0;1;x9mc\x1b[<64;1;1M1".as_slice(),
                    vec![
                        TerminalInput::Character('a'),
                        click(1, 2, 1),
                        TerminalInput::Character('b'),
                        TerminalInput::Character('c'),
                        TerminalInput::PointerScrollUp { column: 0, row: 0 },
                        TerminalInput::Character('1'),
                    ],
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
                        ReaderEvent::Pointer(input) => input,
                        ReaderEvent::Paste(_) => panic!("ordinary sequence became a paste"),
                        ReaderEvent::CursorPosition(_, _)
                        | ReaderEvent::KeyboardReply(_)
                        | ReaderEvent::GraphicsReply | ReaderEvent::CellPixels(_) => {
                            panic!("ordinary sequence became a cursor response")
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(events, expected, "raw keyboard/pointer sequence");
            }
            for (body, expected) in [
                (
                    b"/exit\r\n/setup\x03\t".to_vec(),
                    Ok("/exit\r\n/setup\x03\t".to_owned()),
                ),
                (vec![b'x'; MAX_PASTE_BYTES], Ok("x".repeat(MAX_PASTE_BYTES))),
                (
                    vec![b'x'; MAX_PASTE_BYTES + 1],
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
            let mut decoder = raw::Decoder::default();
            for (offset, clicks) in [(0, 1), (500, 2), (1001, 1)] {
                let at = now + Duration::from_millis(offset);
                let events = b"\x1b[<0;8;10M"
                    .iter()
                    .filter_map(|&byte| decoder.feed(byte, at))
                    .collect::<Vec<_>>();
                assert!(
                    matches!(events.as_slice(), [ReaderEvent::Pointer(input)] if *input == click(7, 9, clicks)),
                    "multi-click window at {offset} ms"
                );
            }
            let mut oversized_record = b"\x1b[<0;".to_vec();
            oversized_record.extend([b'1'; 40]);
            oversized_record.extend_from_slice(b";1;1M\x1b[<0;1;");
            oversized_record.extend([b'2'; 40]);
            oversized_record.extend_from_slice(b"mz");
            assert!(matches!(
                decode(&oversized_record).as_slice(),
                [ReaderEvent::Event(Event::Key(KeyEvent {
                    code: KeyCode::Char('z'),
                    ..
                }))]
            ));
            // Records split across reads complete or stay discarded as a whole.
            for (parts, expected) in [
                ([&b"\x1b[<0;5"[..], b";7M"], Some(click(4, 6, 1))),
                ([&b"\x1b[<0;1;x"[..], b"1M"], None),
                ([&b"\x1b[<"[..], b"0;1;1"], None),
            ] {
                let mut decoder = raw::Decoder::default();
                let events = parts
                    .iter()
                    .enumerate()
                    .flat_map(|(read, part)| {
                        let at = now + Duration::from_millis(50 * read as u64);
                        part.iter()
                            .filter_map(|&byte| decoder.feed(byte, at))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                match expected {
                    Some(input) => assert!(
                        matches!(events.as_slice(), [ReaderEvent::Pointer(event)] if *event == input),
                        "split record {parts:?}"
                    ),
                    None => assert!(events.is_empty(), "split record {parts:?}"),
                }
            }
            for record in [&b"\x1b[<0;1;x1"[..], b"\x1b[<0;12;3"] {
                let mut decoder = raw::Decoder::default();
                assert!(record.iter().all(|&byte| decoder.feed(byte, now).is_none()));
                assert!(
                    decoder
                        .expire(now + Duration::from_secs(9))
                        .unwrap()
                        .is_none()
                );
                assert!(
                    decoder
                        .expire(now + Duration::from_secs(10))
                        .unwrap()
                        .is_none()
                );
                let later = b"7M"
                    .iter()
                    .filter_map(|&byte| decoder.feed(byte, now + Duration::from_secs(10)))
                    .map(|event| match event {
                        ReaderEvent::Event(event) => map_input(event),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    later,
                    [
                        Some(TerminalInput::Character('7')),
                        Some(TerminalInput::Character('M'))
                    ],
                    "a dropped record leaves later input intact"
                );
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
