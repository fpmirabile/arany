use super::{INPUT_POLL_INTERVAL, MAX_DRAFT_BYTES, ReaderEvent, StdinFlags, map_input};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_KEY_SEQUENCE_BYTES: usize = 64;
const RECORD_DEADLINE: Duration = Duration::from_secs(10);
const PASTE_END: &[u8; 6] = b"\x1b[201~";

pub(super) fn start(
    sender: mpsc::Sender<io::Result<ReaderEvent>>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<(JoinHandle<()>, StdinFlags)> {
    let mut poll = Poll::new()?;
    let fd = io::stdin().as_raw_fd();
    poll.registry()
        .register(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;
    let flags = StdinFlags::acquire()?;
    let thread = std::thread::Builder::new()
        .name("arany-terminal-input".into())
        .spawn(move || {
            if let Err(error) = read(&mut poll, fd, &sender, &shutdown) {
                let _ = sender.blocking_send(Err(error));
            }
        })?;
    Ok((thread, flags))
}

fn read(
    poll: &mut Poll,
    fd: i32,
    sender: &mpsc::Sender<io::Result<ReaderEvent>>,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut events = Events::with_capacity(4);
    let mut bytes = [0; 4096];
    let mut decoder = Decoder::default();
    let mut geometry = crossterm::terminal::size().ok();
    while !shutdown.load(Ordering::Acquire) {
        let now = Instant::now();
        if let Some(event) = decoder.expire(now)?
            && sender.blocking_send(Ok(event)).is_err()
        {
            return Ok(());
        }
        let resized = crossterm::terminal::size().ok();
        if resized != geometry {
            geometry = resized;
            if let Some((width, height)) = resized
                && sender
                    .blocking_send(Ok(ReaderEvent::Event(Event::Resize(width, height))))
                    .is_err()
            {
                return Ok(());
            }
        }
        match poll.poll(&mut events, Some(INPUT_POLL_INTERVAL)) {
            Ok(()) if events.is_empty() => continue,
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
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
                    if let Some(event) = decoder.feed(byte, now)
                        && sender.blocking_send(Ok(event)).is_err()
                    {
                        return Ok(());
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
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
                        return event;
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
                event
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
                    Err("paste exceeds 8 KiB")
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
    if release || button > 127 {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [
        (4, KeyModifiers::SHIFT),
        (8, KeyModifiers::ALT),
        (16, KeyModifiers::CONTROL),
    ] {
        if button & bit != 0 {
            modifiers |= modifier;
        }
    }
    let kind = if button & 64 != 0 {
        match button & 3 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            _ => return None,
        }
    } else if button & 32 != 0 && button & 3 == 3 {
        MouseEventKind::Moved
    } else if button & 32 == 0 && button & 3 == 0 {
        MouseEventKind::Down(MouseButton::Left)
    } else {
        return None;
    };
    Some(ReaderEvent::Event(Event::Mouse(MouseEvent {
        kind,
        column: column.checked_sub(1)?,
        row: row.checked_sub(1)?,
        modifiers,
    })))
}
