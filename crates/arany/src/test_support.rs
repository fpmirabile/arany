pub mod admissions;
pub mod approval;
pub mod engine;
pub mod git;
pub mod goals;
#[cfg(unix)]
pub mod process;
pub mod routing;
pub mod scenario;
#[cfg(unix)]
pub mod worker_teams;

pub use crate::provider::test_image;
pub use crate::tools::types::MAX_TOOL_CONTEXT_BYTES;

pub fn payload_version(kind: &str) -> Option<i64> {
    crate::session::payload_version(kind)
}

pub fn tool_context_bytes(context: &crate::ToolContext) -> usize {
    context.rendered_len()
}

static ACCOUNT_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
#[cfg(unix)]
static ACCOUNT_LEASE: std::sync::OnceLock<cap_std::fs::File> = std::sync::OnceLock::new();

pub fn isolate_account_root() -> &'static std::path::Path {
    ACCOUNT_ROOT
        .get_or_init(|| {
            #[cfg(debug_assertions)]
            if let Some(path) = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT") {
                let path = std::path::PathBuf::from(path);
                assert!(path.is_absolute(), "absolute inherited test account root");
                #[cfg(unix)]
                if let Some(lease) = account_roots::inherit(&path).expect("inherited account lease")
                {
                    ACCOUNT_LEASE.set(lease).expect("one account lease");
                }
                return path;
            }
            #[cfg(unix)]
            {
                // macOS `/tmp` is a symlink, which private admission refuses.
                let parent = std::fs::canonicalize(std::env::temp_dir())
                    .expect("host temporary directory")
                    .join(account_roots::parent_name());
                let (path, lease) =
                    account_roots::create_in(&parent, 128).expect("private test account root");
                ACCOUNT_LEASE.set(lease).expect("one account lease");
                path
            }
            #[cfg(not(unix))]
            tempfile::tempdir()
                .expect("private test account root")
                .keep()
        })
        .as_path()
}

pub(crate) fn account_root() -> Option<&'static std::path::Path> {
    ACCOUNT_ROOT.get().map(std::path::PathBuf::as_path)
}

type ToolFailure = (
    uuid::Uuid,
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<String>,
);

const MAX_TOOL_FAILURES: usize = 256;
static TOOL_FAILURES: std::sync::Mutex<std::collections::VecDeque<ToolFailure>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

// The development log is debug-only; release test binaries still need the closed stage and check.
pub(crate) fn record_tool_failure(
    intent: uuid::Uuid,
    site: &'static str,
    stage: &'static str,
    check: Option<&'static str>,
    detail: Option<String>,
) {
    let mut failures = TOOL_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failures.len() == MAX_TOOL_FAILURES {
        failures.pop_front();
    }
    failures.push_back((intent, site, stage, check, detail));
}

pub fn recorded_tool_failure(intent: uuid::Uuid) -> Option<String> {
    TOOL_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .rev()
        .find(|failure| failure.0 == intent)
        .map(|(_, site, stage, check, detail)| {
            let check = check.map(|check| format!(" check={check}"));
            let detail = detail.as_ref().map(|detail| format!(" detail={detail}"));
            format!(
                "{site} stage={stage}{}{}",
                check.unwrap_or_default(),
                detail.unwrap_or_default()
            )
        })
}

#[cfg(unix)]
mod account_roots {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::{
        Dir, DirBuilder, DirBuilderExt, File, MetadataExt, OpenOptions, OpenOptionsExt,
    };
    use rustix::fs::{FlockOperation, flock};
    use std::{
        io,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    const PREFIX: &str = "root-";
    const MAX_ENTRIES: usize = 256;
    const MAX_PRUNE_ENTRIES: usize = 32_768;
    const MAX_DEPTH: usize = 16;

    pub(super) fn parent_name() -> String {
        format!(
            "arany-test-accounts-{}",
            rustix::process::geteuid().as_raw()
        )
    }

    fn directory(path: &Path, create: bool) -> io::Result<crate::private_fs::PrivateDirectory> {
        crate::private_fs::open_directory(path, create)
            .map_err(|error| io::Error::other(format!("private account fixture: {error:?}")))
    }

    fn lease(dir: &Dir, name: &str) -> io::Result<File> {
        let file = dir.open_with(
            name,
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .follow(FollowSymlinks::No),
        )?;
        crate::private_fs::check_acl(&file, true)
            .map_err(|error| io::Error::other(format!("private account lease: {error:?}")))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.len() != 0
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other("unsafe account fixture lease"));
        }
        Ok(file)
    }

    #[cfg(any(debug_assertions, test))]
    pub(super) fn inherit(path: &Path) -> io::Result<Option<File>> {
        if !path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(PREFIX.as_bytes()))
            || !path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == parent_name().as_str())
        {
            return Ok(None);
        }
        let parent = directory(path.parent().expect("managed root parent"), false)?;
        let _namespace = namespace(&parent.dir)?;
        let root = directory(path, false)?;
        let lease = lease(&root.dir, ".lease")?;
        flock(&lease, FlockOperation::NonBlockingLockShared)?;
        Ok(Some(lease))
    }

    pub(super) fn create_in(parent: &Path, limit: usize) -> io::Result<(PathBuf, File)> {
        let parent = directory(parent, true)?;
        let _namespace = namespace(&parent.dir)?;
        prune(&parent.path, &parent.dir, limit)?;
        let name = format!("{PREFIX}{}", uuid::Uuid::now_v7());
        parent
            .dir
            .create_dir_with(&name, DirBuilder::new().mode(0o700))?;
        let root = parent.dir.open_dir_nofollow(&name)?;
        let lease = lease(&root, ".lease")?;
        flock(&lease, FlockOperation::NonBlockingLockShared)?;
        Ok((parent.path.join(name), lease))
    }

    fn namespace(parent: &Dir) -> io::Result<File> {
        let namespace = lease(parent, ".namespace")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match flock(&namespace, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(namespace)
    }

    fn prune(path: &Path, parent: &Dir, limit: usize) -> io::Result<()> {
        let mut retained = 0;
        let mut remaining = MAX_PRUNE_ENTRIES;
        for (index, entry) in parent.entries()?.enumerate() {
            if index >= MAX_ENTRIES {
                return Err(io::Error::other("account fixture entry limit"));
            }
            let entry = entry?;
            let name = entry.file_name();
            if !name.as_encoded_bytes().starts_with(PREFIX.as_bytes()) {
                continue;
            }
            retained += 1;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let root = directory(&path.join(&name), false)?;
            let lease = lease(&root.dir, ".lease")?;
            match flock(&lease, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {
                    remove_contents(&root.dir, &mut remaining, 0)?;
                    parent.remove_dir(&name)?;
                    retained -= 1;
                }
                Err(rustix::io::Errno::WOULDBLOCK) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if retained >= limit {
            return Err(io::Error::other("account fixture root limit"));
        }
        Ok(())
    }

    fn remove_contents(dir: &Dir, remaining: &mut usize, depth: usize) -> io::Result<()> {
        if depth >= MAX_DEPTH {
            return Err(io::Error::other("account fixture depth limit"));
        }
        for entry in dir.entries()? {
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("account fixture pruning limit"))?;
            let entry = entry?;
            let name = entry.file_name();
            if entry.file_type()?.is_dir() {
                let child = dir.open_dir_nofollow(&name)?;
                remove_contents(&child, remaining, depth + 1)?;
                dir.remove_dir(&name)?;
            } else {
                dir.remove_file(&name)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        #[test]
        fn pruning_removes_abandoned_roots_and_preserves_live_roots_and_outside_files() {
            let temp = tempfile::tempdir().unwrap();
            let parent = temp.path().join(parent_name());
            let (live, owner) = create_in(&parent, 4).unwrap();
            let borrower = inherit(&live).unwrap().unwrap();
            drop(owner);
            let (stale, owner) = create_in(&parent, 4).unwrap();
            std::fs::create_dir(stale.join("guard")).unwrap();
            std::fs::write(stale.join("guard/lock"), b"stale").unwrap();
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"outside").unwrap();
            symlink(&outside, stale.join("guard/alias")).unwrap();
            let alias = parent.join("root-alias");
            symlink(&outside, &alias).unwrap();
            drop(owner);
            let (next, owner) = create_in(&parent, 4).unwrap();
            assert!(!stale.exists());
            assert!(live.is_dir() && next.is_dir());
            assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"outside");
            assert!(std::fs::symlink_metadata(&alias).unwrap().is_symlink());
            assert_eq!(
                std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
            drop((borrower, owner));
            let (_last, _owner) = create_in(&parent, 4).unwrap();
            assert!(!live.exists() && !next.exists());
            assert!(outside.join("keep").exists());
            let alias_parent = temp.path().join("alias-parent");
            symlink(&outside, &alias_parent).unwrap();
            assert!(create_in(&alias_parent, 4).is_err());
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        }

        #[test]
        fn root_quota_rejects_live_overflow_and_recovers_after_release() {
            let temp = tempfile::tempdir().unwrap();
            let parent = temp.path().join(parent_name());
            let (first, first_lease) = create_in(&parent, 2).unwrap();
            let (_second, _second_lease) = create_in(&parent, 2).unwrap();
            assert!(create_in(&parent, 2).is_err());
            assert!(first.is_dir());
            drop(first_lease);
            let (_third, _third_lease) = create_in(&parent, 2).unwrap();
            assert!(!first.exists());
        }
    }
}

pub async fn exercise_private_guard_command(
    state: &crate::StateRoot,
    workspace: &std::path::Path,
    executable: std::path::PathBuf,
    case: &str,
) -> Result<(), crate::tools::ToolError> {
    use crate::session::worker::*;
    use crate::tools::{
        Attestation, Cancellation, ToolCall, ToolDisposition, ToolRuntime, worker::View,
    };
    let runtime = ToolRuntime::admit(
        crate::tools::config::Config::load(state)?,
        None,
        None,
        workspace,
        state,
        executable,
    )?;
    let grant = WorkerGrant {
        commands: vec!["sh".into()],
        read_paths: vec!["owned".into()],
        write_paths: vec!["owned".into()],
        tools: vec![WorkerTool::Read, WorkerTool::Write, WorkerTool::Command],
        skills: vec![],
        mcp_servers: vec![],
        permission_mode: None,
    };
    let identity = crate::store::BuildCacheIdentity::new(
        workspace,
        runtime.workspace_identity().device,
        runtime.workspace_identity().inode,
    )
    .unwrap();
    let cache_source = tempfile::tempdir().unwrap();
    std::fs::create_dir(cache_source.path().join("target")).unwrap();
    std::fs::write(
        cache_source.path().join("target/marker"),
        b"assignment-cache",
    )
    .unwrap();
    let directory =
        cap_std::fs::Dir::open_ambient_dir(cache_source.path(), cap_std::ambient_authority())
            .unwrap();
    let mut archive = Vec::new();
    #[cfg(target_os = "linux")]
    crate::store::capture_archive_with_limit(&directory, &identity, &[], &mut archive, 64 << 20)
        .unwrap();
    #[cfg(target_os = "macos")]
    crate::store::capture_archive_async_with_limit(
        &directory,
        &identity,
        &[],
        &mut archive,
        64 << 20,
    )
    .await
    .unwrap();
    let mut primary = state
        .begin_build_cache_with_limits(identity.clone(), 64 << 20, 64 << 20)
        .unwrap();
    primary.publish(&mut archive.as_slice(), &[]).unwrap();
    drop(primary);
    let run_id = crate::RunId::new();
    let mut view = View::capture(
        &runtime,
        run_id,
        crate::AgentRunId::new(),
        grant.clone(),
        WORKER_DATA_BYTES as u32,
    )?;
    view.pin_command_cache(&runtime, 64 << 20)?;
    let mut sibling = View::capture(
        &runtime,
        run_id,
        crate::AgentRunId::new(),
        grant,
        WORKER_DATA_BYTES as u32,
    )?;
    sibling.pin_command_cache(&runtime, 64 << 20)?;
    assert!(matches!(
        state.begin_build_cache_with_limits(identity.clone(), 64 << 20, 0),
        Err(crate::StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_build_cache(identity.clone()),
        Err(crate::StoreError::BuildCacheBusy)
    ));
    let script = match case {
        "success" => {
            "test ! -e .git && test \"$(cat owned/a)\" = original && test \"$(cat \"$CARGO_TARGET_DIR/marker\")\" = assignment-cache && printf poisoned >\"$CARGO_TARGET_DIR/marker\" && printf private >owned/a && (sleep 300 >/dev/null 2>&1 & echo $! >owned/daemon) && printf done"
        }
        "outside" => "printf private >owned/a; printf forbidden >outside",
        "nonzero" => "printf private >owned/a; exit 7",
        "link" => "printf private >owned/a; ln -s a owned/link",
        "cancelled" | "lost" => "printf private >owned/a",
        _ => unreachable!(),
    };
    let mut intent = runtime.intent(
        run_id,
        view.assignment(),
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: ".".into(),
        },
    );
    intent.target = view.target();
    intent.limits.cache_requested_bytes = 64 << 20;
    intent.limits.cache_bytes = 64 << 20;
    intent.limits.cache_candidate_bytes = 0;
    intent.limits.cache_transfer_ms = intent.limits.runtime_ms;
    for stale_revision in [false, true] {
        let mut stale = intent.clone();
        let EffectTarget::WorkerView {
            revision,
            cache_generation,
            ..
        } = &mut stale.target
        else {
            unreachable!()
        };
        if stale_revision {
            *revision += 1;
        } else {
            *cache_generation = Some([9; 32]);
        }
        let refused = view
            .command(&runtime, stale, Cancellation::new())
            .await
            .err()
            .expect("stale pin refused");
        assert_eq!(refused.disposition, ToolDisposition::Denied);
        assert!(refused.attestation.is_none());
    }
    if case == "lost" {
        crate::fault::FaultPlan::lose_guard_receipt(intent.id);
    }
    let cancellation = Cancellation::new();
    if case == "cancelled" {
        cancellation.cancel();
    }
    match view.command(&runtime, intent.clone(), cancellation).await {
        Ok(candidate) => {
            assert_eq!(case, "success");
            let reference = WorkerDataRef {
                assignment_id: view.assignment(),
                purpose: WorkerDataPurpose::Mutation,
                first_sequence: 1,
                seal_sequence: candidate.data.len().div_ceil(WORKER_CHUNK_BYTES) as u64 + 1,
                chunks: candidate.data.len().div_ceil(WORKER_CHUNK_BYTES) as u16,
                bytes: candidate.data.len() as u32,
                sha256: crate::tools::types::digest(&candidate.data),
            };
            let observation = view.acknowledge(candidate, reference)?;
            assert!(observation.valid());
            assert!(matches!(
                observation.attestation,
                Some(Attestation::WorkerCommand { .. })
            ));
        }
        Err(observation) => match case {
            "lost" => {
                assert_eq!(observation.disposition, ToolDisposition::Uncertain);
                assert!(
                    view.command(&runtime, intent, Cancellation::new())
                        .await
                        .is_err()
                );
            }
            "cancelled" => assert!(observation.attestation.is_none()),
            _ => {
                assert!(observation.valid(), "{case}: {observation:?}");
                let result: WorkerCommandResult =
                    serde_json::from_str(&observation.output).unwrap();
                if case == "nonzero" {
                    assert_eq!(result.exit_code, Some(7));
                    assert_eq!(result.private_capture, PrivateCapture::None {});
                } else if cfg!(target_os = "macos") && case == "link" {
                    // macOS ships `ln` as a hard link, which the single-link system catalogue refuses.
                    assert_eq!(result.exit_code, Some(127), "{case}: {result:?}");
                    assert_eq!(result.private_capture, PrivateCapture::None {});
                } else {
                    assert_eq!(
                        result.private_capture,
                        PrivateCapture::Rejected {
                            cause: if case == "outside" {
                                PrivateCaptureRejection::OutOfLease
                            } else {
                                PrivateCaptureRejection::UnsupportedEntry
                            }
                        },
                        "{case}: {result:?}"
                    );
                }
            }
        },
    }
    if case == "success" {
        let mut check = runtime.intent(run_id, view.assignment(), ToolCall::Command {
            program: "sh".into(), args: vec!["-c".into(),
                "test \"$(cat owned/a)\" = private && test -s owned/daemon && test \"$(cat \"$CARGO_TARGET_DIR/marker\")\" = assignment-cache".into()], cwd: ".".into() });
        check.target = view.target();
        check.limits.cache_requested_bytes = 64 << 20;
        check.limits.cache_bytes = 64 << 20;
        check.limits.cache_candidate_bytes = 0;
        check.limits.cache_transfer_ms = check.limits.runtime_ms;
        let observation = view
            .command(&runtime, check, Cancellation::new())
            .await
            .err()
            .expect("second command has no delta");
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{observation:?}"
        );
        assert!(observation.valid());
    }
    assert_eq!(view.dispose()?.revision, u32::from(case == "success"));
    assert_eq!(
        std::fs::read(workspace.join("owned/a")).unwrap(),
        b"original"
    );
    let mut check = runtime.intent(run_id, sibling.assignment(), ToolCall::Command { program: "sh".into(), args: vec!["-c".into(), "test \"$(cat owned/a)\" = original && test \"$(cat \"$CARGO_TARGET_DIR/marker\")\" = assignment-cache".into()], cwd: ".".into() });
    check.target = sibling.target();
    check.limits.cache_requested_bytes = 64 << 20;
    check.limits.cache_bytes = 64 << 20;
    check.limits.cache_candidate_bytes = 0;
    check.limits.cache_transfer_ms = check.limits.runtime_ms;
    let observation = sibling
        .command(&runtime, check, Cancellation::new())
        .await
        .err()
        .expect("empty command capture");
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "{observation:?}"
    );
    assert_eq!(sibling.dispose()?.revision, 0);
    let primary = state
        .begin_build_cache_with_limits(identity, 64 << 20, 0)
        .unwrap();
    let mut committed = primary.archive().unwrap().unwrap().0;
    let mut unchanged = Vec::new();
    std::io::Read::read_to_end(&mut committed, &mut unchanged).unwrap();
    assert_eq!(unchanged, archive);
    Ok(())
}

pub async fn exercise_private_guard_target(
    state: &crate::StateRoot,
    workspace: &std::path::Path,
    executable: std::path::PathBuf,
) -> Result<(), crate::tools::ToolError> {
    use crate::session::worker::*;
    use crate::tools::{Cancellation, ToolCall, ToolDisposition, ToolRuntime, worker::View};
    let runtime = ToolRuntime::admit(
        crate::tools::config::Config::load(state)?,
        None,
        None,
        workspace,
        state,
        executable,
    )?;
    let grant = WorkerGrant {
        commands: Vec::new(),
        read_paths: vec!["owned".into()],
        write_paths: vec!["owned".into()],
        tools: vec![
            WorkerTool::Read,
            WorkerTool::Write,
            WorkerTool::Edit,
            WorkerTool::Mkdir,
        ],
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        permission_mode: None,
    };
    let run_id = crate::RunId::new();
    let mut view = View::capture(
        &runtime,
        run_id,
        crate::AgentRunId::new(),
        grant.clone(),
        WORKER_DATA_BYTES as u32,
    )?;
    view.manifest()?;
    let sibling = View::capture(
        &runtime,
        run_id,
        crate::AgentRunId::new(),
        grant.clone(),
        WORKER_DATA_BYTES as u32,
    )?;
    let original =
        std::fs::read(workspace.join("owned/a")).map_err(|_| crate::tools::ToolError::Operation)?;
    let call = ToolCall::Edit {
        path: "owned/a".into(),
        expected_digest: crate::tools::types::hex_digest(&original),
        old: "original".into(),
        new: "private".into(),
    };
    let mut intent = runtime.intent(view.run_id(), view.assignment(), call);
    intent.target = view.target();
    intent.limits.cache_bytes = 0;
    intent.limits.cache_requested_bytes = 0;
    intent.limits.cache_candidate_bytes = 0;
    let candidate = view
        .edit(&runtime, intent, Cancellation::new())
        .await
        .map_err(|observation| {
            assert_eq!(
                observation.disposition,
                ToolDisposition::Succeeded,
                "private Guard: {:?} {}",
                observation.disposition,
                observation.output
            );
            crate::tools::ToolError::Operation
        })?;
    let chunks: Vec<_> = candidate.chunks().collect();
    assert!(!chunks.is_empty());
    let reference = WorkerDataRef {
        assignment_id: view.assignment(),
        purpose: WorkerDataPurpose::Mutation,
        first_sequence: 1,
        seal_sequence: chunks.len() as u64 + 1,
        chunks: chunks.len() as u16,
        bytes: candidate.data.len() as u32,
        sha256: crate::tools::types::digest(&candidate.data),
    };
    let observation = view.acknowledge(candidate, reference.clone())?;
    assert!(observation.valid());
    for (actor, text) in [(&view, "private"), (&sibling, "original")] {
        let mut read = runtime.intent(
            actor.run_id(),
            actor.assignment(),
            ToolCall::Read {
                path: "owned/a".into(),
                offset: None,
                limit: None,
            },
        );
        read.target = actor.target();
        let read = actor.read(read, None);
        assert_eq!(read.disposition, ToolDisposition::Succeeded);
        let output: serde_json::Value = serde_json::from_str(&read.output).unwrap();
        assert!(
            output["text"]
                .as_str()
                .unwrap()
                .starts_with(&format!("1\t{text}"))
        );
    }
    assert!(
        std::fs::read(workspace.join("owned/a")).unwrap() == original,
        "Workspace changed before private integration"
    );
    let mut idle = View::capture(
        &runtime,
        run_id,
        crate::AgentRunId::new(),
        grant.clone(),
        WORKER_DATA_BYTES as u32,
    )?;
    let mut idle_intent = runtime.intent(
        run_id,
        idle.assignment(),
        ToolCall::Edit {
            path: "owned/a".into(),
            expected_digest: crate::tools::types::hex_digest(&original),
            old: "original".into(),
            new: "never".into(),
        },
    );
    idle_intent.target = idle.target();
    idle_intent.limits.cache_requested_bytes = 0;
    idle_intent.limits.cache_bytes = 0;
    idle_intent.limits.cache_candidate_bytes = 0;
    let cancelled = Cancellation::new();
    cancelled.cancel();
    assert!(idle.edit(&runtime, idle_intent, cancelled).await.is_err());
    assert_eq!(idle.dispose()?.revision, 0);
    let mut lost = View::capture(
        &runtime,
        run_id,
        crate::AgentRunId::new(),
        grant,
        WORKER_DATA_BYTES as u32,
    )?;
    let mut lost_intent = runtime.intent(
        run_id,
        lost.assignment(),
        ToolCall::Edit {
            path: "owned/a".into(),
            expected_digest: crate::tools::types::hex_digest(&original),
            old: "original".into(),
            new: "discarded".into(),
        },
    );
    lost_intent.target = lost.target();
    lost_intent.limits.cache_requested_bytes = 0;
    lost_intent.limits.cache_bytes = 0;
    lost_intent.limits.cache_candidate_bytes = 0;
    crate::fault::FaultPlan::lose_guard_receipt(lost_intent.id);
    let unknown = lost
        .edit(&runtime, lost_intent.clone(), Cancellation::new())
        .await
        .err()
        .expect("lost private receipt");
    assert_eq!(unknown.disposition, ToolDisposition::Uncertain);
    assert!(matches!(
        unknown.intent.target,
        EffectTarget::WorkerView { .. }
    ));
    assert!(
        lost.edit(&runtime, lost_intent, Cancellation::new())
            .await
            .is_err()
    );
    assert_eq!(lost.dispose()?.revision, 0);
    assert!(
        std::fs::read(workspace.join("owned/a")).unwrap() == original,
        "uncertain private edit published to the Workspace"
    );
    let bytes = view.handover()?;
    assert!(bytes.len() > crate::tools::types::MAX_CHANGESET_BYTES);
    let mut capture = runtime.intent(
        view.run_id(),
        view.assignment(),
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: view.assignment(),
                purpose: WorkerDataPurpose::Handover,
                max_chunks: reference.chunks,
                max_bytes: reference.bytes,
            },
        },
    );
    capture.target = view.target();
    capture.limits.cache_bytes = 0;
    capture.limits.cache_requested_bytes = 0;
    capture.limits.cache_candidate_bytes = 0;
    let candidate = view
        .edit(&runtime, capture, Cancellation::new())
        .await
        .map_err(|observation| {
            assert_eq!(
                observation.disposition,
                ToolDisposition::Succeeded,
                "private capture: {:?} {}",
                observation.disposition,
                observation.output
            );
            crate::tools::ToolError::Operation
        })?;
    let observation = view.acknowledge(candidate, reference.clone())?;
    assert!(matches!(
        observation.attestation,
        Some(crate::tools::Attestation::WorkerData { .. })
    ));
    assert_eq!(view.dispose()?.revision, 1);
    let publication = runtime.intent(
        run_id,
        crate::AgentRunId::new(),
        ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover {
                assignment_id: reference.assignment_id,
                handover_sequence: reference.seal_sequence + 1,
                data: reference,
            },
        },
    );
    let observation = runtime
        .apply_worker_handover(publication, bytes.into(), Cancellation::new())
        .await;
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "private publication: {:?} {}",
        observation.disposition,
        observation.output
    );
    let updated = std::fs::read(workspace.join("owned/a")).unwrap();
    assert!(updated.starts_with(b"private"));
    assert_eq!(updated.len() + 1, original.len());
    let changes = WorkerChangeset {
        files: (0..WORKER_FILES)
            .map(|i| crate::tools::FileChange {
                path: format!("owned/batch-{i:04}"),
                before: None,
                after: Some(crate::tools::FileVersion::from_bytes(b"", false)),
            })
            .collect(),
    };
    let result = publish_worker_fixture(&runtime, run_id, &changes).await;
    assert_eq!(
        result.disposition,
        ToolDisposition::Succeeded,
        "maximum-count Guard publication: {:?} {}",
        result.disposition,
        result.output
    );
    assert_eq!(
        std::fs::read_dir(workspace.join("owned")).unwrap().count(),
        WORKER_FILES + 1
    );

    let before = vec![b'a'; 1024 * 1024];
    let after = vec![b'b'; 1024 * 1024];
    let mut maximum = WorkerChangeset { files: Vec::new() };
    for i in 0..16 {
        let path = format!("owned/sized-{i:04}");
        std::fs::write(workspace.join(&path), &before).unwrap();
        maximum.files.push(crate::tools::FileChange {
            path,
            before: Some(crate::tools::FileVersion::from_bytes(&before, false)),
            after: Some(crate::tools::FileVersion::from_bytes(&after, false)),
        });
    }
    let mut overflow = maximum.clone();
    overflow.files.push(crate::tools::FileChange {
        path: "owned/sized-overflow".into(),
        before: None,
        after: Some(crate::tools::FileVersion::from_bytes(b"x", false)),
    });
    let refused = publish_worker_fixture(&runtime, run_id, &overflow).await;
    assert_eq!(refused.disposition, ToolDisposition::Failed);
    assert!(refused.attestation.is_none());
    assert!(!workspace.join("owned/sized-overflow").exists());
    let result = publish_worker_fixture(&runtime, run_id, &maximum).await;
    assert_eq!(
        result.disposition,
        ToolDisposition::Succeeded,
        "maximum-byte Guard publication: {:?} {}",
        result.disposition,
        result.output
    );
    for change in &maximum.files {
        assert!(
            std::fs::read(workspace.join(&change.path)).unwrap() == after,
            "maximum-byte file content"
        );
    }

    Ok(())
}

async fn publish_worker_fixture(
    runtime: &crate::tools::ToolRuntime,
    run_id: crate::RunId,
    changes: &crate::session::worker::WorkerChangeset,
) -> crate::tools::ToolObservation {
    use crate::session::worker::*;
    let bytes = serde_json::to_vec(changes).unwrap();
    let assignment_id = crate::AgentRunId::new();
    let data = WorkerDataRef {
        assignment_id,
        purpose: WorkerDataPurpose::Handover,
        first_sequence: 1,
        seal_sequence: bytes.len().div_ceil(WORKER_CHUNK_BYTES) as u64 + 1,
        chunks: u16::try_from(bytes.len().div_ceil(WORKER_CHUNK_BYTES)).unwrap(),
        bytes: u32::try_from(bytes.len()).unwrap(),
        sha256: crate::tools::types::digest(&bytes),
    };
    let mut publication = runtime.intent(
        run_id,
        crate::AgentRunId::new(),
        crate::tools::ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover {
                assignment_id,
                handover_sequence: data.seal_sequence + 1,
                data,
            },
        },
    );
    publication.limits.runtime_ms = 120_000;
    runtime
        .apply_worker_handover(publication, bytes.into(), crate::tools::Cancellation::new())
        .await
}
