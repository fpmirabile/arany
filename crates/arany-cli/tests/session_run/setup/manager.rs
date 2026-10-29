use super::*;

const COLUMNS: usize = 80;
const ROWS: usize = 24;
const MAX_REVIEW_PAGES: usize = 128;

#[derive(Clone, Copy)]
pub(super) enum Presentation {
    ScreenReader,
    Inline,
}

impl Presentation {
    fn start(
        self,
        temp: &tempfile::TempDir,
        workspace: &Path,
        state: &Path,
        key: Option<&str>,
    ) -> Pty {
        let mode = match self {
            Self::ScreenReader => SetupPtyMode::Manager,
            Self::Inline => SetupPtyMode::InlineManager,
        };
        start_setup_pty_with_key(temp, workspace, state, mode, key)
    }

    pub(super) fn choose(self, pty: &mut Pty, choice: usize) -> usize {
        let from = pty.transcript.len();
        let keys = match self {
            Self::ScreenReader => format!("{choice}\r").into_bytes(),
            Self::Inline => [b"\x1b[B".repeat(choice - 1), b"\r".to_vec()].concat(),
        };
        pty.send(&keys);
        from
    }

    pub(super) fn menu(self, pty: &mut Pty, from: usize, title: &str) {
        match self {
            Self::ScreenReader => wait_setup_choices(pty, from),
            Self::Inline => {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut cursor = from;
                loop {
                    let screen = inline_frame(pty, cursor, title);
                    if screen.iter().any(|row| row.trim() == title) {
                        break;
                    }
                    cursor = pty.transcript.len();
                    assert!(Instant::now() < deadline, "manager title frame");
                }
            }
        }
    }

    pub(super) fn composer(self, pty: &mut Pty, from: usize) {
        match self {
            Self::ScreenReader => pty.wait_for_after(from, b"Input:\r\n"),
            Self::Inline => {
                inline_frame(pty, from, "Ask Arany");
            }
        }
    }
}

pub(super) fn inline_frame(pty: &mut Pty, from: usize, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(end) = pty.transcript[from..]
            .windows(6)
            .rposition(|part| matches!(part, b"\x1b[?25l" | b"\x1b[?25h"))
        {
            let screen = crate::screen_rows(&pty.transcript[..from + end + 6], COLUMNS, ROWS);
            if screen.join(" ").contains(needle) {
                return screen;
            }
        }
        pty.pump();
        assert!(
            Instant::now() < deadline,
            "completed setup frame missing {needle}: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
}

pub(super) fn acknowledge_result(
    pty: &mut Pty,
    presentation: Presentation,
    from: usize,
) -> (usize, String) {
    let mut text = String::new();
    let mut cursor = from;
    let total = match presentation {
        Presentation::ScreenReader => pty.wait_linear_page((cursor, 1)),
        Presentation::Inline => page_total(&inline_frame(pty, cursor, "Page 1/")),
    };
    assert!((1..=MAX_REVIEW_PAGES).contains(&total));
    for page in 1..=total {
        match presentation {
            Presentation::ScreenReader => {
                assert_eq!(pty.wait_linear_page((cursor, page)), total);
                text.push_str(&linear_result_page(pty, cursor, page, total));
            }
            Presentation::Inline => {
                let screen = inline_frame(pty, cursor, &format!("Page {page}/"));
                let header = screen
                    .iter()
                    .position(|row| row.contains(&format!("Page {page}/")))
                    .unwrap();
                let (_, body) = screen[header].split_once("\\u{000a}").unwrap();
                text.push_str(body.trim());
            }
        }
        cursor = pty.transcript.len();
        pty.send(b"\r");
    }
    (cursor, text)
}

fn linear_result_page(pty: &mut Pty, from: usize, page: usize, total: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let marker = format!("Choose: Page {page}/{total}\\u{{000a}}");
    loop {
        if let Some((_, body)) =
            String::from_utf8_lossy(&pty.transcript[from..]).split_once(&marker)
            && let Some((line, _)) = body.split_once("\r\n")
        {
            return line.to_owned();
        }
        pty.pump();
        assert!(Instant::now() < deadline, "complete linear result page");
        thread::yield_now();
    }
}

pub(super) fn assert_result_contains(text: &str, expected: &str) {
    let compact = |value: &str| {
        value
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };
    assert!(
        compact(text).contains(&compact(expected)),
        "acknowledged result missing expected status"
    );
}

fn page_total(screen: &[String]) -> usize {
    screen
        .iter()
        .find_map(|row| row.split_once("Page 1/").map(|(_, suffix)| suffix))
        .map(|suffix| {
            suffix
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn manager_empty_account_removal_waits_for_acknowledgment() {
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        let mut pty = presentation.start(&temp, &workspace, &state, None);
        let accounts = presentation.choose(&mut pty, 1);
        presentation.menu(&mut pty, accounts, "API accounts");
        for kind in [1, 2] {
            let removal = presentation.choose(&mut pty, 5);
            presentation.menu(&mut pty, removal, "Remove saved access");
            let result = presentation.choose(&mut pty, kind);
            let (accounts, message) = acknowledge_result(&mut pty, presentation, result);
            assert_result_contains(&message, "No saved account");
            presentation.menu(&mut pty, accounts, "API accounts");
        }
        presentation.choose(&mut pty, 6);
        finish_manager_pty(pty);
        if state.exists() {
            runtime().block_on(async {
                assert!(
                    arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace,)
                        .await
                        .unwrap()
                        .is_empty()
                );
            });
        }
    }
}

fn finish_manager_pty(pty: Pty) {
    let (result, transcript) = finish_setup_pty_with_transcript(pty);
    assert!(
        result.status.success(),
        "manager lifecycle failed: {:?}; {}",
        result.status.code(),
        tail(&transcript)
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
