use crate::pty::Pty;
use std::path::Path;

use super::{shows, until};

pub(super) fn review_login_shell_capture(pty: &mut Pty, state: &Path) {
    macro_rules! review_capture {
        () => {
            loop {
                until(pty, "capture approval review", &|screen| {
                    shows("Read all pages")(screen) || shows("Tab/Enter")(screen)
                });
                if shows("Tab/Enter")(pty.transcript.as_slice()) {
                    break;
                }
                let page = crate::session_picker::screen_rows(pty.transcript.as_slice(), 80, 24)
                    .into_iter()
                    .find(|line| line.contains("PgDn/Enter"));
                pty.send(b"\r");
                until(pty, "next capture review page", &|screen| {
                    shows("Tab/Enter")(screen)
                        || (shows("Read all pages")(screen)
                            && crate::session_picker::screen_rows(screen, 80, 24)
                                .into_iter()
                                .find(|line| line.contains("PgDn/Enter"))
                                != page)
                });
            }
        };
    }
    let environment_file = state.join("command-environment.json");
    let previous = std::fs::read(&environment_file).unwrap();
    pty.send(b"\x1b[B\r");
    review_capture!();
    pty.send(b"\r");
    until(
        pty,
        "capture declined",
        &shows("Login-shell capture declined"),
    );
    assert_eq!(std::fs::read(&environment_file).unwrap(), previous);
    pty.send(b"/permissions\r");
    until(pty, "capture area", &shows("Network domains"));
    pty.send(b"\x1b[B\x1b[B\x1b[B\r");
    until(
        pty,
        "capture page",
        &shows("Select a name from the Arany process"),
    );
    pty.send(b"\x1b[B\r");
    review_capture!();
    pty.send(b"\x1b[B\r");
    until(
        pty,
        "capture saved",
        &shows("Login-shell environment captured"),
    );
    let captured_bytes = std::fs::read(&environment_file).unwrap();
    let captured: serde_json::Value = serde_json::from_slice(&captured_bytes).unwrap();
    assert_eq!(captured["source"], "login-shell");
    assert_eq!(captured["path"], serde_json::json!(["/bin", "/usr/bin"]));
    assert!(captured["shell"]["sha256"].as_str().is_some());
    for bytes in [
        captured_bytes.as_slice(),
        pty.transcript.as_slice(),
        std::fs::read(state.join("events.sqlite3"))
            .unwrap()
            .as_slice(),
    ] {
        assert!(
            !bytes
                .windows(b"CAPTURE_SECRET_CANARY".len())
                .any(|part| part == b"CAPTURE_SECRET_CANARY")
        );
    }
    pty.send(b"/permissions\r");
    until(
        pty,
        "environment cancellation area",
        &shows("Network domains"),
    );
    pty.send(b"\x1b[B\x1b[B\x1b[B\r");
    until(
        pty,
        "command environment page",
        &shows("Select a name from the Arany process"),
    );
}
