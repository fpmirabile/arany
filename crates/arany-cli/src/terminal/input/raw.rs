use super::{
    CursorQuery, INPUT_POLL_INTERVAL, InputAdmission, MAX_DRAFT_BYTES, ReaderEvent, TerminalInput,
    admit_input, map_input, pointer_input, terminal_input,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read},
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, error::TrySendError};

const MAX_KEY_SEQUENCE_BYTES: usize = 64;
const RECORD_DEADLINE: Duration = Duration::from_secs(10);
const OUTBOX_LIMIT: usize = 256;
const READ_BUFFER_BYTES: usize = 4096;
const MAX_BACKLOG: usize = OUTBOX_LIMIT + READ_BUFFER_BYTES;
const OUTBOX_RETRY: Duration = Duration::from_millis(2);
const PASTE_END: &[u8; 6] = b"\x1b[201~";
const MULTI_CLICK: Duration = Duration::from_millis(500);

pub(super) fn start(
    sender: mpsc::Sender<super::Observed<ReaderEvent>>,
    shutdown: Arc<AtomicBool>,
    cursor: CursorQuery,
) -> io::Result<(JoinHandle<()>, InputAdmission)> {
    let mut input = terminal_input()?;
    let mut poll = Poll::new()?;
    let fd = input.as_raw_fd();
    poll.registry()
        .register(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
    let (admission, mut requests) = InputAdmission::new(&poll)?;
    let thread = std::thread::Builder::new()
        .name("arany-terminal-input".into())
        .spawn(move || {
            let mut outbox = Outbox {
                sender: &sender,
                backlog: VecDeque::new(),
            };
            let result = read(
                &mut poll,
                &mut input,
                &mut outbox,
                &shutdown,
                &cursor,
                &mut requests,
            );
            cursor.close();
            if let Err(error) = result {
                let _ = sender.blocking_send((0, Err(error)));
            }
        })?;
    Ok((thread, admission))
}

// A cursor reply must not wait behind a full event queue while the owner waits for that reply.
struct Outbox<'a> {
    sender: &'a mpsc::Sender<super::Observed<ReaderEvent>>,
    backlog: VecDeque<(u64, ReaderEvent)>,
}

impl Outbox<'_> {
    fn push(&mut self, event: ReaderEvent, observed: u64) -> io::Result<bool> {
        if self.backlog.len() == MAX_BACKLOG && !self.flush() {
            return Ok(false);
        }
        if self.backlog.len() == MAX_BACKLOG {
            return Err(io::Error::other("terminal input backlog exceeded"));
        }
        self.backlog.push_back((observed, event));
        Ok(self.flush())
    }

    fn flush(&mut self) -> bool {
        while let Some((observed, event)) = self.backlog.pop_front() {
            match self.sender.try_send((observed, Ok(event))) {
                Ok(()) => {}
                Err(TrySendError::Full((observed, Ok(event)))) => {
                    self.backlog.push_front((observed, event));
                    break;
                }
                Err(_) => return false,
            }
        }
        true
    }
}

fn read(
    poll: &mut Poll,
    input: &mut File,
    outbox: &mut Outbox<'_>,
    shutdown: &AtomicBool,
    cursor: &CursorQuery,
    requests: &mut mpsc::Receiver<tokio::sync::oneshot::Sender<u64>>,
) -> io::Result<()> {
    let fd = input.as_raw_fd();
    let mut events = Events::with_capacity(4);
    let mut bytes = [0; READ_BUFFER_BYTES];
    let mut decoder = Decoder::default();
    let mut geometry = crossterm::terminal::size().ok();
    let mut epoch = 0;
    while !shutdown.load(Ordering::Acquire) {
        admit_input(input, requests, &mut epoch, || decoder = Decoder::default())?;
        if !outbox.flush() {
            return Ok(());
        }
        if outbox.backlog.len() >= OUTBOX_LIMIT && !cursor.waiting() {
            std::thread::sleep(OUTBOX_RETRY);
            continue;
        }
        let now = Instant::now();
        if let Some(event) = decoder.expire(now)?
            && !outbox.push(event, epoch)?
        {
            return Ok(());
        }
        let resized = crossterm::terminal::size().ok();
        if resized != geometry {
            geometry = resized;
            if let Some((width, height)) = resized
                && !outbox.push(ReaderEvent::Event(Event::Resize(width, height)), epoch)?
            {
                return Ok(());
            }
        }
        let timeout = if outbox.backlog.is_empty() {
            INPUT_POLL_INTERVAL
        } else {
            OUTBOX_RETRY
        };
        match poll.poll(&mut events, Some(timeout)) {
            Ok(()) if events.is_empty() => continue,
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        admit_input(input, requests, &mut epoch, || decoder = Decoder::default())?;
        match input.read(&mut bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal input closed",
                ));
            }
            Ok(len) => {
                let now = Instant::now();
                for &byte in &bytes[..len] {
                    if shutdown.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Some(event) = decoder.feed(byte, now) {
                        if let ReaderEvent::CursorPosition(column, row) = event {
                            cursor.respond(column, row);
                        } else if !outbox.push(event, epoch)? {
                            return Ok(());
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        poll.registry()
            .reregister(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
    }
    Ok(())
}

#[derive(Default)]
enum Mode {
    #[default]
    Key,
    Escape,
    Csi,
    Ss3,
    Utf8 {
        length: usize,
        alt: bool,
    },
    Mouse,
    SkipCsi,
    ControlString {
        escape: bool,
    },
    Paste(Paste),
}

#[derive(Default)]
pub(super) struct Decoder {
    mode: Mode,
    sequence: Vec<u8>,
    started: Option<Instant>,
    last_click: Option<(u16, u16, Instant, u8)>,
}

impl Decoder {
    pub(super) fn expire(&mut self, now: Instant) -> io::Result<Option<ReaderEvent>> {
        let Some(started) = self.started else {
            return Ok(None);
        };
        let elapsed = now.saturating_duration_since(started);
        if matches!(self.mode, Mode::Escape) && elapsed >= INPUT_POLL_INTERVAL {
            self.reset();
            return Ok(key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press));
        }
        if elapsed >= RECORD_DEADLINE {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "unterminated terminal input record",
            ));
        }
        Ok(None)
    }

    fn reset(&mut self) {
        self.mode = Mode::Key;
        self.sequence.clear();
        self.started = None;
    }

    // A press on the cell of the previous press within the window counts as a double or triple click.
    fn count_click(&mut self, event: Option<ReaderEvent>, now: Instant) -> Option<ReaderEvent> {
        let Some(ReaderEvent::Pointer(TerminalInput::PointerClick { column, row, .. })) = event
        else {
            return event;
        };
        let clicks = match self.last_click {
            Some((last_column, last_row, at, clicks))
                if (last_column, last_row) == (column, row)
                    && clicks < 3
                    && now.saturating_duration_since(at) <= MULTI_CLICK =>
            {
                clicks + 1
            }
            _ => 1,
        };
        self.last_click = Some((column, row, now, clicks));
        Some(ReaderEvent::Pointer(TerminalInput::PointerClick {
            column,
            row,
            clicks,
        }))
    }

    pub(super) fn feed(&mut self, byte: u8, now: Instant) -> Option<ReaderEvent> {
        match &mut self.mode {
            Mode::Key => match byte {
                b'\x1b' => {
                    self.mode = Mode::Escape;
                    self.started = Some(now);
                    self.sequence.push(byte);
                    None
                }
                b'\r' => key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press),
                b'\t' => key(KeyCode::Tab, KeyModifiers::NONE, KeyEventKind::Press),
                b'\x7f' | b'\x08' => {
                    key(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Press)
                }
                1..=26 => key(
                    KeyCode::Char(char::from(b'a' + byte - 1)),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Press,
                ),
                b' '..=b'~' => key(
                    KeyCode::Char(char::from(byte)),
                    KeyModifiers::NONE,
                    KeyEventKind::Press,
                ),
                0xc2..=0xf4 => {
                    self.started = Some(now);
                    self.sequence.push(byte);
                    self.mode = Mode::Utf8 {
                        length: utf8_length(byte),
                        alt: false,
                    };
                    None
                }
                _ => None,
            },
            Mode::Escape => match byte {
                b'[' | b'O' => {
                    self.sequence.push(byte);
                    self.mode = if byte == b'[' { Mode::Csi } else { Mode::Ss3 };
                    None
                }
                b']' | b'P' | b'_' | b'^' => {
                    self.sequence.clear();
                    self.mode = Mode::ControlString { escape: false };
                    None
                }
                b'\x1b' => {
                    self.started = Some(now);
                    key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press)
                }
                0xc2..=0xf4 => {
                    self.sequence.clear();
                    self.sequence.push(byte);
                    self.mode = Mode::Utf8 {
                        length: utf8_length(byte),
                        alt: true,
                    };
                    None
                }
                _ => {
                    self.reset();
                    None
                }
            },
            Mode::Utf8 { length, alt } => {
                self.sequence.push(byte);
                if self.sequence.len() < *length {
                    return None;
                }
                let event = (!*alt)
                    .then(|| std::str::from_utf8(&self.sequence).ok()?.chars().next())
                    .flatten()
                    .and_then(|character| {
                        key(
                            KeyCode::Char(character),
                            KeyModifiers::NONE,
                            KeyEventKind::Press,
                        )
                    });
                self.reset();
                event
            }
            Mode::Csi | Mode::Ss3 => {
                if self.sequence.len() == MAX_KEY_SEQUENCE_BYTES {
                    self.sequence.clear();
                    self.mode = Mode::SkipCsi;
                } else {
                    self.sequence.push(byte);
                    if self.sequence == b"\x1b[M" {
                        self.mode = Mode::Mouse;
                        return None;
                    }
                    if self.sequence == b"\x1b[200~" {
                        self.sequence.clear();
                        self.mode = Mode::Paste(Paste::default());
                        return None;
                    }
                    if (b'@'..=b'~').contains(&byte) {
                        let event = sequence_event(&self.sequence);
                        self.reset();
                        return self.count_click(event, now);
                    }
                }
                if byte.is_ascii_control() || (b'@'..=b'~').contains(&byte) {
                    self.reset();
                }
                None
            }
            Mode::Mouse => {
                self.sequence.push(byte);
                if self.sequence.len() < 6 {
                    return None;
                }
                let event = self.sequence[3].checked_sub(32).and_then(|button| {
                    mouse(
                        button.into(),
                        self.sequence[4].checked_sub(32)?.into(),
                        self.sequence[5].checked_sub(32)?.into(),
                        false,
                    )
                });
                self.reset();
                self.count_click(event, now)
            }
            Mode::SkipCsi => {
                if byte.is_ascii_control() || (b'@'..=b'~').contains(&byte) {
                    self.reset();
                }
                None
            }
            Mode::ControlString { escape } => {
                if byte == b'\x07' || (*escape && byte == b'\\') {
                    self.reset();
                } else {
                    *escape = byte == b'\x1b';
                }
                None
            }
            Mode::Paste(paste) => {
                if !paste.feed(byte) {
                    return None;
                }
                let Mode::Paste(paste) = std::mem::take(&mut self.mode) else {
                    unreachable!()
                };
                self.started = None;
                Some(ReaderEvent::Paste(if paste.overflow {
                    Err("paste is too large; paste a smaller section")
                } else {
                    String::from_utf8(paste.bytes).map_err(|_| "paste is not valid UTF-8")
                }))
            }
        }
    }
}

#[derive(Default)]
struct Paste {
    bytes: Vec<u8>,
    marker: usize,
    overflow: bool,
}

impl Paste {
    fn push(&mut self, byte: u8) {
        if self.overflow {
            return;
        }
        if self.bytes.len() == MAX_DRAFT_BYTES {
            self.bytes.clear();
            self.overflow = true;
        } else {
            self.bytes.push(byte);
        }
    }

    fn feed(&mut self, byte: u8) -> bool {
        if byte == PASTE_END[self.marker] {
            self.marker += 1;
            return self.marker == PASTE_END.len();
        }
        for &pending in &PASTE_END[..self.marker] {
            self.push(pending);
        }
        self.marker = 0;
        if byte == PASTE_END[0] {
            self.marker = 1;
        } else {
            self.push(byte);
        }
        false
    }
}

fn utf8_length(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Option<ReaderEvent> {
    let event = Event::Key(KeyEvent::new_with_kind(code, modifiers, kind));
    map_input(event.clone()).map(|_| ReaderEvent::Event(event))
}

fn modifiers(value: &str) -> Option<(KeyModifiers, KeyEventKind)> {
    if value.is_empty() {
        return Some((KeyModifiers::NONE, KeyEventKind::Press));
    }
    let mut parts = value.split(':');
    let mask = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    if mask > 255 {
        return None;
    }
    let kind = match parts.next() {
        None | Some("1") => KeyEventKind::Press,
        Some("2") => KeyEventKind::Repeat,
        Some("3") => KeyEventKind::Release,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    let mut selected = KeyModifiers::NONE;
    for (bit, modifier) in [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ] {
        if mask & bit != 0 {
            selected |= modifier;
        }
    }
    Some((selected, kind))
}

fn codepoint(value: u32) -> Option<KeyCode> {
    match value {
        9 => Some(KeyCode::Tab),
        13 => Some(KeyCode::Enter),
        27 => Some(KeyCode::Esc),
        127 => Some(KeyCode::Backspace),
        57344..=63743 => None,
        _ => char::from_u32(value).map(KeyCode::Char),
    }
}

fn sequence_event(bytes: &[u8]) -> Option<ReaderEvent> {
    let end = *bytes.last()?;
    let text = std::str::from_utf8(&bytes[2..bytes.len() - 1]).ok()?;
    if end == b'R' && bytes.starts_with(b"\x1b[") {
        let (row, column) = text.split_once(';')?;
        if !row.bytes().all(|byte| byte.is_ascii_digit())
            || !column.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        return Some(ReaderEvent::CursorPosition(
            column.parse::<u16>().ok()?.checked_sub(1)?,
            row.parse::<u16>().ok()?.checked_sub(1)?,
        ));
    }
    if text.starts_with('<') && matches!(end, b'M' | b'm') {
        let mut parts = text[1..].split(';');
        let button = parts.next()?.parse().ok()?;
        let column = parts.next()?.parse().ok()?;
        let row = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        return mouse(button, column, row, end == b'm');
    }
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
    {
        return None;
    }
    let mut parts = text.split(';');
    let first = parts.next()?;
    let (modifiers, kind) = modifiers(parts.next().unwrap_or(""))?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let code = match end {
        b'Z' if first.is_empty() && third.is_none() => KeyCode::BackTab,
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F' if matches!(first, "" | "1") && third.is_none() => {
            match end {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                _ => KeyCode::End,
            }
        }
        b'~' if first == "27" => codepoint(third?.parse().ok()?)?,
        b'~' if third.is_none() => match first {
            "1" | "7" => KeyCode::Home,
            "4" | "8" => KeyCode::End,
            "3" => KeyCode::Delete,
            "5" => KeyCode::PageUp,
            "6" => KeyCode::PageDown,
            _ => return None,
        },
        b'u' if third.is_none() => {
            let mut points = first.split(':');
            let base = points.next()?.parse().ok()?;
            let shifted = points.next();
            if let Some(layout) = points.next() {
                layout.parse::<u32>().ok()?;
            }
            if points.next().is_some() {
                return None;
            }
            if modifiers.contains(KeyModifiers::SHIFT)
                && shifted.is_some_and(|value| !value.is_empty())
            {
                codepoint(shifted?.parse().ok()?)?
            } else {
                let mut code = codepoint(base)?;
                if modifiers.contains(KeyModifiers::SHIFT)
                    && let KeyCode::Char(character) = &mut code
                {
                    *character = character.to_ascii_uppercase();
                }
                code
            }
        }
        _ => return None,
    };
    key(code, modifiers, kind)
}

fn mouse(button: u16, column: u16, row: u16, release: bool) -> Option<ReaderEvent> {
    if button > 127 {
        return None;
    }
    let left = button & 3 == 0;
    let motion = button & 32 != 0;
    let kind = if button & 64 != 0 {
        match button & 3 {
            0 if !release => MouseEventKind::ScrollUp,
            1 if !release => MouseEventKind::ScrollDown,
            _ => return None,
        }
    } else if release {
        if !left || motion {
            return None;
        }
        MouseEventKind::Up(MouseButton::Left)
    } else if motion && button & 3 == 3 {
        MouseEventKind::Moved
    } else if left {
        if motion {
            MouseEventKind::Drag(MouseButton::Left)
        } else {
            MouseEventKind::Down(MouseButton::Left)
        }
    } else {
        return None;
    };
    pointer_input(kind, column.checked_sub(1)?, row.checked_sub(1)?, 1).map(ReaderEvent::Pointer)
}

#[cfg(test)]
mod tests {
    use super::super::{
        INPUT_QUEUE_CAPACITY, InputReader, LineEvent, LineReader, TerminalInput, TerminalReader,
        read_lines,
    };
    use super::*;
    use rustix::fs::OFlags;
    use std::{io::Write, os::fd::OwnedFd, os::unix::net::UnixStream};

    async fn next_input(reader: &mut TerminalReader) -> TerminalInput {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = reader.recv().await.unwrap();
                if !matches!(event, TerminalInput::LineRejected | TerminalInput::Resize) {
                    break event;
                }
            }
        })
        .await
        .unwrap()
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(Instant::now() < deadline, "reader progress deadline");
            std::thread::yield_now();
        }
    }

    #[tokio::test]
    async fn approval_admission_discards_pre_barrier_records_and_delivers_later_bytes() {
        for linear in [false, true] {
            let (mut master, mut input) = arany::test_support::process::cloexec_pty(None);
            let probe = input.try_clone().unwrap();
            let flags = rustix::fs::fcntl_getfl(&input).unwrap();
            rustix::fs::fcntl_setfl(&input, flags | OFlags::NONBLOCK).unwrap();
            // A full input queue fails the write instead of blocking the test.
            let flags = rustix::fs::fcntl_getfl(&master).unwrap();
            rustix::fs::fcntl_setfl(&master, flags | OFlags::NONBLOCK).unwrap();
            let mut settings = rustix::termios::tcgetattr(&input).unwrap();
            // Linear keeps canonical echo, which shows when the line discipline holds a partial line.
            if !linear {
                settings.make_raw();
                settings
                    .local_modes
                    .remove(rustix::termios::LocalModes::ECHO);
            }
            rustix::termios::tcsetattr(&input, rustix::termios::OptionalActions::Now, &settings)
                .unwrap();
            let mut poll = Poll::new().unwrap();
            let fd = input.as_raw_fd();
            poll.registry()
                .register(&mut SourceFd(&fd), Token(0), Interest::READABLE)
                .unwrap();
            let (admission, mut requests) = InputAdmission::new(&poll).unwrap();
            let (start, ready) = std::sync::mpsc::sync_channel(1);
            let shutdown = Arc::new(AtomicBool::new(false));
            let should_stop = shutdown.clone();
            let mut reader = if linear {
                let (sender, receiver) = mpsc::channel(4);
                for _ in 0..4 {
                    sender
                        .try_send((0, Ok(LineEvent::Submitted(b"1".to_vec()))))
                        .unwrap();
                }
                assert_eq!(sender.capacity(), 0);
                master.write_all(b"1\n1\n").unwrap();
                let thread = std::thread::spawn(move || {
                    ready.recv().unwrap();
                    read_lines(&mut poll, &mut input, &sender, &should_stop, &mut requests)
                        .unwrap();
                });
                TerminalReader::Lines(LineReader {
                    receiver,
                    shutdown,
                    thread: Some(thread),
                    pending: VecDeque::from([TerminalInput::Character('1'), TerminalInput::Submit]),
                    draft_bytes: 1,
                    epoch: 0,
                    admission,
                })
            } else {
                let (sender, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
                for _ in 0..INPUT_QUEUE_CAPACITY {
                    sender
                        .try_send((
                            0,
                            Ok(ReaderEvent::Event(Event::Key(KeyEvent::new(
                                KeyCode::Enter,
                                KeyModifiers::NONE,
                            )))),
                        ))
                        .unwrap();
                }
                assert_eq!(sender.capacity(), 0);
                // XNU blocks PTY writers near its 1024-byte input queue, so the unread bytes stay small.
                master.write_all(&b"1\r".repeat(64)).unwrap();
                let cursor = CursorQuery::default();
                let query = cursor.clone();
                let thread = std::thread::spawn(move || {
                    ready.recv().unwrap();
                    let mut outbox = Outbox {
                        sender: &sender,
                        backlog: VecDeque::new(),
                    };
                    read(
                        &mut poll,
                        &mut input,
                        &mut outbox,
                        &should_stop,
                        &query,
                        &mut requests,
                    )
                    .unwrap();
                });
                TerminalReader::Keys(InputReader {
                    receiver,
                    shutdown,
                    thread: Some(thread),
                    paste: None,
                    cursor,
                    epoch: 0,
                    admission,
                })
            };
            let (admitted, ()) = tokio::join!(biased; reader.discard_typeahead(), async {
                start.send(()).unwrap();
            });
            admitted.unwrap();
            master
                .write_all(if linear { b"no\n" } else { b"n" })
                .unwrap();
            assert_eq!(
                next_input(&mut reader).await,
                TerminalInput::Character('n'),
                "linear={linear}: unread OS bytes and queued events cannot act after admission"
            );
            if linear {
                assert_eq!(next_input(&mut reader).await, TerminalInput::Character('o'));
                assert_eq!(next_input(&mut reader).await, TerminalInput::Submit);
                master.write_all(b"yes").unwrap();
                let mut echoed = Vec::new();
                wait_until(|| {
                    let mut chunk = [0; 256];
                    if let Ok(count) = master.read(&mut chunk) {
                        echoed.extend_from_slice(&chunk[..count]);
                    }
                    echoed.ends_with(b"yes")
                });
            } else {
                // A read prefix of a kitty Enter must not complete after admission.
                master.write_all(b"x\x1b[13").unwrap();
                assert_eq!(next_input(&mut reader).await, TerminalInput::Character('x'));
                wait_until(|| rustix::io::ioctl_fionread(&probe).unwrap() == 0);
            }
            reader.discard_typeahead().await.unwrap();
            master
                .write_all(if linear { b"no\n" } else { b"u1\r" })
                .unwrap();
            let expected: &[TerminalInput] = if linear {
                &[
                    TerminalInput::Character('n'),
                    TerminalInput::Character('o'),
                    TerminalInput::Submit,
                ]
            } else {
                &[
                    TerminalInput::Character('u'),
                    TerminalInput::Character('1'),
                    TerminalInput::Submit,
                ]
            };
            for expected in expected {
                assert_eq!(
                    next_input(&mut reader).await,
                    *expected,
                    "linear={linear}: partial records before admission are discarded and later bytes delivered"
                );
            }
            reader.stop();
        }
    }

    #[test]
    fn oversized_mouse_records_are_discarded_within_the_sequence_bound() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        for _ in 0..3 {
            let mut record = b"\x1b[<0;".to_vec();
            record.extend([b'9'; 4096]);
            record.extend_from_slice(b";1Mx");
            let events = record
                .iter()
                .filter_map(|&byte| decoder.feed(byte, now))
                .collect::<Vec<_>>();
            assert!(matches!(
                events.as_slice(),
                [ReaderEvent::Event(Event::Key(KeyEvent {
                    code: KeyCode::Char('x'),
                    ..
                }))]
            ));
            assert!(decoder.sequence.capacity() <= MAX_KEY_SEQUENCE_BYTES);
        }
    }

    #[test]
    fn cursor_reply_bypasses_full_channel_and_backlog_without_losing_keys() {
        let (mut peer, input) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        let mut input = File::from(OwnedFd::from(input));
        let mut poll = Poll::new().unwrap();
        let fd = input.as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&fd), Token(0), Interest::READABLE)
            .unwrap();
        let (_admission, mut requests) = InputAdmission::new(&poll).unwrap();
        let (sender, mut receiver) = mpsc::channel(super::super::INPUT_QUEUE_CAPACITY);
        let event = || key(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Press).unwrap();
        let observed = 0;
        for _ in 0..super::super::INPUT_QUEUE_CAPACITY {
            assert!(sender.try_send((observed, Ok(event()))).is_ok());
        }
        let shutdown = Arc::new(AtomicBool::new(false));
        let cursor = CursorQuery::default();
        peer.write_all(b"x\x1b[3;5R").unwrap();
        let stopped = Arc::clone(&shutdown);
        let query = cursor.clone();
        let reader = std::thread::spawn(move || {
            let mut outbox = Outbox {
                sender: &sender,
                backlog: (0..OUTBOX_LIMIT).map(|_| (observed, event())).collect(),
            };
            let result = read(
                &mut poll,
                &mut input,
                &mut outbox,
                &stopped,
                &cursor,
                &mut requests,
            );
            (result, outbox.backlog)
        });
        let reply = query.query(&mut Vec::new());
        shutdown.store(true, Ordering::Release);
        let (result, backlog) = reader.join().unwrap();
        result.unwrap();
        assert_eq!(reply.unwrap(), (4, 2));
        assert_eq!(backlog.len(), OUTBOX_LIMIT + 1);
        for (_, event) in backlog {
            assert!(matches!(
                event,
                ReaderEvent::Event(Event::Key(KeyEvent {
                    code: KeyCode::Char('x'),
                    ..
                }))
            ));
        }
        for _ in 0..super::super::INPUT_QUEUE_CAPACITY {
            assert!(matches!(
                receiver.try_recv().unwrap().1.unwrap(),
                ReaderEvent::Event(Event::Key(KeyEvent {
                    code: KeyCode::Char('x'),
                    ..
                }))
            ));
        }
        assert!(receiver.try_recv().is_err());
        let (sender, _receiver) = mpsc::channel(1);
        assert!(sender.try_send((observed, Ok(event()))).is_ok());
        let mut outbox = Outbox {
            sender: &sender,
            backlog: (0..MAX_BACKLOG).map(|_| (observed, event())).collect(),
        };
        assert!(outbox.push(event(), observed).is_err());
        assert_eq!(outbox.backlog.len(), MAX_BACKLOG);
    }
}
