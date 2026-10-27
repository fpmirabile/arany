use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

const HELPER: &str = "tools::native_exec::supervisor::supervisor_entry_helper";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupervisorMode {
    UnalignedProbe,
    NamespaceDenied,
    ProbeDenied,
    OverlayProbeDenied,
    Submount,
    Success,
    Replacement,
    MountFlags,
}

impl SupervisorMode {
    fn parse(value: &str) -> Self {
        match value {
            "unaligned-probe" => Self::UnalignedProbe,
            "namespace-denied" => Self::NamespaceDenied,
            "probe-denied" => Self::ProbeDenied,
            "overlay-probe-denied" => Self::OverlayProbeDenied,
            "submount" => Self::Submount,
            "success" => Self::Success,
            "replacement" => Self::Replacement,
            "mount-flags" => Self::MountFlags,
            _ => panic!("unsupported supervisor fixture mode: {value}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::UnalignedProbe => "unaligned-probe",
            Self::NamespaceDenied => "namespace-denied",
            Self::ProbeDenied => "probe-denied",
            Self::OverlayProbeDenied => "overlay-probe-denied",
            Self::Submount => "submount",
            Self::Success => "success",
            Self::Replacement => "replacement",
            Self::MountFlags => "mount-flags",
        }
    }
}

fn entry_case(mode: SupervisorMode) {
    use SupervisorMode::*;
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut command = match mode {
        MountFlags | Submount => {
            let mut command = Command::new("/usr/bin/unshare");
            command
                .args([
                    "--user",
                    "--map-current-user",
                    "--mount",
                    "--propagation",
                    "private",
                    "--keep-caps",
                    "--",
                ])
                .arg(executable);
            command
        }
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Success
        | Replacement => Command::new(executable),
    };
    let output = command
        .args(["--exact", HELPER, "--ignored", "--nocapture"])
        .env_clear()
        .env("ARANY_TEST_SUPERVISOR_MODE", mode.as_str())
        .env("ARANY_TEST_SUPERVISOR_ROOT", root.path())
        .current_dir(root.path())
        .bounded_output_for(Duration::from_secs(30), 32 * 1024)
        .unwrap();
    assert!(
        output.status.success(),
        "supervisor {mode:?}: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    crate::process::assert_single_test_completed(&output.stdout, "production supervisor entry");
    assert!(output.stderr.is_empty());
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn supervisor_admission_and_namespace_denials_precede_probe_and_payload() {
    entry_case(SupervisorMode::UnalignedProbe);
    entry_case(SupervisorMode::NamespaceDenied);
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux supervisor namespaces and fresh tmpfs"
)]
fn supervisor_probes_tmpfs_clears_caps_and_reaps_bwrap() {
    entry_case(SupervisorMode::ProbeDenied);
    entry_case(SupervisorMode::Success);
    entry_case(SupervisorMode::Replacement);
    entry_case(SupervisorMode::MountFlags);
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux overlay probe and host submount refusal"
)]
fn supervisor_overlay_probe_and_submount_refusal_clean_up_before_payload() {
    entry_case(SupervisorMode::OverlayProbeDenied);
    entry_case(SupervisorMode::Submount);
}

#[test]
#[ignore = "isolated production-supervisor fixture"]
fn supervisor_entry_helper() {
    use SupervisorMode::*;
    let mode = SupervisorMode::parse(&std::env::var("ARANY_TEST_SUPERVISOR_MODE").unwrap());
    let root = PathBuf::from(std::env::var_os("ARANY_TEST_SUPERVISOR_ROOT").unwrap());
    let (executable, file, launch) = entry_launch(&root, mode);
    match mode {
        Submount => submount_fixture(&root),
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Success
        | Replacement | MountFlags => {}
    }
    let namespace = root.join("namespace");
    deny_entry(mode);
    let path = match mode {
        Replacement => PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        )),
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Submount
        | Success | MountFlags => executable.clone(),
    };
    let mut command = entry_command(&path, &launch);
    match mode {
        Replacement => {
            command.args(["/arany", "--version"]);
            let replacement = root.join("replacement");
            std::fs::write(
                &replacement,
                b"#!/bin/sh\nprintf 'replacement executed\\n'\n",
            )
            .unwrap();
            std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(replacement, &executable).unwrap();
        }
        MountFlags => {
            command.args(["/arany", "--version"]);
        }
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Submount
        | Success => {
            command.args(["/usr/bin/sh", "-c",
                "set -eu; count=0; while IFS= read -r line; do case \"$line\" in CapInh:*|CapPrm:*|CapEff:*|CapBnd:*|CapAmb:*) set -- $line; test \"$2\" = 0000000000000000; count=$((count + 1));; NoNewPrivs:*) set -- $line; test \"$2\" = 1;; esac; done < /proc/self/status; test \"$count\" = 5; printf 'capabilities cleared\\n'"]);
        }
    }
    let output = command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir("/")
        .bounded_output_for(Duration::from_secs(20), 16 * 1024)
        .unwrap();
    let expected = match mode {
        Success => Ok("capabilities cleared\n".to_owned()),
        Replacement | MountFlags => Ok(format!("arany {}\n", env!("CARGO_PKG_VERSION"))),
        UnalignedProbe => Err("admission"),
        NamespaceDenied => Err("namespace_entry"),
        ProbeDenied | OverlayProbeDenied | Submount => Err("runtime_mounts"),
    };
    match expected {
        Ok(stdout) => {
            assert!(
                output.status.success(),
                "post-clear bwrap: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, stdout.as_bytes());
            assert!(output.stderr.is_empty());
        }
        Err(stage) => {
            assert!(!output.status.success());
            assert!(
                output.stdout.is_empty(),
                "no payload or READY before probe admission"
            );
            let failure: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(failure, json!({"stage":stage}));
        }
    }
    drop(file);
    match mode {
        Submount => {
            rustix::mount::unmount(
                root.join("workspace/vendor"),
                rustix::mount::UnmountFlags::empty(),
            )
            .unwrap();
        }
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Success
        | Replacement | MountFlags => {}
    }
    match mode {
        OverlayProbeDenied | Submount => std::fs::remove_dir_all(root.join("workspace")).unwrap(),
        UnalignedProbe | NamespaceDenied | ProbeDenied | Success | Replacement | MountFlags => {}
    }
    std::fs::remove_dir(namespace).unwrap();
    std::fs::remove_file(executable).unwrap();
    match mode {
        MountFlags => {
            let directory = root.join("restricted");
            rustix::mount::unmount(&directory, rustix::mount::UnmountFlags::empty()).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Submount
        | Success | Replacement => {}
    }
}

fn entry_launch(root: &Path, mode: SupervisorMode) -> (PathBuf, std::fs::File, Value) {
    use SupervisorMode::*;
    let namespace = root.join("namespace");
    std::fs::create_dir(&namespace).unwrap();
    let installed = Path::new(env!("CARGO_BIN_EXE_arany"));
    let executable = match mode {
        MountFlags => restricted_executable(root, installed),
        UnalignedProbe | NamespaceDenied | ProbeDenied | OverlayProbeDenied | Submount
        | Success | Replacement => {
            let path = root.join("arany");
            std::fs::copy(installed, &path).unwrap();
            path
        }
    };
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut hash = Sha256::new();
    let mut file = std::fs::File::open(&executable).unwrap();
    let metadata = file.metadata().unwrap();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let digest: [u8; 32] = hash.finalize().into();
    let bytes = match mode {
        UnalignedProbe => 1,
        NamespaceDenied | ProbeDenied | OverlayProbeDenied | Submount | Success | Replacement
        | MountFlags => arany::ToolLimits::default().project_bytes,
    };
    let mut launch = json!({"pin":{"device":metadata.dev(),"inode":metadata.ino(),"sha256":digest},
        "probe":{"root":namespace,"bytes":bytes}});
    match mode {
        OverlayProbeDenied | Submount => {
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let metadata = std::fs::metadata(&workspace).unwrap();
            launch["overlay"] = json!({"path":workspace,"device":metadata.dev(),"inode":metadata.ino(),"digest":vec![0; 32]});
        }
        UnalignedProbe | NamespaceDenied | ProbeDenied | Success | Replacement | MountFlags => {}
    }
    (executable, file, launch)
}

fn restricted_executable(root: &Path, installed: &Path) -> PathBuf {
    use rustix::mount::MountFlags;
    let directory = root.join("restricted");
    std::fs::create_dir(&directory).unwrap();
    let options = std::ffi::CString::new(format!(
        "size={},nr_inodes=2,mode=0700",
        std::fs::metadata(installed).unwrap().len()
    ))
    .unwrap();
    rustix::mount::mount(
        "tmpfs",
        &directory,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOATIME | MountFlags::NODIRATIME,
        options.as_c_str(),
    )
    .unwrap();
    let executable = directory.join("arany");
    std::fs::copy(installed, &executable).unwrap();
    executable
}

fn deny_entry(mode: SupervisorMode) {
    use SupervisorMode::*;
    let syscall = match mode {
        NamespaceDenied => nix::libc::SYS_unshare,
        ProbeDenied => nix::libc::SYS_fsetxattr,
        OverlayProbeDenied => nix::libc::SYS_umount2,
        UnalignedProbe | Submount | Success | Replacement | MountFlags => return,
    };
    let filter: seccompiler::BpfProgram = seccompiler::SeccompFilter::new(
        std::collections::BTreeMap::from([(syscall, Vec::new())]),
        seccompiler::SeccompAction::Allow,
        seccompiler::SeccompAction::Errno(nix::libc::EPERM as u32),
        std::env::consts::ARCH.try_into().unwrap(),
    )
    .unwrap()
    .try_into()
    .unwrap();
    seccompiler::apply_filter_all_threads(&filter).unwrap();
}

fn entry_command(path: &Path, launch: &Value) -> Command {
    let mut command = Command::new(path);
    command
        .args(["--internal-tool-guard", "--supervisor"])
        .arg(launch.to_string())
        .arg("--")
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--die-with-parent",
            "--cap-drop",
            "ALL",
            "--clearenv",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--chdir",
            "/",
        ]);
    command
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux helper death after GO and closed replay"
)]
fn helper_death_after_go_stays_uncertain_without_retry_or_publication() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline =
        arany::test_support::engine::deadline("supervisor helper death", SPILL_JOURNEY_WAIT);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (root, loss) in [
                ("src", LostEvidence::Helper),
                (".", LostEvidence::Helper),
                (".", LostEvidence::Completion),
            ] {
                assert_lost_guard_evidence(root, loss).await;
            }
        });
}

#[derive(Clone, Copy)]
enum LostEvidence {
    Helper,
    Completion,
}

async fn assert_lost_guard_evidence(root: &str, loss: LostEvidence) {
    let fixture = SpillFixture::new();
    fixture.set_workspace_root(root);
    let session = fixture
        .engine
        .create_session(fixture.workspace.clone(), None)
        .await
        .unwrap();
    let script = match loss {
        LostEvidence::Helper => "printf private > src/current.txt; kill -KILL \"$PPID\"",
        LostEvidence::Completion => {
            "printf private > src/current.txt; printf overlay-command-completed"
        }
    };
    let command = ToolCall::Command {
        program: "bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: String::new(),
    };
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        crate::tool_outcome(command.clone()),
    ]));
    let mut request = proposal_request(&fixture.workspace);
    request.session_id = Some(session);
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let completion_loss = matches!(loss, LostEvidence::Completion);
    let inject = async {
        if completion_loss {
            let action = inbox
                .next()
                .await
                .expect("one exact completion-loss intent");
            assert_eq!(action.intent.call, command);
            let entered = arany::FaultPlan::lose_guard_completion(action.intent.id);
            action.decide(true);
            entered
                .await
                .expect("the overlay completion reader reached the injected loss");
        }
    };
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: completion_loss.then_some(approvals),
        ..Default::default()
    };
    let (outcome, ()) = tokio::time::timeout(SPILL_JOURNEY_WAIT, async {
        tokio::join!(
            run_with(&fixture.engine, &provider, request, controls),
            inject
        )
    })
    .await
    .expect("bounded lost Guard evidence journey");
    assert!(matches!(
        outcome,
        Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
    ));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_uncertain_guard_replay(fixture, session, &command).await;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

async fn assert_uncertain_guard_replay(
    fixture: SpillFixture,
    session: arany::SessionId,
    command: &ToolCall,
) {
    fixture.engine.close().await.unwrap();
    let (events, view) = arany::test_support::goals::replayed(&fixture.state, session).await;
    let run = &view.runs[0];
    assert_eq!(run.tools.len(), 1);
    let observation = run.tools[0].observation.as_ref().unwrap();
    assert_eq!(&observation.intent.call, command);
    assert_eq!(observation.disposition, ToolDisposition::Uncertain);
    assert_eq!(observation.enforcement_cause, None);
    assert_eq!(view.run_stop(run), Some(arany::RunStop::Uncertain));
    assert!(!events.iter().any(|event| matches!(&event.event, arany::Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::ApplyChangeset { .. }))));
    assert_eq!(
        std::fs::read(fixture.workspace.join("src/current.txt")).unwrap(),
        b"current"
    );
}

fn submount_fixture(root: &Path) {
    let target = root.join("workspace/vendor");
    std::fs::create_dir(&target).unwrap();
    rustix::mount::mount(
        "tmpfs",
        target,
        "tmpfs",
        rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NODEV,
        c"size=4096,nr_inodes=2",
    )
    .unwrap();
}
