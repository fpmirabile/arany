use super::*;

#[cfg(debug_assertions)]
const PRIVATE_FILE_PROMPT: &[u8] =
    b"Choose: NOT encrypted; same-user apps read key; type a choice name: 1. Cancel or 2. Use private file; empty Enter selects 1. Cancel\r\nInput:\r\n";

#[cfg(debug_assertions)]
const PROVIDER_PROMPT: &[u8] =
    b"Choose: Only the selected Provider receives requests; type a choice name: 1. OpenAI or 2. Anthropic; empty Enter selects 1. OpenAI\r\nInput:\r\n";

fn choose_native_account_setup(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
) {
    wait_for(
        output,
        input,
        transcript,
        answered,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    input
        .write_all(b"API accounts\r")
        .expect("open API accounts");
    acknowledge_linear_pages(
        output,
        input,
        transcript,
        answered,
        (0, API_ACCOUNTS_PROMPT),
    );
    input
        .write_all(b"Add or replace native API account\r")
        .expect("add native API account");
}

#[cfg(debug_assertions)]
#[test]
fn bare_start_reuses_private_file_account_without_setup_or_run() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    prepare_test_file_account(temp.path());
    let shell = "printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("saved-account PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Input:\r\n",
    );
    let text = String::from_utf8_lossy(&transcript);
    let shell_pid = text
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse::<u32>()
        .expect("shell PID number");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    input.write_all(b"/exit\r").expect("exit Session");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "saved-account startup failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader mode uses no cursor query"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains("Provider setup"), "setup was reopened");
    assert!(
        !text.contains("synthetic-existing-key"),
        "key entered output"
    );
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(&state).expect("existing State"),
            workspace.clone(),
        )
        .await
        .expect("Workspace Sessions");
        assert!(
            sessions.is_empty(),
            "startup, local rejection and setup navigation save no empty conversation"
        );
    });
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap release-account setup gate"]
fn release_setup_uses_private_passwd_home_without_test_override() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    let workspace = private_home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("private Workspace");
    std::fs::create_dir(private_home.join("environment-home")).expect("environment home");
    std::fs::create_dir(private_home.join("environment-state")).expect("environment State");
    let passwd_home = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .expect("passwd lookup")
        .expect("effective user")
        .dir;
    assert!(passwd_home.is_absolute(), "absolute passwd home");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_HOME", &private_home)
        .env("ARANY_PASSWD_HOME", &passwd_home)
        .env("ARANY_TEST_SETUP", "1")
        .env("ARANY_TEST_STATE", passwd_home.join("state"))
        .env("ARANY_TEST_HOME_ENV", passwd_home.join("environment-home"))
        .env(
            "ARANY_TEST_XDG_STATE",
            passwd_home.join("environment-state"),
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .env("PATH", "/usr/bin")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", RELEASE_ACCOUNT_SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("release setup PTY"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\r").expect("start product");
    choose_native_account_setup(&mut output, &mut input, &mut transcript, &mut answered);
    let access_at = transcript.len();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        b"Setup: Use private file?",
    );
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        b"Input:\r\n",
    );
    assert!(String::from_utf8_lossy(&transcript[access_at..]).contains(
        "type a choice name: 1. Cancel or 2. Use private file; empty Enter selects 1. Cancel"
    ));
    let cancel_at = transcript.len();
    input
        .write_all(b"\r")
        .expect("default declines file storage");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        cancel_at,
        b"TTY_AFTER:",
    );
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "cancelled release setup failed");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader setup queried the cursor"
    );
    assert!(
        !private_home.join("state").exists(),
        "created Session State"
    );
    assert!(
        !private_home.join(".local/state/arany").exists(),
        "created OS-user account State"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap release-account cross-StateRoot gate"]
fn release_saved_file_account_is_shared_across_session_roots() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    std::fs::create_dir_all(private_home.join("workspace")).expect("private Workspace");
    super::super::prepare_test_file_account_at(&private_home.join(".local/state/arany"));
    let passwd_home = release_passwd_home();
    for name in ["state-one", "state-two"] {
        let view = release_bare_session(
            &private_home,
            &passwd_home,
            name,
            None,
            &["synthetic-existing-key"],
        )
        .expect("native Session");
        assert!(view.contains("Provider: openai\r\nModel: gpt-5.4\r\n"));
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux cross-user product gate; requires subordinate IDs, unshare, bwrap, and setpriv"]
fn release_saved_account_is_private_across_os_users() {
    const STAGE: &str = "ARANY_TEST_CROSS_USER_PRODUCT_STAGE";
    const TEST: &str = "setup::native::release_saved_account_is_private_across_os_users";

    if let Some(stage) = std::env::var_os(STAGE) {
        let (home, expected_account) = match stage.to_str() {
            Some("owner") => {
                assert_eq!(nix::unistd::geteuid().as_raw(), 0);
                assert_eq!(
                    StateRoot::account_path().unwrap(),
                    Path::new("/owner/.local/state/arany")
                );
                (
                    "/owner",
                    Some(
                        Uuid::parse_str(
                            &std::env::var("ARANY_TEST_CROSS_USER_ACCOUNT_ID").unwrap(),
                        )
                        .unwrap(),
                    ),
                )
            }
            Some("other") => {
                assert_eq!(nix::unistd::geteuid().as_raw(), 1);
                assert_eq!(
                    StateRoot::account_path().unwrap(),
                    Path::new("/other/.local/state/arany")
                );
                assert_eq!(
                    std::fs::symlink_metadata("/other/.local/state/arany")
                        .expect_err("second user's account root is absent")
                        .kind(),
                    std::io::ErrorKind::NotFound
                );
                std::fs::create_dir("/other/workspace").expect("second user's Workspace");
                ("/other", None)
            }
            _ => panic!("invalid cross-user product stage"),
        };
        let shell = if expected_account.is_some() {
            "/arany --screen-reader --state-dir /owner/state --workspace /owner/workspace"
        } else {
            "/arany --screen-reader --state-dir /other/state --workspace /other/workspace"
        };
        let fixture_state = if expected_account.is_some() {
            "/owner/spoofed-state"
        } else {
            "/other/fixture-state"
        };
        let mut command = Command::new("/usr/bin/script");
        command
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .env_clear()
            .env("SHELL", "/usr/bin/sh")
            .env("TERM", "dumb")
            .env("HOME", "/owner")
            .env("XDG_STATE_HOME", fixture_state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("cross-user product PTY"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::default();
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Input:\r\n",
        );
        input.write_all(b"/exit\r").expect("exit Session");
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "cross-user product exit");
        assert_eq!(
            answered.cursor_queries, 0,
            "screen-reader startup queried the cursor"
        );
        assert!(
            !transcript
                .windows(b"synthetic-existing-key".len())
                .any(|part| part == b"synthetic-existing-key")
        );
        assert!(
            !transcript.contains(&b'\x1b'),
            "screen-reader output has escapes"
        );

        let state = Path::new(home).join("state");
        let workspace = Path::new(home).join("workspace");
        let journal = std::fs::read(state.join("events.sqlite3")).expect("Session journal");
        assert!(
            !journal
                .windows(b"synthetic-existing-key".len())
                .any(|part| part == b"synthetic-existing-key"),
            "saved key entered Session journal"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("read-only runtime");
        runtime.block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(&state).expect("existing State"),
                workspace,
            )
            .await
            .expect("Workspace Sessions");
            assert!(
                sessions.is_empty(),
                "bare startup saves no empty conversation"
            );
            assert!(std::str::from_utf8(&transcript).expect("UTF-8").contains(
                if expected_account.is_some() {
                    "Provider: openai\r\nModel: gpt-5.4\r\n"
                } else {
                    "Provider: unset\r\nModel: unset\r\n"
                }
            ));
        });
        if expected_account.is_none() {
            assert!(!Path::new("/other/.local/state/arany").exists());
        } else {
            let status = Command::new("/usr/bin/chown")
                .args(["1:1", "/other"])
                .status()
                .expect("prepare second user's home");
            assert!(status.success(), "prepare second user's home");
            let output = Command::new("/usr/bin/setpriv")
                .args([
                    "--reuid",
                    "1",
                    "--regid",
                    "1",
                    "--clear-groups",
                    "/test",
                    "--exact",
                    TEST,
                    "--ignored",
                    "--nocapture",
                ])
                .env_clear()
                .env(STAGE, "other")
                .bounded_output()
                .expect("second OS user");
            assert!(
                output.status.success(),
                "second OS user's product check failed: {}",
                format!(
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
                .replace("synthetic-existing-key", "[redacted]")
                .chars()
                .take(2048)
                .collect::<String>()
            );
            crate::process::assert_single_test_completed(&output.stdout, "second OS user");
        }
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("private cross-user fixture");
    let owner_home = temp.path().join("owner-home");
    std::fs::create_dir_all(owner_home.join("workspace")).expect("owner Workspace");
    std::fs::set_permissions(&owner_home, std::fs::Permissions::from_mode(0o700))
        .expect("owner-only home");
    let account_id =
        super::super::prepare_test_file_account_at(&owner_home.join(".local/state/arany"));
    let passwd = temp.path().join("passwd");
    std::fs::write(
        &passwd,
        b"owner:x:0:0::/owner:/bin/sh\nother:x:1:1::/other:/bin/sh\n",
    )
    .expect("synthetic passwd entries");
    let output = Command::new("/usr/bin/timeout")
        .args([
            "-k",
            "2s",
            "40s",
            "/usr/bin/unshare",
            "--map-auto",
            "--map-user",
            "0",
            "--map-group",
            "0",
            "--user",
            "--mount",
            "--net",
            "--fork",
            "--kill-child",
            "/usr/bin/bwrap",
            "--unshare-pid",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--ro-bind",
        ])
        .arg(&passwd)
        .args(["/etc/passwd", "--chmod", "0755", "/etc", "--bind"])
        .arg(&owner_home)
        .args(["/owner", "--tmpfs", "/other", "--ro-bind"])
        .arg(std::env::current_exe().expect("test executable"))
        .args([
            "/test",
            "--ro-bind",
            env!("CARGO_BIN_EXE_arany"),
            "/arany",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--clearenv",
            "--setenv",
            STAGE,
            "owner",
            "--setenv",
            "ARANY_TEST_CROSS_USER_ACCOUNT_ID",
        ])
        .arg(account_id.to_string())
        .args(["--", "/test", "--exact", TEST, "--ignored", "--nocapture"])
        .env_clear()
        .bounded_output()
        .expect("isolated cross-user product test");
    assert!(
        output.status.success(),
        "isolated cross-user product test failed: {}",
        format!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .replace("synthetic-existing-key", "[redacted]")
        .chars()
        .take(4096)
        .collect::<String>()
    );
    crate::process::assert_single_test_completed(&output.stdout, "saved account");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
#[ignore = "native Linux network-isolated saved-account replacement gate"]
fn replaced_file_account_cannot_run_an_older_session() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let original_account = prepare_test_file_account(temp.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let session_id = runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("State"),
            workspace.clone(),
            None,
        ))
        .expect("Session");
    runtime
        .block_on(set_session_defaults(
            StateRoot::open_existing(&state).expect("State reopen"),
            workspace.clone(),
            session_id,
            SessionDefaults {
                provider: Some("openai".into()),
                model: Some("gpt-5.4".into()),
                effort: None,
                account_id: Some(original_account),
                policy: Default::default(),
            },
        ))
        .expect("saved-account defaults");
    let replacement = prepare_test_file_account(temp.path());
    assert_ne!(replacement, original_account);

    crate::isolation::remember_workspace_trust(
        &temp.path().join("account-root"),
        &workspace,
        std::path::Path::new("/data/workspace"),
    );
    let shell = "/usr/bin/timeout -k 1s 5s /usr/bin/bwrap --unshare-user --unshare-net --unshare-pid --die-with-parent --tmpfs / --ro-bind /usr /usr --symlink usr/lib /lib --ro-bind /lib64 /lib64 --proc /proc --dev-bind /dev /dev --bind \"$ARANY_TEST_ROOT\" /data --ro-bind \"$ARANY_TEST_EXE\" /arany --clearenv --setenv HOME /data --setenv XDG_STATE_HOME /data/xdg-state --setenv ARANY_TEST_ACCOUNT_ROOT /data/account-root --setenv DBUS_SESSION_BUS_ADDRESS unixexec:path=/usr/bin/false --setenv TERM dumb --chdir /data/workspace -- /arany --screen-reader --state-dir /data/state --workspace /data/workspace --resume \"$ARANY_TEST_SESSION\" 'synthetic stale-account objective'";
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_ROOT", temp.path())
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(command.spawn().expect("network-isolated product"));
    assert_eq!(output.status.code(), Some(1), "stale account must fail");
    assert!(output.stderr.is_empty(), "script wrapper stderr");
    let transcript = String::from_utf8(output.stdout).expect("screen-reader UTF-8");
    let diagnostic = tail(
        transcript
            .replace("synthetic-existing-key", "[redacted]")
            .as_bytes(),
    );
    assert!(!transcript.contains("synthetic-existing-key"));
    assert!(
        !transcript.contains('\x1b'),
        "screen-reader output has escapes"
    );
    assert!(
        transcript
            == "error: invalid saved account; use arany --setup to choose or reset access\r\n",
        "stale account was not rejected exactly: {diagnostic}"
    );

    runtime.block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
            .expect("read-only Store");
        let events = store.load_session(session_id).await.expect("Events");
        let view = SessionView::replay(session_id, &events)
            .expect("strict replay")
            .expect("Session");
        assert_eq!(events.len(), 2, "rejected objective changed the journal");
        assert!(view.runs.is_empty(), "rejected objective began a Run");
        assert_eq!(view.defaults.account_id, Some(original_account));
        store.close().await.expect("close Store");
    });
}

#[cfg(debug_assertions)]
#[test]
fn narrow_no_color_setup_hides_key_and_restores_terminal_on_cancel() {
    for explicit_workspace in [false, true] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        prepare_test_file_account(temp.path());
        let account_root = StateRoot::open_existing(&temp.path().join("account-root"))
            .expect("synthetic account root");
        let account_before = account_root
            .record(RecordKind::SavedAccount)
            .read()
            .expect("initial account");
        let mut command = Command::new("/usr/bin/script");
        command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unixexec:path=/usr/bin/false",
        )
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "stty rows 8 cols 16; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --no-color --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("narrow setup PTY"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::default();
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"TTY_BEFORE:",
        );
        super::super::slash_completion::wait_for_frame(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (0, 16, 8),
            |screen| {
                screen.iter().any(|row| row.trim() == "Provider setup")
                    && screen.iter().any(|row| row.starts_with("> API accounts"))
            },
        );
        let feedback_at = transcript.len();
        input.write_all(b"x").expect("non-navigation setup key");
        super::super::slash_completion::wait_for_frame(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (feedback_at, 16, 8),
            |screen| {
                screen.iter().any(|row| row.starts_with("Use Up/Down"))
                    && screen.iter().any(|row| row.contains("Enter Esc"))
                    && screen.iter().any(|row| row.starts_with("> API accounts"))
            },
        );
        assert!(!state.exists(), "invalid choice created State");
        let accounts_at = transcript.len();
        input.write_all(b"\r").expect("open API accounts");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            accounts_at,
            b"API accounts",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            accounts_at,
            b"\x1b[?25l",
        );
        let access_at = transcript.len();
        input.write_all(b"\r").expect("select API key access");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            access_at,
            b"NOT encrypted",
        );
        let consent_at = transcript.len();
        input
            .write_all(b"\x1b[B\r")
            .expect("choose and confirm file storage");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            consent_at,
            b"Choose Provid",
        );
        let provider_at = consent_at
            + transcript[consent_at..]
                .windows(b"Choose Provid".len())
                .position(|part| part == b"Choose Provid")
                .expect("provider frame title")
            + b"Choose Provid".len();
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            provider_at,
            b"\x1b[?25l",
        );
        input
            .write_all(if explicit_workspace {
                b"\x1b[B\r"
            } else {
                b"\r"
            })
            .expect("native Provider choice");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"512",
        );
        let text = String::from_utf8_lossy(&transcript);
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("setup PTY marker")
                .to_owned()
        };
        let shell_pid = marker("SHELL_PID:").parse::<u32>().expect("shell PID");
        let before = marker("TTY_BEFORE:");
        let product_pid = product_child_of(shell_pid);
        let _product = ProductGuard::new(product_pid, state.clone());
        assert_ne!(tty_settings(product_pid), before, "key input hides echo");
        input
            .write_all(b"\x1b[200~CANARYKEY0\x1b[201~")
            .expect("unsubmitted secret paste");
        let key_at = transcript.len();
        crate::session_picker::pump_until(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            "hidden input was not consumed",
            |transcript| {
                transcript[key_at..]
                    .windows(b"10 chars".len())
                    .any(|part| part == b"10 chars")
            },
        );
        let rejected_at = transcript.len();
        input
            .write_all(b"\x1b[200~ \r/setup\x1b[201~")
            .expect("reject whole invalid secret paste without submission");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            rejected_at,
            b"Invalid",
        );
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            rejected_at,
            b"\x1b[?25l",
        );
        let blocked_at = transcript.len();
        input
            .write_all(b"X")
            .expect("valid byte remains blocked after rejected byte");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            blocked_at,
            b"\x1b[?25l",
        );
        assert!(
            !transcript[blocked_at..]
                .windows(b"\x1b[6;2H1".len())
                .any(|part| part == b"\x1b[6;2H1"),
            "a byte entered the key while rejected input remained"
        );
        let corrected_at = transcript.len();
        input
            .write_all(b"\x7f\x7fX")
            .expect("correct rejected key bytes and append valid byte");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            corrected_at,
            b"\x1b[6;2H1",
        );
        if explicit_workspace {
            let scope_at = transcript.len();
            input.write_all(b"\r").expect("submit hidden synthetic key");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                scope_at,
                b"API workspace",
            );
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                scope_at,
                b"\x1b[?25l",
            );
            let field_at = transcript.len();
            input
                .write_all(b"\x1b[B\r")
                .expect("choose explicit workspace");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                field_at,
                b"wrkspc_...",
            );
            let typed_at = transcript.len();
            input
                .write_all(b"\x1b[200~wrkspc_Test123\x1b[201~")
                .expect("visible workspace ID paste without submission");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                typed_at,
                b"\x1b[5;3H",
            );
            let invalid_at = transcript.len();
            input
                .write_all(b"\x1b[D\x7f-\r")
                .expect("reject locally malformed workspace");
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                invalid_at,
                b"Error:",
            );
        }
        input.write_all(b"\x1b").expect("cancel setup with Escape");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if child.child().try_wait().expect("product status").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "setup did not exit after cancellation"
            );
            thread::yield_now();
        }
        let status = child.take().wait().expect("product exit");
        assert!(status.success(), "setup failed after cancellation");
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let text = String::from_utf8_lossy(&transcript);
        assert!(!text.contains("CANARYKEY0"));
        assert!(
            !text.contains("\x1b[?1049h"),
            "setup entered alternate screen"
        );
        assert!(!state.exists(), "cancelled setup created State");
        let after = text
            .split("TTY_AFTER:")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .expect("TTY after marker")
            .trim_end_matches('\r');
        assert_eq!(before, after, "terminal settings restored");
        assert!(
            account_before
                == account_root
                    .record(RecordKind::SavedAccount)
                    .read()
                    .expect("account after cancel"),
            "cancelled workspace setup replaced the account"
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn cancelled_screen_reader_setup_hides_key_and_creates_no_session() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    prepare_test_file_account(temp.path());
    let account_root = StateRoot::open_existing(&temp.path().join("account-root"))
        .expect("synthetic account root");
    let account_before = account_root
        .record(RecordKind::SavedAccount)
        .read()
        .expect("initial account");
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", SETUP_SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("setup PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\n").expect("script input gate");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    let (shell_pid, before) = {
        let text = String::from_utf8_lossy(&transcript);
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("setup PTY marker")
                .to_owned()
        };
        (
            field("SHELL_PID:").parse::<u32>().expect("shell PID"),
            field("TTY_BEFORE:"),
        )
    };
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    assert_eq!(
        tty_settings(product_pid),
        before,
        "choices keep terminal echo"
    );
    let access_at = transcript.len();
    assert!(
        String::from_utf8_lossy(&transcript)
            .contains("1. API accounts or 2. Custom routes or 3. Role routing")
    );
    input
        .write_all(b"API accounts\r")
        .expect("open API accounts");
    acknowledge_linear_pages(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (access_at, API_ACCOUNTS_PROMPT),
    );
    input
        .write_all(b"Add or replace native API account\r")
        .expect("select native API account");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        access_at,
        PRIVATE_FILE_PROMPT,
    );
    let consent_at = transcript.len();
    input
        .write_all(b"Use private file\r")
        .expect("private file consent");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        consent_at,
        PROVIDER_PROMPT,
    );
    input
        .write_all(b"Anthropic\r")
        .expect("named Anthropic choice");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Enter API key",
    );
    let hidden = tty_settings(product_pid);
    assert_ne!(hidden, before, "setup input echo is disabled");
    kill_process(product_pid, Signal::TSTP).expect("suspend setup");
    wait_stopped(product_pid);
    assert_eq!(
        tty_settings(product_pid),
        before,
        "echo restored while stopped"
    );
    let resumed_at = transcript.len();
    kill_process(product_pid, Signal::CONT).expect("resume setup");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[resumed_at..]
        .windows(b"Enter API key".len())
        .any(|part| part == b"Enter API key")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(Instant::now() < deadline, "setup did not resume");
        thread::yield_now();
    }
    assert_eq!(
        tty_settings(product_pid),
        hidden,
        "echo hidden after resume"
    );
    input.write_all(&[b'K'; 513]).expect("overlong key prefix");
    input.write_all(b"\n").expect("reject overlong key");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Invalid or overlong API key; retry",
    );
    input
        .write_all(b"SYNTHETIC_SECRET_CANARY\r")
        .expect("submit hidden synthetic key");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. Key scoped to workspace or 2. Choose API workspace; empty Enter selects 1. Key scoped to workspace\r\nInput:\r\n",
    );
    assert_eq!(
        tty_settings(product_pid),
        before,
        "workspace choices restore echo"
    );
    let field_at = transcript.len();
    input
        .write_all(b"Choose API workspace\r")
        .expect("explicit workspace choice");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        field_at,
        b"Input:\r\n",
    );
    for invalid in [b"\r".as_slice(), b"wrong\r".as_slice()] {
        let invalid_at = transcript.len();
        input.write_all(invalid).expect("invalid visible workspace");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            invalid_at,
            b"Notice: Error: enter wrkspc_ followed by letters or digits",
        );
    }
    input
        .write_all(b"wrkspc_Unsubmitted")
        .expect("visible workspace draft");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"wrkspc_Unsubmitted",
    );
    input.write_all(b"\x03").expect("cancel setup");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if child.child().try_wait().expect("product status").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "setup did not exit after cancellation"
        );
        thread::yield_now();
    }
    let status = child.take().wait().expect("product exit");
    assert!(status.success(), "setup failed after cancellation");
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    let text = String::from_utf8_lossy(&transcript);
    assert!(!text.contains("SYNTHETIC_SECRET_CANARY"));
    assert!(!text.contains(&"K".repeat(513)));
    assert!(!state.exists(), "cancelled setup created State");
    assert!(
        account_before
            == account_root
                .record(RecordKind::SavedAccount)
                .read()
                .expect("account after cancel"),
        "cancelled workspace setup replaced the account"
    );
    assert!(
        !transcript.contains(&0x1b),
        "linear workspace setup emitted terminal controls"
    );
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("TTY marker")
            .to_owned()
    };
    assert_eq!(before, marker("TTY_AFTER:"));
}

#[cfg(debug_assertions)]
#[test]
fn setup_signals_after_hidden_key_segment_restore_echo_and_preserve_defaults() {
    for (signal, signal_name, attached) in [
        (Signal::TERM, "SIGTERM", false),
        (Signal::HUP, "SIGHUP", false),
        (Signal::TERM, "SIGTERM", true),
        (Signal::HUP, "SIGHUP", true),
    ] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        prepare_test_file_account(temp.path());
        let account_path = temp.path().join("account-root/account-credentials.json");
        let account_before = std::fs::read(&account_path).expect("original account record");
        let shell = if attached {
            MIXED_START_SHELL
        } else {
            SETUP_SHELL
        };
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("setup PTY process"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::default();
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\n").expect("script input gate");
        if attached {
            wait_for(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                b"Input:\r\n",
            );
        }
        let setup_at = transcript.len();
        if attached {
            input
                .write_all(b"/setup\r")
                .expect("setup in existing Session");
        }
        let access_at = transcript.len();
        if attached {
            choose_native_account_setup(&mut output, &mut input, &mut transcript, &mut answered);
        } else {
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                setup_at,
                b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
            );
            input
                .write_all(b"API accounts\r")
                .expect("open API accounts");
            acknowledge_linear_pages(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                (access_at, API_ACCOUNTS_PROMPT),
            );
            input
                .write_all(b"Add or replace native API account\r")
                .expect("select native account");
        }
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            access_at,
            PRIVATE_FILE_PROMPT,
        );
        let consent_at = transcript.len();
        input
            .write_all(b"Use private file\r")
            .expect("private file consent");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            consent_at,
            PROVIDER_PROMPT,
        );
        input.write_all(b"OpenAI\r").expect("OpenAI choice");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Enter API key",
        );
        let text = String::from_utf8_lossy(&transcript);
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("setup PTY marker")
                .to_owned()
        };
        let shell_pid = marker("SHELL_PID:").parse::<u32>().expect("shell PID");
        let before = marker("TTY_BEFORE:");
        let product_pid = product_child_of(shell_pid);
        let _product = ProductGuard::new(product_pid, state.clone());
        assert_ne!(tty_settings(product_pid), before, "hidden input owns echo");
        let segment_at = transcript.len();
        input
            .write_all(b"SYNTHETIC_SECRET_CANARY")
            .expect("hidden key segment");
        input.write_all(b"\x04").expect("continue hidden key line");
        let retry_notice = b"Invalid or overlong API key line; retry";
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript[segment_at..]
            .windows(retry_notice.len())
            .any(|part| part == retry_notice)
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "{signal_name} did not consume hidden key segment"
            );
            thread::yield_now();
        }
        kill_process(product_pid, signal).expect("signal setup");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if child.child().try_wait().expect("product status").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "setup did not exit after signal");
            thread::yield_now();
        }
        let status = child.take().wait().expect("product exit");
        assert_eq!(status.code(), Some(1), "{signal_name}: ordinary error exit");
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let text = String::from_utf8_lossy(&transcript);
        assert!(text.contains(&format!("terminated by {signal_name}")));
        let canary = text.find("SYNTHETIC_SECRET_CANARY");
        let shutdown = text.find(&format!("terminated by {signal_name}"));
        let shell_return = text.find("TTY_AFTER:");
        assert!(
            canary.is_none(),
            "{signal_name} exposed synthetic input: canary={canary:?}, shutdown={shutdown:?}, shell_return={shell_return:?}"
        );
        let account_after = std::fs::read(&account_path).expect("account after shutdown");
        assert!(
            account_before == account_after,
            "shutdown changed account record"
        );
        if attached {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("read-only runtime");
            runtime.block_on(async {
                let sessions = arany::list_sessions(
                    StateRoot::open_existing(&state).expect("existing State"),
                    workspace.clone(),
                )
                .await
                .expect("Workspace Sessions");
                assert!(
                    sessions.is_empty(),
                    "startup, local rejection and setup navigation save no empty conversation"
                );
            });
        } else {
            assert!(!state.exists(), "signaled pre-Session setup created State");
        }
        let after = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("TTY_AFTER:"))
            .expect("TTY after marker");
        assert_eq!(before, after, "terminal echo restored after {signal_name}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn credential_helper_rejects_invalid_record_before_os_store_access() {
    let temp = tempfile::tempdir().expect("private test root");
    let socket = temp.path().join("bus");
    let listener = UnixListener::bind(&socket).expect("private synthetic D-Bus socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking synthetic bus");
    let mut child = Command::new("/usr/bin/timeout")
        .args([
            "2s",
            env!("CARGO_BIN_EXE_arany"),
            "--internal-credential-helper",
            "write",
            "default-native-api-account",
        ])
        .env_clear()
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("credential helper");
    child
        .stdin
        .take()
        .expect("bounded input pipe")
        .write_all(b"invalid record")
        .expect("synthetic invalid record");
    let status = child.wait().expect("helper status");
    assert_ne!(
        status.code(),
        Some(124),
        "invalid record caused OS-store wait"
    );
    assert!(!status.success(), "invalid record accepted");
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "invalid record reached OS store"
    );
    let status = Command::new("/usr/bin/timeout")
        .args([
            "2s",
            env!("CARGO_BIN_EXE_arany"),
            "--internal-credential-helper",
            "delete",
            "unsupported-account-slot",
        ])
        .env_clear()
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("invalid deletion helper");
    assert!(!status.success() && status.code() != Some(124));
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "native account deletion reached OS store"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn saved_account_fifo_is_rejected_without_blocking_or_keyring_access() {
    let temp = tempfile::tempdir().expect("private test root");
    #[cfg(debug_assertions)]
    let account_root = temp.path().join("account-root");
    #[cfg(not(debug_assertions))]
    let private_home = temp.path().join("home");
    #[cfg(not(debug_assertions))]
    let account_root = private_home.join(".local/state/arany");
    drop(StateRoot::admit(&account_root).expect("private account root"));
    let run = || {
        let mut command = Command::new("/usr/bin/timeout");
        command.args(["--signal=TERM", "--kill-after=1s", "2s"]);
        #[cfg(debug_assertions)]
        command
            .arg(env!("CARGO_BIN_EXE_arany"))
            .args(["provider", "models", "openai", "--saved-account"])
            .env_clear()
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
            .current_dir(temp.path());
        #[cfg(not(debug_assertions))]
        {
            let passwd_home = release_passwd_home();
            command
                .args([
                    "/usr/bin/bwrap",
                    "--unshare-user",
                    "--unshare-net",
                    "--unshare-pid",
                    "--die-with-parent",
                    "--tmpfs",
                    "/",
                    "--ro-bind",
                    "/usr",
                    "/usr",
                    "--symlink",
                    "usr/lib",
                    "/lib",
                    "--ro-bind-try",
                    "/lib64",
                    "/lib64",
                    "--ro-bind",
                    "/etc/passwd",
                    "/etc/passwd",
                ])
                .arg("--bind")
                .arg(&private_home)
                .arg(&passwd_home)
                .arg("--ro-bind")
                .arg(env!("CARGO_BIN_EXE_arany"))
                .arg("/arany")
                .args(["--proc", "/proc", "--dev-bind", "/dev", "/dev"])
                .arg("--chdir")
                .arg(&passwd_home)
                .arg("--clearenv")
                .arg("--setenv")
                .arg("HOME")
                .arg(&passwd_home)
                .arg("--setenv")
                .arg("XDG_STATE_HOME")
                .arg(passwd_home.join(".local/state"))
                .args([
                    "--setenv",
                    "DBUS_SESSION_BUS_ADDRESS",
                    "unixexec:path=/usr/bin/false",
                    "--setenv",
                    "PATH",
                    "/usr/bin",
                    "--",
                    "/arany",
                    "provider",
                    "models",
                    "openai",
                    "--saved-account",
                ])
                .env_clear()
                .current_dir(temp.path());
        }
        command.bounded_output().expect("saved-account read")
    };
    let without_leaf = run();
    assert!(
        String::from_utf8_lossy(&without_leaf.stderr)
            .contains("unsafe or unsupported D-Bus session address"),
        "missing account leaf must reach credential transport admission; exit={:?}; stderr={:?}",
        without_leaf.status,
        tail(&without_leaf.stderr)
    );
    let fifo = account_root.join("account-credentials.json");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("FIFO fixture")
            .success()
    );
    let output = run();
    assert!(
        !matches!(output.status.code(), Some(124 | 137)),
        "FIFO blocked account admission"
    );
    assert!(!output.status.success(), "FIFO account accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("private account state unavailable"),
        "FIFO did not fail at account State admission"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires a native unlocked OS credential store"]
fn native_credential_helper_round_trips_a_synthetic_isolated_slot() {
    const SERVICE: &str = "io.github.fpmirabile.arany";
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(entry) = keyring::Entry::new(SERVICE, &self.0) {
                let _ = entry.delete_credential();
            }
        }
    }

    let temp = tempfile::tempdir().expect("private test root");
    let slot = format!("native-test-{}", Uuid::now_v7());
    let _cleanup = Cleanup(slot.clone());
    let account = serde_json::to_vec(&serde_json::json!({
        "schema": 3,
        "id": Uuid::now_v7(),
        "provider": {"kind": "openai", "custom_name": null},
        "model": "gpt-5.4",
        "effort": null,
        "api_key": "synthetic-native-helper-key", "anthropic_workspace_id": null
    }))
    .expect("synthetic account");
    let mut command = credential_helper_command("write", &slot, temp.path());
    let mut write = ChildGuard::new(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("native helper write"),
    );
    write
        .child()
        .stdin
        .take()
        .expect("private input pipe")
        .write_all(&account)
        .expect("synthetic account write");
    assert!(wait_product(write.take()).status.success());

    let output = wait_product(
        credential_helper_command("read", &slot, temp.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("native helper read"),
    );
    assert!(output.status.success(), "native helper read failed");
    assert!(output.stdout.len() <= RecordKind::SavedAccount.limit());
    let loaded: serde_json::Value = serde_json::from_slice(&output.stdout).expect("saved record");
    let expected: serde_json::Value = serde_json::from_slice(&account).expect("synthetic record");
    assert_eq!(loaded, expected);
    assert!(output.stderr.is_empty());
    keyring::Entry::new(SERVICE, &slot)
        .expect("synthetic account entry")
        .delete_credential()
        .expect("remove synthetic account");
}

#[cfg(unix)]
fn credential_helper_command(operation: &str, slot: &str, directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .args(["--internal-credential-helper", operation, slot])
        .env_clear()
        .current_dir(directory)
        .stderr(Stdio::null());
    for name in [
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "HOME",
        "USER",
        "LOGNAME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}
