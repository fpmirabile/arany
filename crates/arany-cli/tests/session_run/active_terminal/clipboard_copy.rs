use super::super::session_picker::tail;
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

// Ratatui skips unchanged blank cells, so a spaceless canary is the only contiguous rendered text.
const ANSWER: &str = "COPY_CANARY\u{1b}]52;c;aGlqYWNr\u{7}done";
const DISPLAYED: &str = "COPY_CANARY\\u{001b}]52;c;aGlqYWNr\\u{0007}done";
// The idle composer caret, found by the composer border wherever the Classic viewport sits.
const CARET: &[u8] = b"<idle composer caret>";

fn osc52_payloads(bytes: &[u8]) -> Vec<&[u8]> {
    let mut payloads = Vec::new();
    let mut rest = bytes;
    while let Some(start) = rest.windows(4).position(|part| part == b"\x1b]52") {
        rest = &rest[start + 4..];
        let end = rest
            .iter()
            .position(|byte| *byte == 0x07)
            .unwrap_or(rest.len());
        payloads.push(&rest[..end]);
        rest = &rest[end..];
    }
    payloads
}

#[test]
fn copy_over_ssh_writes_one_osc52_on_stderr_and_linear_mode_writes_none() {
    for linear in [false, true] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let id = crate::seed_answer_session(&state, &workspace, ANSWER);
        // Stdout goes to /dev/null, so every PTY byte below comes from stderr.
        let shell = format!(
            "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; \"$ARANY_TEST_EXE\" {} --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" >/dev/null; exit \"$?\"",
            if linear {
                "--screen-reader"
            } else {
                "--classic"
            }
        );
        let mut pty = Pty::new(
            crate::product::PtyLaunch::new(
                crate::product::Isolation::Passwd(crate::isolation::account_wrapper(temp.path())),
                &shell,
            )
            .session(&state, &workspace)
            .env("ARANY_TEST_SESSION", id.to_string())
            .env("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22")
            .term("xterm")
            .spawn(),
        );
        let wait = |pty: &mut Pty, from, needle: &[u8]| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !if needle == CARET {
                crate::composer_caret(&pty.transcript, from, 80, 24)
                    .is_some_and(|(column, _)| column == 3)
            } else {
                pty.transcript[from..]
                    .windows(needle.len())
                    .any(|part| part == needle)
            } {
                pty.pump();
                assert!(
                    Instant::now() < deadline,
                    "copy PTY (linear: {linear}) missing {}: {}",
                    tail(needle),
                    tail(&pty.transcript)
                );
                thread::yield_now();
            }
        };
        wait(&mut pty, 0, b"SHELL_PID:");
        let label = pty
            .transcript
            .windows(b"SHELL_PID:".len())
            .position(|part| part == b"SHELL_PID:")
            .expect("shell PID label");
        wait(&mut pty, label, b"\r\n");
        let text = String::from_utf8_lossy(&pty.transcript).into_owned();
        let pid = product_child_of(transcript_field(&text, "SHELL_PID:").parse().unwrap());
        let _product = ProductGuard::new(pid, state.clone());
        if linear {
            wait(&mut pty, 0, b"Input:\r\n");
        } else {
            wait(&mut pty, 0, DISPLAYED.as_bytes());
            wait(&mut pty, 0, CARET);
        }
        assert!(
            osc52_payloads(&pty.transcript).is_empty(),
            "an answer's OSC 52 reached the terminal before any copy gesture"
        );

        let from = pty.send(b"/copy\r");
        if linear {
            wait(&mut pty,
                from,
                b"Notice: Copy unavailable: no native or tmux clipboard worked; OSC 52 unavailable in linear mode\r\n",
            );
            wait(&mut pty, from, b"Input:\r\n");
            assert!(
                osc52_payloads(&pty.transcript).is_empty(),
                "linear mode wrote OSC 52"
            );
        } else {
            wait(&mut pty, from, b"\x07");
            let sent = from
                + pty.transcript[from..]
                    .iter()
                    .position(|byte| *byte == 0x07)
                    .expect("OSC 52 terminator");
            wait(&mut pty, sent, CARET);
            let payloads = osc52_payloads(&pty.transcript);
            assert_eq!(payloads.len(), 1, "copy wrote one OSC 52 sequence");
            let encoded = payloads[0]
                .strip_prefix(b";c;")
                .expect("fixed clipboard selection");
            assert_eq!(
                STANDARD.decode(encoded).expect("base64 payload"),
                DISPLAYED.as_bytes(),
                "OSC 52 carries the escaped display text"
            );
        }

        pty.send(b"/exit\r");
        pty.wait_exit("copy PTY exit");
        assert!(pty.finish().status.success());
        assert_eq!(
            osc52_payloads(&pty.transcript).len(),
            usize::from(!linear),
            "no further OSC 52 after the gesture"
        );
    }
}
