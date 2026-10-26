use super::*;

#[test]
fn supervisor_caps_require_every_set_empty_and_no_new_privileges() {
    let status = "NoNewPrivs:\t1\nCapEff:\t0000000000000000\nCapPrm:\t0000000000000000\nCapInh:\t0000000000000000\nCapAmb:\t0000000000000000\nCapBnd:\t0000000000000000\n";
    assert!(verify_capabilities(status).is_ok());
    for line in status.lines() {
        let missing = status.replace(&format!("{line}\n"), "");
        let changed = status.replace(line, &line.replace('0', "1").replace("\t1", "\t0"));
        for invalid in [missing, changed, format!("{status}{line}\n")] {
            assert!(
                verify_capabilities(&invalid).is_err(),
                "{}",
                line.split_once(':').unwrap().0
            );
        }
    }
}

#[test]
fn supervisor_pin_rejects_same_path_content_and_identity_replacement() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("arany");
    std::fs::write(&path, b"original executable").unwrap();
    let pin = Pin::read(&path).unwrap();
    assert!(pin.verify(&path).is_ok());
    std::fs::write(&path, b"modified executable").unwrap();
    assert!(matches!(pin.verify(&path), Err(ToolError::ChangedInput)));
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, b"original executable").unwrap();
    std::fs::rename(replacement, &path).unwrap();
    assert!(matches!(pin.verify(&path), Err(ToolError::ChangedInput)));
    let large = std::fs::File::create(&path).unwrap();
    large.set_len(MAX_EXECUTABLE_BYTES + 1).unwrap();
    assert!(Pin::read(&path).is_err());
    let fifo = root.path().join("pipe");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
    assert!(
        Pin::read(&fifo).is_err(),
        "nonblocking pin admission rejects special files"
    );
}

#[test]
#[cfg_attr(not(arany_ci), ignore = "native Linux systemd executable replacement")]
fn supervisor_launch_never_executes_a_path_replacement() {
    use crate::test_process::BoundedOutput;
    use std::os::unix::fs::PermissionsExt;

    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let seed = crate::tools::fs::open_directory(root.path()).unwrap();
    let mut runtime = crate::tools::tests::in_process_runtime(
        root.path(),
        serde_json::json!({"version":1,"workspace_paths":["."],"write":false,
            "commands":[],"skills":[],"mcp":[]}),
    );
    runtime.guard_executable = root.path().join("guard");
    std::fs::copy("/usr/bin/echo", &runtime.guard_executable).unwrap();
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::List { path: ".".into() },
    );
    let (command, executable) = unit_command(
        &runtime,
        &intent,
        &format!("arany-pin-test-{}.service", uuid::Uuid::now_v7()),
        "/usr/bin:/bin",
        UnitFilesystem {
            preparation: false,
            seed_path: root.path(),
            seed_dir: &seed,
            runtime_directories: Vec::new(),
            mounts: Vec::new(),
        },
    )
    .unwrap();
    let arguments: Vec<_> = command
        .as_std()
        .get_args()
        .map(|arg| arg.to_owned())
        .collect();
    let replacement = root.path().join("replacement");
    std::fs::write(
        &replacement,
        b"#!/bin/sh\nprintf 'replacement executed\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The launch has admitted its object; release execution only after replacing the pathname.
    std::fs::rename(&replacement, &runtime.guard_executable).unwrap();
    let pinned = executable_path(&executable);
    assert!(arguments.iter().any(|arg| arg == pinned.as_os_str()));
    let output = command
        .into_std()
        .bounded_output_for(Duration::from_secs(20), GUARD_CAPTURE_BYTES)
        .unwrap();
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .starts_with(b"--internal-tool-guard --supervisor ")
    );
    assert!(
        !output
            .stdout
            .windows(b"replacement executed".len())
            .any(|bytes| bytes == b"replacement executed")
    );
}

#[test]
fn namespace_remedy_uses_only_the_admitted_static_executable_path() {
    let remedy = namespace_remedy(Path::new("/opt/arany/bin/arany")).unwrap();
    assert!(remedy.contains("profile arany_supervisor \"/opt/arany/bin/arany\""));
    assert!(remedy.contains("sudo apparmor_parser -r /etc/apparmor.d/arany-userns"));
    assert!(remedy.contains("Omit arany_bwrap when an attachment already exists"));
    assert!(remedy.contains("kernel.apparmor_restrict_unprivileged_userns=1"));
    assert!(!remedy.contains("/usr/bin/unshare"));
    assert!(namespace_remedy(Path::new("relative/arany")).is_none());
    let escaped = namespace_remedy(Path::new("/opt/my app/*/@{name}/arany\nPROFILE")).unwrap();
    assert!(escaped.contains(r#""/opt/my\x20app/\x2a/\x40\x7bname\x7d/arany\x0aPROFILE""#));
}

#[test]
fn xattr_probe_requires_set_read_remove_on_the_exact_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("probe");
    let file = std::fs::File::create_new(path).unwrap();
    probe_xattr(&file).unwrap();
    rustix::fs::fsetxattr(
        &file,
        PROBE_NAME,
        PROBE_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )
    .unwrap();
    assert!(matches!(
        probe_xattr(&file),
        Err(ToolError::ProtectionUnavailable)
    ));
    assert!(matches!(
        probe_xattr(&std::fs::File::open("/dev/null").unwrap()),
        Err(ToolError::ProtectionUnavailable)
    ));
}
