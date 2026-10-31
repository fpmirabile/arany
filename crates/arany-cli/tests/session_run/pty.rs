#[cfg(target_os = "linux")]
use crate::active_terminal::{ProductGuard, product_child_of};
use crate::loopback::wait_product;
use crate::product::ChildGuard;
use crate::{PtyResponses, drain_pty, reply_positioned_cursor_queries, screen_rows, tail};
#[cfg(target_os = "linux")]
use rustix::process::Pid;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::{
    io::Write,
    process::{ChildStdin, ChildStdout, ExitStatus, Output},
    thread,
    time::{Duration, Instant},
};

pub(crate) const PTY_STAGE_WAIT: Duration = Duration::from_secs(10);
const MAX_PTY_HISTORY: usize = 512 * 1024;
const MAX_LINEAR_PAGES: usize = 128;

// One launched PTY journey: the guarded `script` child, its streams and the captured screen.
// Fields drop in order: the product guard, then the PTY pipes, then the reaping child guard.
pub(crate) struct Pty {
    #[cfg(target_os = "linux")]
    product: Option<ProductGuard>,
    pub(crate) input: ChildStdin,
    pub(crate) output: ChildStdout,
    pub(crate) transcript: Vec<u8>,
    pub(crate) answered: PtyResponses,
    pub(crate) child: ChildGuard,
}

impl Pty {
    pub(crate) fn new(child: ChildGuard) -> Self {
        Self::with_responses(child, PtyResponses::default())
    }

    pub(crate) fn with_responses(mut child: ChildGuard, answered: PtyResponses) -> Self {
        Self {
            #[cfg(target_os = "linux")]
            product: None,
            input: child.child().stdin.take().expect("PTY input"),
            output: child.child().stdout.take().expect("PTY output"),
            transcript: Vec::new(),
            answered,
            child,
        }
    }

    // Drain output, then answer cursor queries and, unless manual, decline folder consent.
    pub(crate) fn pump(&mut self) {
        drain_pty(&mut self.output, &mut self.transcript);
        reply_positioned_cursor_queries(
            &mut self.input,
            &self.transcript,
            &mut self.answered.cursor_queries,
        );
        if self.answered.auto_consent {
            let mut declined = false;
            crate::process::decline_workspace_consent(
                &mut self.input,
                &self.transcript[self.answered.consent_from..],
                &mut declined,
            );
            if declined {
                self.answered.consent_from = self.transcript.len();
            }
        }
    }

    // Each later step waits only on bytes after this point, keeping a long journey under the
    // capture bound.
    pub(crate) fn fresh_capture(&mut self) {
        self.transcript.clear();
        self.answered = PtyResponses::default();
    }

    // The transcript offset before `bytes`, from which their effect is awaited.
    pub(crate) fn send(&mut self, bytes: &[u8]) -> usize {
        let from = self.transcript.len();
        self.input.write_all(bytes).expect("PTY input write");
        from
    }

    // Send `bytes`, then await `needle` after them; the offset before `bytes`.
    #[track_caller]
    pub(crate) fn send_and_wait(&mut self, bytes: &[u8], needle: &[u8]) -> usize {
        let from = self.send(bytes);
        self.wait_for_after(from, needle);
        from
    }

    // Pump until the wrapper exits; its status.
    #[track_caller]
    pub(crate) fn wait_exit(&mut self, what: &str) -> ExitStatus {
        let deadline = Instant::now() + PTY_STAGE_WAIT;
        loop {
            self.pump();
            if let Some(status) = self.child.child().try_wait().expect("PTY wrapper status") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: {}",
                tail(&self.transcript)
            );
            thread::yield_now();
        }
    }

    #[track_caller]
    pub(crate) fn pump_until(&mut self, what: &str, done: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + PTY_STAGE_WAIT;
        while !done(&self.transcript) {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "{what}: {}",
                tail(&self.transcript)
            );
            thread::yield_now();
        }
    }

    #[track_caller]
    pub(crate) fn wait_for(&mut self, needle: &[u8]) {
        self.wait_for_after(0, needle);
    }

    #[track_caller]
    pub(crate) fn wait_for_after(&mut self, from: usize, needle: &[u8]) {
        self.pump_until(
            &format!("PTY stage missing {}", tail(needle)),
            |transcript| {
                crate::process::pty_text_matches(&transcript[from..], needle)
                    .next()
                    .is_some()
            },
        );
    }

    #[track_caller]
    pub(crate) fn wait_for_screen(
        &mut self,
        (from, columns, rows): (usize, usize, usize),
        needle: &str,
    ) {
        let deadline = Instant::now() + PTY_STAGE_WAIT;
        // A stale screen that already shows `needle` is not the action's result: require a
        // changed decoded screen, while still accepting a needle that was visible before.
        let before = screen_rows(&self.transcript[..from], columns, rows);
        while self.transcript.len() == from
            || screen_rows(&self.transcript, columns, rows) == before
            || !screen_contains(&self.transcript, columns, rows, needle)
        {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "screen missing {needle}: {}",
                tail(&self.transcript)
            );
            thread::yield_now();
        }
    }

    // Keep unchanged diff cells while bounding each pump capture; retain raw evidence separately.
    pub(crate) fn checkpoint_screen(
        &mut self,
        history: &mut Vec<u8>,
        (columns, rows): (usize, usize),
        from: usize,
    ) -> usize {
        assert!(
            history.len() + self.transcript.len() - from <= MAX_PTY_HISTORY,
            "bounded PTY history"
        );
        history.extend_from_slice(&self.transcript[from..]);
        let screen = screen_rows(&self.transcript, columns, rows);
        self.transcript.clear();
        for (row, line) in screen.iter().enumerate() {
            write!(self.transcript, "\x1b[{};1H{line}", row + 1).expect("screen checkpoint");
        }
        self.answered = PtyResponses::default();
        self.transcript.len()
    }

    // Reap the wrapper, then capture what it wrote before exiting.
    pub(crate) fn finish(&mut self) -> Output {
        let result = wait_product(self.child.take());
        self.pump();
        result
    }

    // The total page count once linear page `current` from `start` shows.
    pub(crate) fn wait_linear_page(&mut self, (start, current): (usize, usize)) -> usize {
        let marker = format!("Page {current}/");
        let deadline = Instant::now() + PTY_STAGE_WAIT;
        loop {
            if let Some(after) = String::from_utf8_lossy(&self.transcript[start..])
                .split_once(&marker)
                .and_then(|(_, rest)| rest.split_once("\r\n").map(|(line, _)| line))
                && let Ok(total) = after
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
            {
                assert!(
                    (current..=MAX_LINEAR_PAGES).contains(&total),
                    "bounded setup pages"
                );
                return total;
            }
            self.pump();
            assert!(
                Instant::now() < deadline,
                "setup review page incomplete: {}",
                tail(&self.transcript)
            );
            thread::yield_now();
        }
    }

    // Enter through every linear page from `start`, then await `next`; the page count and the
    // offset before the last acknowledgement.
    pub(crate) fn acknowledge_linear_pages(
        &mut self,
        (start, next): (usize, &[u8]),
    ) -> (usize, usize) {
        let total = self.wait_linear_page((start, 1));
        let mut next_at = self.transcript.len();
        for current in 1..=total {
            next_at = self.send(b"\r");
            if current < total {
                assert_eq!(self.wait_linear_page((next_at, current + 1)), total);
            }
        }
        self.wait_for_after(next_at, next);
        if next != b"Page 1/" {
            self.wait_for_after(next_at, b"Input:\r\n");
        }
        (total, next_at)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn adopt_product(&mut self, state: &Path) -> Pid {
        let pid = product_child_of(self.shell_pid());
        self.guard_product(ProductGuard::new(pid, state.to_owned()));
        pid
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn guard_product(&mut self, guard: ProductGuard) {
        self.product = Some(guard);
    }

    pub(crate) fn shell_pid(&self) -> u32 {
        String::from_utf8_lossy(&self.transcript)
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse()
            .expect("shell PID number")
    }
}

pub(crate) fn screen_contains(
    transcript: &[u8],
    columns: usize,
    rows: usize,
    needle: &str,
) -> bool {
    screen_rows(transcript, columns, rows)
        .join(" ")
        .contains(needle)
}
