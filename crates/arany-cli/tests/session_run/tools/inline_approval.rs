use arany::{RunStatus, StateRoot, Store, ToolDisposition};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[test]
#[cfg_attr(not(arany_ci), ignore = "native Linux inline approval PTY and Guard")]
fn numbered_tool_approval_dispatches_only_after_review_and_an_explicit_choice() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    for case in [
        "yes",
        "escape",
        "early-digit",
        "typeahead",
        "linear-yes",
        "linear-no",
        "cancel",
        "unguarded",
        "unguarded-page-up",
        "unguarded-pages",
    ] {
        run_case(case);
    }
}

fn run_case(case: &'static str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let tls = crate::tls_fixture::build(root);
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.join("state");
    StateRoot::admit(&state).unwrap();
    crate::seed_fullscreen(&state);
    crate::private_file(&state.join("permissions.json"), br#"{"permissions":{}}"#);
    crate::private_file(&state.join("tools.json"), &serde_json::to_vec(&json!({
        "version":1,"workspace_paths":["."],"write":true,
        "commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":crate::hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],
        "skills":[],"mcp":[]
    })).unwrap());
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = crate::loopback::ChildGuard::new(
        Command::new("/usr/bin/openssl")
            .args(["s_server", "-quiet", "-accept"])
            .arg(format!("127.0.0.1:{port}"))
            .arg("-cert")
            .arg(&tls.cert)
            .arg("-key")
            .arg(&tls.key)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "TLS startup");
        thread::yield_now();
    }
    let mut peer_out = server.child().stdout.take().unwrap();
    let flags = rustix::fs::fcntl_getfl(&peer_out).unwrap();
    rustix::fs::fcntl_setfl(&peer_out, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut peer_in = server.child().stdin.take().unwrap();
    let mut product = Command::new("/usr/bin/script");
    product.env_clear().env("PATH", "/usr/bin:/bin").env("SHELL", "/bin/sh")
        .env("TERM", if case.starts_with("linear") { "dumb" } else { "xterm" })
        .env("LD_PRELOAD", &tls.shim).env("ARANY_TEST_TLS_PORT", port.to_string())
        .env("SSL_CERT_FILE", &tls.ca).env("OPENAI_API_KEY", "synthetic-native-key")
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state).env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_ROWS", if case == "unguarded-page-up" { "50" } else { "24" })
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", "stty rows \"$ARANY_TEST_ROWS\" cols 80; exec \"$ARANY_TEST_EXE\" --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4 --tools approval", "/dev/null"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = crate::loopback::ChildGuard::new(product.spawn().unwrap());
    let input = Arc::new(Mutex::new(child.child().stdin.take().unwrap()));
    let peer_input = input.clone();
    let (typeahead_delivered, delivery) = mpsc::sync_channel(1);
    let peer = thread::spawn(move || {
        while let Some(request) = crate::active_terminal::tool_group::tls_request(&mut peer_out) {
            let end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: Value = serde_json::from_slice(&request[end..]).unwrap();
            let step = crate::semantic::tool_results(&body).len();
            if case == "typeahead" && step == 0 {
                peer_input.lock().unwrap().write_all(b"\r").unwrap();
                delivery
                    .recv_timeout(Duration::from_secs(5))
                    .expect("product handled typeahead Enter before Provider response");
            }
            let command = if case == "unguarded-pages" {
                // The argument alone wraps past one 24-row page, within the 4096-byte argument limit.
                format!("exit 42;#{}", "padding ".repeat(200))
            } else if case.starts_with("unguarded") {
                "exit 42".to_owned()
            } else if case == "early-digit" {
                // Multi-page, yet within the 4096-byte Command argument limit.
                format!("printf approval-dispatched;#{}", "padding ".repeat(450))
            } else {
                "printf approval-dispatched".into()
            };
            let (name, arguments) = if case.starts_with("unguarded") && step == 1 {
                (
                    "arany_command_unguarded",
                    json!({"program":"bash","args":["-c",command],"cwd":"","justification":"Run this exact failed command on the host"}),
                )
            } else {
                (
                    "arany_command",
                    json!({"program":"bash","args":["-c",command],"cwd":""}),
                )
            };
            let output = if step >= if case.starts_with("unguarded") { 2 } else { 1 } {
                arany::semantic::responses_output(&json!("APPROVAL_FINAL"))
            } else {
                json!([{"type":"function_call","id":format!("fc_{step}"),"call_id":format!("call_{step}"),"status":"completed","name":name,"arguments":arguments.to_string()}])
            };
            let response = json!({"id":format!("resp_{step}"),"status":"completed","model":"gpt-5.4","output":output,"usage":{"input_tokens":12,"output_tokens":8}});
            let (kind, response) = if body["stream"] == true {
                (
                    "text/event-stream",
                    crate::loopback::responses_stream(response),
                )
            } else {
                ("application/json", response.to_string())
            };
            if write!(peer_in, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).and_then(|()| peer_in.flush()).is_err() { break; }
        }
    });
    let pipe: std::os::fd::OwnedFd = child.child().stdout.take().unwrap().into();
    let mut driver = Driver {
        wrapper_pid: child.child().id(),
        typeahead_delivered: (case == "typeahead").then_some(typeahead_delivered),
        ..Default::default()
    };
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::process::capture_terminal(
            child.child(),
            std::fs::File::from(pipe),
            Duration::from_secs(60),
            256 * 1024,
            |bytes| driver.advance(case, bytes, &input, &state, &workspace),
        )
    }))
    .unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let message: String = sanitized(&message).chars().take(200).collect();
        driver.annotate(case, &format!("capture; panic={message}"));
        std::panic::resume_unwind(panic)
    });
    drop(server);
    peer.join().unwrap();
    if !output.status.success() || !driver.answered {
        driver.annotate(case, "exit");
    }
    assert!(output.status.success(), "{case}: attached exit");
    assert!(driver.answered, "{case}: approval frame missing");
    assert_replay(&state, &workspace, case);
}

fn sanitized(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " _=:,.-()[]".contains(c) {
                c
            } else {
                '.'
            }
        })
        .collect()
}

#[derive(Default)]
struct Driver {
    wrapper_pid: u32,
    typeahead_delivered: Option<mpsc::SyncSender<()>>,
    product: Option<crate::active_terminal::ProductGuard>,
    declined: bool,
    answered: bool,
    early: bool,
    closed: bool,
    pages: usize,
    last_frame: String,
    output: Vec<u8>,
}

impl Driver {
    fn annotate(&self, case: &str, stage: &str) {
        let rows = if case == "unguarded-page-up" { 50 } else { 24 };
        let screen = crate::session_picker::screen_rows(&self.output, 80, rows)
            .iter()
            .map(|row| row.trim_end())
            .filter(|row| !row.is_empty())
            .map(|row| format!("[{row}]"))
            .collect::<String>();
        let tail = sanitized(
            &screen
                .chars()
                .rev()
                .take(1000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>(),
        );
        crate::process::annotate_native_fixture(&format!(
            "fixture=inline_approval; case={case}; stage={stage}; answered={}; early={}; pages={}; closed={}; bytes={}; screen={tail}",
            self.answered,
            self.early,
            self.pages,
            self.closed,
            self.output.len()
        ));
    }

    fn advance(
        &mut self,
        case: &str,
        bytes: &[u8],
        input: &Arc<Mutex<std::process::ChildStdin>>,
        state: &std::path::Path,
        workspace: &std::path::Path,
    ) {
        if self.product.is_none()
            && let Some(pid) = crate::active_terminal::try_product_child_of_executable(
                self.wrapper_pid,
                std::path::Path::new(env!("CARGO_BIN_EXE_arany")),
            )
        {
            self.product = Some(crate::active_terminal::ProductGuard::new(
                pid,
                state.to_path_buf(),
            ));
        }
        if bytes.len() != self.output.len() {
            self.output.clear();
            self.output.extend_from_slice(bytes);
        }
        let mut input = input.lock().unwrap();
        crate::process::decline_workspace_consent(&mut *input, bytes, &mut self.declined);
        let text = String::from_utf8_lossy(bytes);
        // This notice comes from handling empty Enter, proving delivery through the PTY relay.
        if self.typeahead_delivered.is_some()
            && crate::session_picker::screen_rows(bytes, 80, 24)
                .iter()
                .any(|row| row.contains("Run is active; Ctrl+C cancels"))
        {
            self.typeahead_delivered.take().unwrap().send(()).unwrap();
        }
        let frame_end: &[u8] = if case.starts_with("linear") {
            b"empty Enter denies:\r\n"
        } else {
            b"\x1b[?25l"
        };
        // The question row can arrive before the rest of the approval frame.
        let frame_bytes = bytes
            .windows(frame_end.len())
            .rposition(|part| part == frame_end)
            .map_or(&[][..], |end| &bytes[..end + frame_end.len()]);
        let frame = if case.starts_with("linear") {
            String::from_utf8_lossy(frame_bytes).into_owned()
        } else {
            crate::session_picker::screen_rows(
                frame_bytes,
                80,
                if case == "unguarded-page-up" { 50 } else { 24 },
            )
            .join("\n")
        };
        // Choices act only once armed; linear prints its prompt only then.
        let armed = case.starts_with("linear")
            || (1..=3).any(|choice| frame.contains(&format!("› {choice}. ")));
        // History rows redraw under the dialog, so only a changed dialog is a new step.
        let dialog = frame
            .rfind("Tool approval")
            .map_or(frame.as_str(), |start| &frame[start..]);
        if dialog != self.last_frame
            && armed
            && frame.contains("Do you want to proceed?")
            && !self.answered
        {
            self.last_frame = dialog.to_owned();
            // The guarded attempt is approved once its last page shows; later frames await the re-run.
            if case.starts_with("unguarded") && !frame.contains("Run outside the Guard?") {
                if !self.early && frame.contains("more lines") {
                    input.write_all(b"\x1b[6~").unwrap();
                } else if !self.early {
                    input.write_all(b"1").unwrap();
                    self.early = true;
                }
                return;
            }
            if frame.contains("more lines") || frame.contains("Review the whole action first") {
                if case == "early-digit" && !self.early {
                    input.write_all(b"1").unwrap();
                    self.early = true;
                    return;
                }
                if case == "early-digit" && frame.contains("Review the whole action first") {
                    assert_pending(state, workspace);
                }
                self.pages += 1;
                assert!(self.pages < 100);
                input.write_all(b"\x1b[6~").unwrap();
                return;
            }
            if case.starts_with("unguarded") {
                assert!(frame.contains("› 3. No"), "unguarded opens with No focused");
            }
            if case == "unguarded-page-up" {
                assert_eq!(self.pages, 0, "PageUp is tested on page zero");
            } else if case == "unguarded-pages" {
                assert!(
                    self.pages > 0,
                    "backward and forward navigation requires multiple pages"
                );
            }
            let answer: &[u8] = match case {
                "escape" | "typeahead" => b"\x1b",
                "early-digit" | "yes" => b"1",
                "linear-yes" => b"1\r",
                "linear-no" => b"no\r",
                "cancel" => b"\x03",
                "unguarded" => b"2",
                "unguarded-page-up" => b"\x1b[5~\r",
                "unguarded-pages" => b"\x1b[5~\x1b[6~\r",
                _ => unreachable!(),
            };
            input.write_all(answer).unwrap();
            self.answered = true;
        }
        if !self.answered || self.closed {
            return;
        }
        // Every terminal Run notice: finish, denial, cancellation (which pauses the Goal) and failure.
        // The replay assertions check which one each case reached.
        const OUTCOMES: [&str; 5] = [
            "APPROVAL_FINAL",
            "Action denied or approval expired",
            "Run cancelled",
            "Goal paused",
            "Run failed",
        ];
        // Ctrl+D does not end active work, so wait until the Run has returned to the idle composer.
        let idle = if case.starts_with("linear") {
            OUTCOMES
                .iter()
                .filter_map(|outcome| text.find(outcome))
                .min()
                .is_some_and(|end| text[end..].contains("Input:"))
        } else {
            // Diff rendering can split a row's bytes, so match the rendered screen.
            let rows = crate::session_picker::screen_rows(
                bytes,
                80,
                if case == "unguarded-page-up" { 50 } else { 24 },
            );
            rows.iter()
                .any(|row| OUTCOMES.iter().any(|outcome| row.contains(outcome)))
                && rows
                    .iter()
                    .any(|row| row.trim_start().starts_with("ready · "))
        };
        if idle {
            input.write_all(b"\x04").unwrap();
            self.closed = true;
        }
    }
}

fn assert_replay(state: &std::path::Path, workspace: &std::path::Path, case: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_path_buf(),
        )
        .await
        .unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
        assert_eq!(sessions.len(), 1);
        let events = store.load_session(sessions[0].id).await.unwrap();
        let view = arany::SessionView::replay(sessions[0].id, &events)
            .unwrap()
            .unwrap();
        let run = &view.runs[0];
        let denied = matches!(
            case,
            "escape" | "typeahead" | "linear-no" | "unguarded-page-up" | "unguarded-pages"
        );
        assert_eq!(
            run.status,
            if case == "cancel" {
                RunStatus::Cancelled
            } else if denied {
                RunStatus::Failed
            } else {
                RunStatus::Finished
            },
            "{case}"
        );
        let observation = run.tools.last().unwrap().observation.as_ref().unwrap();
        if denied || case == "cancel" {
            assert!(observation.attestation.is_none(), "{case}: no dispatch");
            if case.starts_with("unguarded") {
                assert!(!state.join("unguarded-commands.json").exists());
            }
        } else if case.starts_with("unguarded") {
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::Unguarded(_))
            ));
            assert!(state.join("unguarded-commands.json").exists());
        } else {
            assert_eq!(observation.disposition, ToolDisposition::Succeeded);
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::Guard(_))
            ));
        }
        store.close().await.unwrap();
    });
}

fn assert_pending(state: &std::path::Path, workspace: &std::path::Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_path_buf(),
        )
        .await
        .unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
        let view = store.load_open_view(sessions[0].id).await.unwrap().unwrap();
        let tool = view.runs[0].tools.last().unwrap();
        assert!(
            tool.permission_verdict.is_none() && tool.observation.is_none(),
            "an early digit must not record a verdict or dispatch"
        );
        store.close().await.unwrap();
    });
}
