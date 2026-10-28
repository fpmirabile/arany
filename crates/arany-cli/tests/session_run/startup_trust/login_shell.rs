use std::io::Write;
use std::path::Path;
use std::process::{ChildStdin, ChildStdout};

use super::{shows, until};

pub(super) fn review_login_shell_capture(
    output: &mut ChildStdout,
    input: &mut ChildStdin,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    state: &Path,
) {
    macro_rules! review_capture {
        () => {
            loop {
                until(
                    output,
                    input,
                    transcript,
                    answered,
                    "capture approval review",
                    &|screen| shows("Read all pages")(screen) || shows("Tab/Enter")(screen),
                );
                if shows("Tab/Enter")(transcript.as_slice()) {
                    break;
                }
                let page = crate::session_picker::screen_rows(transcript.as_slice(), 80, 24)
                    .into_iter()
                    .find(|line| line.contains("PgDn/Enter"));
                input.write_all(b"\r").unwrap();
                until(
                    output,
                    input,
                    transcript,
                    answered,
                    "next capture review page",
                    &|screen| {
                        shows("Tab/Enter")(screen)
                            || (shows("Read all pages")(screen)
                                && crate::session_picker::screen_rows(screen, 80, 24)
                                    .into_iter()
                                    .find(|line| line.contains("PgDn/Enter"))
                                    != page)
                    },
                );
            }
        };
    }
    let environment_file = state.join("command-environment.json");
    let previous = std::fs::read(&environment_file).unwrap();
    input.write_all(b"\x1b[B\r").unwrap();
    review_capture!();
    input.write_all(b"\r").unwrap();
    until(
        output,
        input,
        transcript,
        answered,
        "capture declined",
        &shows("Login-shell capture declined"),
    );
    assert_eq!(std::fs::read(&environment_file).unwrap(), previous);
    input.write_all(b"/permissions\r").unwrap();
    until(
        output,
        input,
        transcript,
        answered,
        "capture area",
        &shows("Network domains"),
    );
    input.write_all(b"\x1b[B\x1b[B\x1b[B\r").unwrap();
    until(
        output,
        input,
        transcript,
        answered,
        "capture page",
        &shows("Select a name from the Arany process"),
    );
    input.write_all(b"\x1b[B\r").unwrap();
    review_capture!();
    input.write_all(b"\x1b[B\r").unwrap();
    until(
        output,
        input,
        transcript,
        answered,
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
        transcript.as_slice(),
        std::fs::read(state.join("events.sqlite3")).unwrap().as_slice(),
    ] {
        assert!(
            !bytes
                .windows(b"CAPTURE_SECRET_CANARY".len())
                .any(|part| part == b"CAPTURE_SECRET_CANARY")
        );
    }
    input.write_all(b"/permissions\r").unwrap();
    until(
        output,
        input,
        transcript,
        answered,
        "environment cancellation area",
        &shows("Network domains"),
    );
    input.write_all(b"\x1b[B\x1b[B\x1b[B\r").unwrap();
    until(
        output,
        input,
        transcript,
        answered,
        "command environment page",
        &shows("Select a name from the Arany process"),
    );
}
