use super::super::session_picker::{PtyResponses, pump, tail};
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
        let mut command = crate::isolation::account_isolated_script(temp.path());
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("ARANY_TEST_SESSION", id.to_string())
            .env("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22")
            .env("TERM", "xterm")
            .env("SHELL", "/bin/sh")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("copy PTY process"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = rustix::fs::fcntl_getfl(&output).expect("PTY output flags");
        rustix::fs::fcntl_setfl(&output, flags | rustix::fs::OFlags::NONBLOCK)
            .expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = PtyResponses::default();
        let mut wait = |input: &mut ChildStdin, transcript: &mut Vec<u8>, from, needle: &[u8]| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !if needle == CARET {
                crate::composer_caret(transcript, from, 80, 24)
                    .is_some_and(|(column, _)| column == 3)
            } else {
                transcript[from..]
                    .windows(needle.len())
                    .any(|part| part == needle)
            } {
                pump(&mut output, input, transcript, &mut answered);
                assert!(
                    Instant::now() < deadline,
                    "copy PTY (linear: {linear}) missing {}: {}",
                    tail(needle),
                    tail(transcript)
                );
                thread::yield_now();
            }
        };
        wait(&mut input, &mut transcript, 0, b"SHELL_PID:");
        let label = transcript
            .windows(b"SHELL_PID:".len())
            .position(|part| part == b"SHELL_PID:")
            .expect("shell PID label");
        wait(&mut input, &mut transcript, label, b"\r\n");
        let text = String::from_utf8_lossy(&transcript).into_owned();
        let pid = product_child_of(transcript_field(&text, "SHELL_PID:").parse().unwrap());
        let _product = ProductGuard::new(pid, state.clone());
        if linear {
            wait(&mut input, &mut transcript, 0, b"Input:\r\n");
        } else {
            wait(&mut input, &mut transcript, 0, DISPLAYED.as_bytes());
            wait(&mut input, &mut transcript, 0, CARET);
        }
        assert!(
            osc52_payloads(&transcript).is_empty(),
            "an answer's OSC 52 reached the terminal before any copy gesture"
        );

        let from = transcript.len();
        input.write_all(b"/copy\r").expect("copy command");
        if linear {
            wait(
                &mut input,
                &mut transcript,
                from,
                b"Notice: Copy unavailable: no native or tmux clipboard worked; OSC 52 unavailable in linear mode\r\n",
            );
            wait(&mut input, &mut transcript, from, b"Input:\r\n");
            assert!(
                osc52_payloads(&transcript).is_empty(),
                "linear mode wrote OSC 52"
            );
        } else {
            wait(&mut input, &mut transcript, from, b"\x07");
            let sent = from
                + transcript[from..]
                    .iter()
                    .position(|byte| *byte == 0x07)
                    .expect("OSC 52 terminator");
            wait(&mut input, &mut transcript, sent, CARET);
            let payloads = osc52_payloads(&transcript);
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

        input.write_all(b"/exit\r").expect("exit");
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.child().try_wait().expect("PTY status").is_none() {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "copy PTY exit: {}",
                tail(&transcript)
            );
            thread::yield_now();
        }
        assert!(wait_product(child.take()).status.success());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert_eq!(
            osc52_payloads(&transcript).len(),
            usize::from(!linear),
            "no further OSC 52 after the gesture"
        );
    }
}
