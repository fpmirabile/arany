use super::*;

#[cfg(target_os = "linux")]
#[test]
#[ignore = "isolated clipboard product gate; requires bubblewrap user/PID/network namespaces"]
fn isolated_clipboard_clients_preserve_drafts_and_cancel_owned_processes() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::os::unix::net::UnixListener;

    const CLIENT: &str = r#"#!/usr/bin/sh
set -eu
[ "$(pwd)" = / ]
[ -z "${OPENAI_API_KEY+x}${ANTHROPIC_API_KEY+x}${HTTP_PROXY+x}${ARANY_TEST_EXE+x}${DBUS_SESSION_BUS_ADDRESS+x}" ]
arguments="$*"
if [ -f /data/client-pid ]; then
    previous=$(/usr/bin/cat /data/client-pid)
    [ ! -e "/proc/$previous" ]
fi
if [ -f /data/descendant-pid ]; then
    previous=$(/usr/bin/cat /data/descendant-pid)
    if [ -e "/proc/$previous/stat" ]; then
        set -- $(/usr/bin/cat "/proc/$previous/stat")
        [ "$3" = Z ]
    fi
fi
case "$0" in
    /usr/bin/wl-paste)
        [ "$XDG_RUNTIME_DIR" = /data/runtime ]
        [ "$WAYLAND_DISPLAY" = wayland-0 ]
        [ -z "${DISPLAY+x}${XAUTHORITY+x}" ]
        case "$arguments" in
            --list-types) action=types ;;
            '--no-newline --type image/png'|'--no-newline --type text/plain') action=payload ;;
            *) exit 31 ;;
        esac ;;
    /usr/bin/xclip)
        [ "$DISPLAY" = :99.0 ]
        [ "$XAUTHORITY" = /data/authority ]
        [ "$HOME" = / ]
        [ -z "${XDG_RUNTIME_DIR+x}${WAYLAND_DISPLAY+x}" ]
        case "$arguments" in
            '-selection clipboard -out -target TARGETS') action=types ;;
            '-selection clipboard -out -target image/png'|'-selection clipboard -out -target text/plain') action=payload ;;
            *) exit 32 ;;
        esac ;;
    *) exit 33 ;;
esac
printf '%s\n' "$action" >> /data/client-log
if [ "$action" = types ]; then
    exec /usr/bin/cat /data/types
fi
printf '%s\n' "$$" > /data/client-pid
mode=$(/usr/bin/cat /data/mode)
case "$mode" in
    success) exec /usr/bin/cat /data/payload ;;
    stderr) exec /usr/bin/cat /data/payload >&2 ;;
    failure) exit 1 ;;
    held)
        /usr/bin/cat /data/hold &
        printf '%s\n' "$!" > /data/descendant-pid
        printf 'ready\n' > /data/ready
        wait ;;
    *) exit 34 ;;
esac
"#;
    for wayland in [true, false] {
        let temp = tempfile::tempdir().expect("private clipboard fixture");
        let dir = temp.path();
        let workspace = dir.join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        std::fs::write(
            dir.join("passwd"),
            b"root:x:0:0:Synthetic owner:/data:/usr/bin/sh\n",
        )
        .expect("isolated OS account");
        let listener = UnixListener::bind(dir.join("bus")).expect("synthetic credential bus");
        listener.set_nonblocking(true).expect("nonblocking bus");
        StateRoot::admit(&dir.join(".local/state/arany"))
            .expect("isolated account root")
            .record(RecordKind::SavedAccount)
            .replace(br#"{"storage":"keyring","account":null}"#)
            .expect("credential-free backend marker");
        std::fs::create_dir(dir.join("runtime")).expect("private Wayland runtime");
        std::fs::set_permissions(dir.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .expect("runtime mode");
        let _socket =
            UnixListener::bind(dir.join("runtime/wayland-0")).expect("synthetic Wayland socket");
        std::fs::write(dir.join("authority"), b"synthetic X authority")
            .expect("private X authority");
        std::fs::set_permissions(
            dir.join("authority"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("authority mode");
        std::fs::write(dir.join("client"), CLIENT).expect("synthetic clipboard client");
        std::fs::set_permissions(dir.join("client"), std::fs::Permissions::from_mode(0o755))
            .expect("client mode");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            dir.join("hold"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("held clipboard FIFO");
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC")
            .expect("synthetic PNG");
        let saved_id =
            saved_conversation(&dir.join("state"), &workspace, SessionDefaults::default());
        let shell = format!(
            "trap ':' INT; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; /arany --screen-reader --provider openai --model gpt-5.4 --resume {saved_id} --state-dir /data/state --workspace /data/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$code\""
        );
        let mut command = sandbox(&["--unshare-net"], dir, Path::new("/data"));
        command.env_clear().current_dir(dir);
        for client in ["wl-paste", "xclip"] {
            command
                .arg("--ro-bind")
                .arg(dir.join("client"))
                .arg(format!("/usr/bin/{client}"));
        }
        command.args([
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin",
            "--setenv",
            "SHELL",
            "/usr/bin/sh",
            "--setenv",
            "TERM",
            "dumb",
            "--setenv",
            "OPENAI_API_KEY",
            "synthetic-clipboard-key",
            "--setenv",
            "ANTHROPIC_API_KEY",
            "omitted-clipboard-key",
            "--setenv",
            "HTTP_PROXY",
            "http://127.0.0.1:9",
            "--setenv",
            "ARANY_TEST_EXE",
            "must-not-forward",
            "--setenv",
            "HOME",
            "/data",
            "--setenv",
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/data/bus",
        ]);
        if wayland {
            command.args([
                "--setenv",
                "XDG_RUNTIME_DIR",
                "/data/runtime",
                "--setenv",
                "WAYLAND_DISPLAY",
                "wayland-0",
            ]);
        } else {
            command.args([
                "--setenv",
                "DISPLAY",
                ":99.0",
                "--setenv",
                "XAUTHORITY",
                "/data/authority",
            ]);
        }
        command.args(["--chdir", "/data/workspace", "--"]);
        let mut pty = Pty::new(
            crate::product::PtyLaunch::new(crate::product::Isolation::Passwd(command), &shell)
                .spawn(),
        );
        pty.wait_for(b"Input:\r\n");
        assert!(!dir.join("client-log").exists(), "startup read clipboard");
        let offer = |types: &[u8], mode: &str, payload: &[u8]| {
            std::fs::write(dir.join("types"), types).expect("offered clipboard types");
            std::fs::write(dir.join("mode"), mode).expect("client mode");
            std::fs::write(dir.join("payload"), payload).expect("clipboard payload");
            std::fs::write(dir.join("client-log"), b"").expect("fresh client log");
        };
        let client_calls = || std::fs::read(dir.join("client-log")).expect("client calls");
        let large_text = "synthetic paste ".repeat(600);
        let large_preview = format!(
            "Draft (not sent):\r\n  [Pasted Content 9600 chars]\r\nExpanded text (not sent):\r\n  {large_text}\r\nDraft: 9600 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n"
        );
        for (types, mode, payload, expected, calls) in [
            (
                "text/plain\n",
                "success",
                b"first\nsecond".as_slice(),
                "Draft (not sent):\r\n  first\r\n  second\r\nDraft: 12 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n",
                "types\npayload\n",
            ),
            (
                "text/plain\n",
                "success",
                large_text.as_bytes(),
                large_preview.as_str(),
                "types\npayload\n",
            ),
            (
                "image/png\ntext/plain\n",
                "success",
                png.as_slice(),
                "Draft (not sent):\r\n  \r\n  Image 1: PNG 1x1, 69 B\r\nDraft: 0 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n",
                "types\npayload\n",
            ),
        ] {
            offer(types.as_bytes(), mode, payload);
            pty.send_and_wait(b"/paste\r", expected.as_bytes());
            assert_eq!(client_calls(), calls.as_bytes());
            if types == "text/plain\n" {
                pty.send_and_wait(b"\x03", b"Input:\r\n");
            }
        }
        pty.send_and_wait(b"/new\r", b"Notice: Error: Submit or clear image attachments before switching Sessions\r\nInput:\r\n");
        for (types, mode, payload, diagnostic, calls) in [
            (
                b"image/jpeg\ntext/plain\n".to_vec(),
                "success",
                b"filename.png".to_vec(),
                "clipboard image format is unsupported; use PNG",
                "types\n",
            ),
            (
                b"image/png\n".to_vec(),
                "success",
                b"invalid PNG".to_vec(),
                "invalid or unsupported PNG image",
                "types\npayload\n",
            ),
            (
                b"image/png\n".to_vec(),
                "success",
                vec![0; arany::MAX_IMAGE_BYTES + 1],
                "clipboard output exceeds its limit",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "success",
                vec![b'x'; 65_537],
                "clipboard output exceeds its limit",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "success",
                vec![0xff],
                "clipboard text is not valid UTF-8",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "success",
                b"\x1b[31m/exit".to_vec(),
                "paste contains unsupported control characters",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "stderr",
                b"PRIVATE_CLIPBOARD_ERROR".repeat(256),
                "clipboard output exceeds its limit",
                "types\npayload\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "failure",
                Vec::new(),
                "clipboard read unavailable",
                "types\npayload\n",
            ),
            (
                vec![b'x'; 16 * 1024 + 1],
                "success",
                Vec::new(),
                "clipboard output exceeds its limit",
                "types\n",
            ),
            (
                b"text/plain\n".to_vec(),
                "held",
                Vec::new(),
                "clipboard read cancelled or timed out",
                "types\npayload\n",
            ),
        ] {
            offer(&types, mode, &payload);
            pty.send_and_wait(
                b"/paste\r",
                format!("Notice: Error: {diagnostic}; draft unchanged\r\nInput:\r\n").as_bytes(),
            );
            assert_eq!(client_calls(), calls.as_bytes());
        }
        std::fs::write(dir.join("types"), b"image/png\n").expect("offered PNG");
        std::fs::write(dir.join("mode"), b"success").expect("successful client");
        std::fs::write(dir.join("payload"), &png).expect("valid PNG");
        pty.send_and_wait(b"/paste\r", b"Image 2: PNG 1x1, 69 B\r\nDraft: 0 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n");
        pty.send_and_wait(b"\x03", b"Input:\r\n");
        std::fs::write(dir.join("types"), b"text/plain\n").expect("held text offer");
        std::fs::write(dir.join("mode"), b"held").expect("held client");
        std::fs::write(dir.join("ready"), b"").expect("new cancellation readiness");
        pty.send(b"/paste\r");
        pty.pump_until("held clipboard client did not start", |_| {
            std::fs::read(dir.join("ready")).ok().as_deref() == Some(b"ready\n")
        });
        pty.send_and_wait(
            b"kept\x04",
            b"Notice: Draft: 4 characters; Enter submits\r\nInput:\r\n",
        );
        pty.send_and_wait(
            b"\x03",
            b"Notice: Clipboard read cancelled; draft retained\r\nInput:\r\n",
        );
        pty.send_and_wait(
            b"\x04",
            b"Notice: Draft retained; press Enter to submit or Ctrl+C to clear\r\nInput:\r\n",
        );
        pty.send_and_wait(b"\x03", b"Input:\r\n");
        std::fs::write(dir.join("mode"), b"success").expect("recover after cancellation");
        std::fs::write(dir.join("payload"), b"recovered").expect("post-cancel text");
        pty.send_and_wait(b"/paste\r", b"Draft (not sent):\r\n  recovered\r\nDraft: 9 characters; Enter submits when idle; Ctrl+C clears when idle\r\nInput:\r\n");
        pty.send_and_wait(b"\x03", b"Input:\r\n");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("public lifecycle runtime");
        let state = dir.join("state");
        let session_id = runtime.block_on(async {
            let sessions = list_sessions(
                StateRoot::open_existing(&state).expect("State"),
                workspace.clone(),
            )
            .await
            .expect("current Session");
            assert_eq!(sessions.len(), 1);
            sessions[0].id
        });
        let defaults = arany::SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: Some(uuid::Uuid::now_v7()),
            policy: Default::default(),
        };
        runtime
            .block_on(arany::set_session_defaults(
                StateRoot::open_existing(&state).expect("State"),
                workspace.clone(),
                session_id,
                defaults.clone(),
            ))
            .expect("synthetic saved-account selection");
        let prefix = runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("prefix Store");
            let events = store.load_session(session_id).await.expect("prefix Events");
            store.close().await.expect("close prefix");
            events
        });
        assert_eq!(prefix.len(), 3, "clipboard persisted transient data");
        pty.send_and_wait(
            format!("/resume {session_id}\r").as_bytes(),
            b"Notice: Resumed: Empty conversation\r\nInput:\r\n",
        );
        let from = pty.send(b"synthetic credential-wait objective\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    pty.pump();
                    assert!(
                        Instant::now() < deadline,
                        "credential helper did not connect"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("synthetic bus accept: {error}"),
            }
        };
        pty.wait_for_after(from, b"Notice: Preparing request... Ctrl+C cancels\r\n");
        std::fs::write(dir.join("types"), b"image/png\n").expect("preparing PNG offer");
        std::fs::write(dir.join("payload"), &png).expect("preparing PNG");
        let from = pty.send_and_wait(b"/paste\r", b"Draft (not sent):\r\n  \r\n  Image 1: PNG 1x1, 69 B\r\nDraft: 0 characters; Enter submits when idle; Ctrl+C clears when idle\r\n");
        let pasted = String::from_utf8_lossy(&pty.transcript[from..]);
        assert!(
            !pasted.contains("Error:"),
            "paste waited for admission failure"
        );
        assert!(
            !pasted.contains("Input:\r\n"),
            "busy paste printed an idle prompt"
        );
        connection
            .set_nonblocking(true)
            .expect("nonblocking handshake");
        let mut observed = 0;
        loop {
            let mut bytes = [0; 1024];
            match connection.read(&mut bytes) {
                Ok(0) => panic!("paste preview waited for credential helper exit"),
                Ok(count) => {
                    observed += count;
                    assert!(observed <= 4096, "bounded credential handshake");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("synthetic handshake: {error}"),
            }
        }
        pty.send_and_wait(b"\x03", b"Notice: Submission interrupted before Run admission; no task was accepted. Account work already started may still complete.\r\nInput:\r\n");
        pty.send_and_wait(
            b"\x04",
            b"Notice: Draft retained; press Enter to submit or Ctrl+C to clear\r\nInput:\r\n",
        );
        pty.send_and_wait(b"\x03", b"Input:\r\n");
        pty.send(b"/exit\r");
        assert!(pty.wait_exit("clipboard namespace did not exit").success());
        pty.pump();
        for omitted in [
            b"PRIVATE_CLIPBOARD_ERROR".as_slice(),
            b"synthetic-clipboard-key",
            b"omitted-clipboard-key",
            b"iVBORw0KGgo",
        ] {
            assert!(
                !pty.transcript
                    .windows(omitted.len())
                    .any(|part| part == omitted),
                "private clipboard bytes escaped"
            );
        }
        let text = String::from_utf8_lossy(&pty.transcript);
        assert_tty_restored(&text);
        connection
            .set_nonblocking(false)
            .expect("blocking final handshake");
        connection
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("closed-helper deadline");
        loop {
            let mut bytes = [0; 1024];
            let count = connection
                .read(&mut bytes)
                .expect("credential helper closed");
            if count == 0 {
                break;
            }
            observed += count;
            assert!(observed <= 4096, "bounded final credential handshake");
        }
        runtime.block_on(async {
            let state = dir.join("state");
            let root = StateRoot::open_existing(&state).expect("closed State");
            let sessions = list_sessions(root, workspace).await.expect("Session list");
            assert_eq!(sessions.len(), 1, "image switching created a Session");
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("read-only Store");
            let events = store
                .load_session(sessions[0].id)
                .await
                .expect("closed Events");
            let view = arany::SessionView::replay(sessions[0].id, &events)
                .expect("strict replay")
                .expect("Session");
            assert!(view.runs.is_empty(), "clipboard started a Run");
            assert_eq!(
                events, prefix,
                "clipboard or preparation changed the prefix"
            );
            assert_eq!(view.defaults, defaults);
            store.close().await.expect("close Store");
        });
    }
}

#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(not(arany_ci), ignore = "requires bubblewrap user namespaces and PTY")]
fn pasted_mentions_stay_data_and_legacy_continuation_explains_missing_includes() {
    use super::loopback::{read_request, send_response, write_profile};
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;

    for (linear, prefix) in [(false, ""), (true, ""), (false, "//literal ")] {
        let temp = tempfile::tempdir().expect("private attachment fixture");
        let root = temp.path();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("private.txt"), "OMITTED_ATTACHMENT_CANARY").unwrap();
        let state = root.join("state");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        write_profile(&state, listener.local_addr().unwrap().port());
        let payload = format!("@private.txt {}", "ordinary text ".repeat(700));
        let expected = format!(
            "{}{payload} typed suffix",
            prefix.strip_prefix('/').unwrap_or(prefix)
        );
        let expected_request = expected.clone();
        let (submit, submitted) = std::sync::mpsc::sync_channel(1);
        let (requests_sent, requests) = std::sync::mpsc::sync_channel(1);
        let server = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            submitted
                .recv_timeout(Duration::from_secs(30))
                .expect("attachment submission gate");
            {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing synthetic request");
                            thread::yield_now();
                        }
                        Err(error) => panic!("loopback accept: {error}"),
                    }
                };
                let body = read_request(&mut stream);
                assert!(!String::from_utf8_lossy(&body).contains("OMITTED_ATTACHMENT_CANARY"));
                let wire = serde_json::from_slice(&body).unwrap();
                let input = crate::semantic::input(&wire);
                assert_eq!(input["objective"], expected_request);
                assert_eq!(input["includes"], serde_json::json!([]));
                send_response(&mut stream, 0, serde_json::json!("Attachment received"));
                requests_sent.send(()).expect("synthetic request completed");
            }
        });
        let id = saved_conversation(&state, &workspace, SessionDefaults::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let before = runtime.block_on(async {
            let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
            let view = store.load_view(id).await.unwrap().unwrap();
            let goal_id = arany::GoalId::new();
            let run_id = arany::RunId::new();
            for event in [
                arany::Event::GoalStarted {
                    goal_id,
                    objective: "Legacy @private.txt".into(),
                    include_paths: None,
                    workspace_identity: view.workspace_identity.unwrap(),
                    budget: arany::GoalBudget {
                        max_runs: 1,
                        ..Default::default()
                    },
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: Default::default(),
                    continues: None,
                },
                arany::Event::GoalRunLinked {
                    goal_id,
                    run_id,
                    phase: arany::GoalPhase::Implementation,
                },
                arany::Event::MessageAccepted {
                    skill: None,
                    run_id,
                    text: "Legacy @private.txt".into(),
                    images: Vec::new(),
                },
                arany::Event::GoalFinished {
                    goal_id,
                    status: arany::GoalStatus::Failed,
                    active_ms: 0,
                    report: arany::GoalReport {
                        summary: "Budget exhausted".into(),
                        changes: Vec::new(),
                        evidence: Vec::new(),
                        unresolved: Vec::new(),
                        cached_input_tokens: 0,
                    },
                    exhausted: true,
                    budget_refusal: None,
                },
            ] {
                store.append(id, event).await.unwrap();
            }
            let events = store.load_session(id).await.unwrap();
            assert!(
                store
                    .load_view(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .continuable_goal()
                    .is_some()
            );
            store.close().await.unwrap();
            events
        });
        let clipboard = root.join("clipboard");
        std::fs::write(&clipboard, "#!/bin/sh\ncase \"$*\" in\n --list-types) printf 'text/plain\\n' ;;\n '--no-newline --type text/plain') exec /usr/bin/cat /clipboard-payload ;;\n *) exit 1 ;;\nesac\n").unwrap();
        std::fs::set_permissions(&clipboard, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("payload"), &payload).unwrap();
        let wayland = root.join("runtime");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&wayland)
            .unwrap();
        let _socket = UnixListener::bind(wayland.join("wayland-0")).unwrap();
        let home = root.join("account-home");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&home)
            .unwrap();
        std::fs::write(
            root.join("passwd"),
            format!(
                "root:x:0:0:Synthetic owner:{}:/usr/bin/sh\n",
                home.display()
            ),
        )
        .unwrap();
        let mut command = sandbox(&[], root, root);
        command
            .args(["--symlink", "usr/bin", "/bin", "--ro-bind"])
            .arg(&clipboard)
            .arg("/usr/bin/wl-paste")
            .arg("--ro-bind")
            .arg(root.join("payload"))
            .arg("/clipboard-payload");
        // The HTTP client loads system roots even for loopback HTTP; preserve bundle symlink targets.
        if std::path::Path::new("/etc/ssl").is_dir() {
            command.args(["--ro-bind", "/etc/ssl", "/etc/ssl"]);
        }
        command.args([
            "--ro-bind-try",
            "/etc/ca-certificates",
            "/etc/ca-certificates",
        ]);
        command.arg("--");
        let mode = if linear {
            "--screen-reader"
        } else {
            "--classic"
        };
        let shell = format!(
            "trap ':' INT; stty rows 24 cols 120; exec \"$ARANY_TEST_EXE\" {mode} --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume {id} --provider custom:local --model model-1"
        );
        let mut pty = Pty::new(
            crate::product::PtyLaunch::new(crate::product::Isolation::Passwd(command), &shell)
                .session(&state, &workspace)
                .executable("/arany")
                .env("HOME", &home)
                .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
                .env("XDG_RUNTIME_DIR", &wayland)
                .env("WAYLAND_DISPLAY", "wayland-0")
                .term(if linear { "dumb" } else { "xterm" })
                .spawn(),
        );
        let wait_text = |pty: &mut Pty, from: usize, text: &str| {
            if linear {
                pty.wait_for_after(from, text.as_bytes());
                from + crate::process::pty_text_matches(&pty.transcript[from..], text.as_bytes())
                    .next()
                    .expect("observed linear text")
                    .end
            } else {
                pty.wait_for_screen((from, 120, 24), text);
                pty.transcript.len()
            }
        };
        wait_text(&mut pty, 0, if linear { "Input:\r\n" } else { "Ask Arany" });
        if linear {
            pty.fresh_capture();
        } else {
            wait_for_caret(&mut pty, 0, 3);
        }
        let from = pty.send(b"/goal continue\r");
        let refused = wait_text(
            &mut pty,
            from,
            &arany::EngineError::GoalIncludesUnavailable.to_string(),
        );
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(id).await.unwrap();
            // Startup may save defaults, but a refused continuation must not append Goal/Run facts.
            assert_eq!(
                events
                    .iter()
                    .filter(|event| !matches!(
                        event.event,
                        arany::Event::SessionDefaultChanged { .. }
                    ))
                    .collect::<Vec<_>>(),
                before.iter().collect::<Vec<_>>()
            );
            store.close().await.unwrap();
        });
        if !linear {
            let from = pty.send(b"\x03");
            wait_for_caret(&mut pty, from, 3);
        } else {
            pty.wait_for_after(refused, b"Input:\r\n");
        }
        if linear {
            pty.fresh_capture();
        }
        let from = pty.transcript.len();
        if linear {
            pty.send(b"/paste\r");
        } else {
            pty.send(format!("{prefix}\x1b[200~{payload}\x1b[201~").as_bytes());
        }
        let label = format!("[Pasted Content {} chars]", payload.chars().count());
        let pasted = wait_text(&mut pty, from, &label);
        if linear {
            pty.wait_for_after(pasted, b"Input:\r\n");
        } else {
            wait_for_caret(
                &mut pty,
                from,
                u16::try_from(3 + prefix.len() + label.len()).unwrap(),
            );
        }
        if linear {
            pty.fresh_capture();
        }
        let from = pty.send(b" typed suffix");
        if !linear {
            wait_for_caret(
                &mut pty,
                from,
                u16::try_from(3 + prefix.len() + label.len() + " typed suffix".len()).unwrap(),
            );
        }
        submit.send(()).expect("arm attachment request deadline");
        pty.send(b"\r");
        let answer = wait_text(&mut pty, from, "Attachment received");
        requests
            .recv_timeout(Duration::from_secs(10))
            .expect("attachment request");
        server.join().unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        pty.pump_until("attachment Goal did not commit completion", |_| {
            let view = runtime.block_on(store.load_view(id)).unwrap().unwrap();
            view.goals
                .last()
                .is_some_and(|goal| goal.status == arany::GoalStatus::Completed)
        });
        runtime.block_on(store.close()).unwrap();
        if linear {
            let receipt = wait_text(&mut pty, answer, "Status: finished\r\n");
            pty.wait_for_after(receipt, b"Input:\r\n");
        } else {
            pty.wait_for_screen((from, 120, 24), "ready · custom:local/model-1");
            wait_for_caret(&mut pty, from, 3);
        }
        let exit_from = pty.send(b"/exit");
        if !linear {
            pty.wait_for_screen((exit_from, 120, 24), "> /exit");
            wait_for_caret(&mut pty, exit_from, 8);
        }
        pty.send(b"\r");
        let status = pty.wait_exit(&format!(
            "attachment journey did not exit (linear={linear}, prefix={prefix:?})"
        ));
        assert!(status.success());
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let view = store.load_view(id).await.unwrap().unwrap();
            let goal = view.goals.last().unwrap();
            assert_eq!(goal.objective, expected);
            assert_eq!(view.runs.last().unwrap().objective, expected);
            assert_eq!(goal.include_paths, Some(Vec::new()));
            assert_eq!(goal.status, arany::GoalStatus::Completed);
            store.close().await.unwrap();
        });
    }
}

// A private root holding system libraries, the PTY shell tools, the product at `/arany` and `root`
// bound at `at`.
fn sandbox(namespaces: &[&str], root: &Path, at: &Path) -> Command {
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .args([
            "--unshare-user",
            "--uid",
            "0",
            "--gid",
            "0",
            "--unshare-pid",
        ])
        .args(namespaces)
        .args([
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--dir",
            "/usr",
            "--dir",
            "/usr/bin",
            "--dir",
            "/etc",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev-bind",
            "/dev",
            "/dev",
            "--bind",
        ])
        .arg(root)
        .arg(at)
        .arg("--ro-bind")
        .arg(root.join("passwd"))
        .arg("/etc/passwd");
    for program in ["sh", "script", "stty", "cat"] {
        command
            .arg("--ro-bind")
            .arg(format!("/usr/bin/{program}"))
            .arg(format!("/usr/bin/{program}"));
    }
    command
        .arg("--ro-bind")
        .arg(env!("CARGO_BIN_EXE_arany"))
        .arg("/arany");
    command
}
