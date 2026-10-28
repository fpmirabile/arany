use super::*;
use std::os::unix::fs::PermissionsExt;

fn fixture() -> (
    tempfile::TempDir,
    CommandEnvironment,
    EnvironmentCaptureIntent,
) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let environment = CommandEnvironment::for_path("/usr/bin:/bin").with_name("LANG", "C");
    let intent = prepare_intent(&environment, Path::new("/bin/sh"), &home, &workspace).unwrap();
    (temp, environment, intent)
}

#[test]
fn shell_and_home_pins_are_rechecked_before_dispatch() {
    let (temp, environment, mut intent) = fixture();
    let launcher = std::env::current_exe().unwrap();
    assert!(command(&launcher, &intent, &environment).is_ok());
    intent.shell.inode += 1;
    assert!(command(&launcher, &intent, &environment).is_err());
    intent.shell.inode -= 1;
    std::fs::rename(&intent.home, temp.path().join("previous-home")).unwrap();
    std::fs::create_dir(&intent.home).unwrap();
    assert!(command(&launcher, &intent, &environment).is_err());
    let invalid_shell = temp.path().join("shell");
    std::fs::write(&invalid_shell, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&invalid_shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        prepare_intent(
            &environment,
            &invalid_shell,
            Path::new(&intent.home),
            &temp.path().join("workspace")
        )
        .is_err()
    );
}

fn fixture_command(intent: &EnvironmentCaptureIntent, script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .args(["-c", script])
        .current_dir(&intent.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn fixture_shell_capture_has_bounded_output_closed_failures_and_no_record_side_effect() {
    let (_temp, environment, intent) = fixture();
    let success = fixture_command(
        &intent,
        "printf 'PATH=/bin\\000LANG=C\\000API_TOKEN=CAPTURE_SECRET_CANARY\\000'",
    );
    let captured = run(success, intent.clone(), environment.clone())
        .await
        .unwrap();
    assert_eq!(captured.source(), "login-shell");
    assert_eq!(captured.path(), ["/bin"]);
    assert_eq!(captured.names().collect::<Vec<_>>(), [("LANG", "C")]);
    assert_eq!(environment.source(), "process");
    for (script, expected) in [
        ("printf 'PATH=/bin\\000'; exit 7".to_owned(), Stage::Exit),
        ("printf malformed".to_owned(), Stage::InvalidEnvironment),
        (
            format!(
                "exec /usr/bin/head -c {} /dev/zero",
                LOGIN_SHELL_CAPTURE_OUTPUT_BYTES + 1
            ),
            Stage::OutputLimit,
        ),
        (
            format!(
                "exec /usr/bin/head -c {} /dev/zero >&2",
                LOGIN_SHELL_CAPTURE_OUTPUT_BYTES + 1
            ),
            Stage::OutputLimit,
        ),
    ] {
        assert_eq!(
            run(
                fixture_command(&intent, &script),
                intent.clone(),
                environment.clone()
            )
            .await,
            Err(expected)
        );
    }
}

#[tokio::test]
async fn output_limit_is_shared_by_both_streams_and_read_errors_are_unknown() {
    let total = AtomicUsize::new(0);
    assert!(read_pipe(&b"PATH=/bin\0"[..], true, &total).await.is_ok());
    total.store(LOGIN_SHELL_CAPTURE_OUTPUT_BYTES, Ordering::Relaxed);
    assert_eq!(
        read_pipe(&b"x"[..], false, &total).await,
        Err(Stage::OutputLimit)
    );
    struct Broken;
    impl AsyncRead for Broken {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("fixture read failed")))
        }
    }
    assert_eq!(
        read_pipe(Broken, true, &AtomicUsize::new(0)).await,
        Err(Stage::Uncertain)
    );
}

#[tokio::test]
async fn capture_deadline_stops_pending_streams_without_returning_partial_environment() {
    let (_temp, _environment, intent) = fixture();
    let mut command = fixture_command(&intent, "printf ready; while :; do :; done");
    let mut child = command.spawn().unwrap();
    let group = Pid::from_raw(i32::try_from(child.id().unwrap()).unwrap()).unwrap();
    let mut marker = [0; 5];
    tokio::time::timeout(
        CAPTURE_TIMEOUT,
        child.stdout.as_mut().unwrap().read_exact(&mut marker),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&marker, b"ready");
    tokio::time::pause();
    let result = collect(&mut child).await;
    tokio::time::resume();
    let _ = kill_process_group(group, Signal::KILL);
    let _ = child.start_kill();
    let reaped = tokio::time::timeout(REAP_BOUND, reap_owned_child(&mut child))
        .await
        .unwrap()
        .is_ok();
    assert!(reaped);
    assert!(stop_group(group).await);
    assert_eq!(result, Err(Stage::Timeout));
}
