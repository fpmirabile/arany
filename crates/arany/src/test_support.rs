pub mod admissions;
pub mod approval;
pub mod engine;
pub mod git;
pub mod goals;
pub mod latency;
#[cfg(unix)]
pub mod process;
pub mod routing;
pub mod scenario;
#[cfg(target_os = "linux")]
pub use crate::session::selection_failure_session;
#[cfg(unix)]
pub mod worker_teams;

pub use crate::provider::test_image;
pub use crate::tools::types::MAX_TOOL_CONTEXT_BYTES;

pub const MAX_SESSION_EVENTS: usize = crate::store::MAX_SESSION_EVENTS;

pub const fn max_changeset_bytes() -> usize {
    crate::tools::types::MAX_CHANGESET_BYTES
}

pub const fn max_changeset_files() -> usize {
    crate::tools::types::MAX_CHANGESET_FILES
}

pub async fn record_setup_edit_state(
    store: &crate::Store,
    session_id: crate::SessionId,
    uncertain: bool,
) -> uuid::Uuid {
    let operation_id = if uncertain {
        store
            .load_view(session_id)
            .await
            .expect("setup Session")
            .expect("existing Session")
            .pending_provider_setup_edit
            .expect("pending setup edit")
    } else {
        let operation_id = uuid::Uuid::now_v7();
        store
            .append(
                session_id,
                crate::Event::ProviderSetupEditStarted {
                    operation_id,
                    target: crate::ProviderSetupTarget {
                        kind: crate::ProviderSetupTargetKind::ChatGptAccount,
                        identity: uuid::Uuid::now_v7().to_string(),
                    },
                    expected_digest: None,
                },
            )
            .await
            .expect("start setup edit");
        operation_id
    };
    if uncertain {
        store
            .append(
                session_id,
                crate::Event::ProviderSetupEditFinished {
                    operation_id,
                    resulting_digest: None,
                    disposition: crate::ProviderSetupDisposition::Uncertain,
                },
            )
            .await
            .expect("record uncertain setup edit");
    }
    operation_id
}

pub fn payload_version(kind: &str) -> i64 {
    crate::session::payload_version(kind).expect("known Event kind")
}

pub fn tool_context_bytes(context: &crate::ToolContext) -> usize {
    context.rendered_len()
}

pub fn tool_continuation_reserve(max_output_tokens: u32, feedback: usize) -> usize {
    crate::tools::ToolRuntime::continuation_reserve(
        crate::provider::max_turn_bytes(max_output_tokens),
        feedback,
    )
}

pub fn history_tool_block(run: &crate::RunView) -> String {
    crate::provider::history_tool_block(run)
}

#[derive(Debug)]
#[must_use = "the clock is restored when the guard drops"]
pub struct TestClock {
    previous: Option<u64>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl TestClock {
    pub fn set(&self, now_ms: u64) {
        crate::tools::set_test_clock_ms(Some(now_ms));
    }

    pub fn advance(&self, milliseconds: u64) {
        self.set(crate::tools::now_ms().saturating_add(milliseconds));
    }
}

impl Drop for TestClock {
    fn drop(&mut self) {
        crate::tools::set_test_clock_ms(self.previous);
    }
}

pub fn test_clock(now_ms: u64) -> TestClock {
    TestClock {
        previous: crate::tools::set_test_clock_ms(Some(now_ms)),
        _thread_bound: std::marker::PhantomData,
    }
}

static ACCOUNT_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
#[cfg(unix)]
static ACCOUNT_LEASE: std::sync::OnceLock<cap_std::fs::File> = std::sync::OnceLock::new();

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn inherit_account_root(path: &std::path::Path) {
    assert!(path.is_absolute(), "absolute inherited test account root");
    let lease = account_roots::inherit(path)
        .expect("inherited account lease")
        .expect("managed test account root");
    ACCOUNT_LEASE.set(lease).expect("one account lease");
    ACCOUNT_ROOT
        .set(path.to_path_buf())
        .expect("one test account root");
}

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
    #[cfg(test)]
    isolate_account_root();
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

impl crate::supervisor::SupervisorPolicy {
    pub fn all_off() -> Self {
        let mut policy = Self::default();
        let mut value = serde_json::to_value(&policy.floors).expect("thresholds");
        for v in value
            .as_object_mut()
            .expect("threshold fields")
            .values_mut()
        {
            *v = serde_json::json!(u16::MAX);
        }
        policy.floors = serde_json::from_value(value).expect("thresholds");
        policy.candidates = policy.floors.clone();
        policy.digest = policy.computed_digest();
        policy
    }
}

pub fn host_admission_fixtures() -> Vec<crate::ToolView> {
    use crate::permissions::{Decision, PermissionSource};
    [
        (
            "sh",
            Decision::Ask,
            PermissionSource::UnavailableApproval,
            "Host interpreter requires a matching tools.json command with interpreter: true, executable path and digest; nothing ran",
        ),
        (
            "sh",
            Decision::Ask,
            PermissionSource::UnavailableApproval,
            "Host command refused: permission settings changed outside admission; use /permissions to re-admit them. Nothing ran",
        ),
        (
            "sudo",
            Decision::Deny,
            PermissionSource::Mode,
            "Arany cannot run privilege-escalation commands. Please run this command by hand:\nargv (JSON): [\"sudo\",\"echo\",\"canary\"]\nWorkspace-relative cwd (JSON): \"\"\nNothing ran. Do not repackage this command through a wrapper, shell or child.",
        ),
    ]
    .into_iter()
    .map(|(program, decision, source, output)| host_admission_tool(program, decision, source, output))
    .collect()
}

pub fn uncertain_hook_fixture() -> crate::ToolView {
    use crate::permissions::{
        Decision, HookEvent, HookIgnoreReason, HookInvocation, HookOutcome, HookResult,
        PermissionSource,
    };
    let mut tool = host_admission_tool(
        "sh",
        Decision::Deny,
        PermissionSource::Hook,
        "Hook outcome uncertain; no operation was dispatched",
    );
    let mut observation = tool.observation.as_ref().unwrap().clone();
    observation.intent.id = uuid::Uuid::now_v7();
    observation.intent.call = crate::ToolCall::Command {
        program: "sh".into(),
        args: Vec::new(),
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
    };
    observation.disposition = crate::ToolDisposition::Uncertain;
    observation.cache_update = crate::CacheUpdate::NotAttempted;
    let verdict = tool.permission_verdict.as_mut().unwrap();
    verdict.hook_restriction = Some(Decision::Deny);
    verdict.hooks = 1;
    tool.hooks.push(crate::session::HookView {
        sequence: 1,
        invocation: HookInvocation {
            event: HookEvent::PreToolUse,
            group: 0,
            handler: 0,
            subject_digest: tool.intent.digest(),
            intent: observation.intent.clone(),
            command_subject: None,
            command_tool: None,
            input_digest: [1; 32],
            active_ceiling_ms: None,
        },
        outcome: Some(HookOutcome {
            boundary_active_ms: None,
            observation,
            result: HookResult::Ignored {
                why: HookIgnoreReason::NotCompleted,
            },
        }),
        idle: false,
    });
    tool
}

fn host_admission_tool(
    program: &str,
    decision: crate::permissions::Decision,
    source: crate::permissions::PermissionSource,
    output: &str,
) -> crate::ToolView {
    let intent = crate::EffectIntent {
        host: None,
        rewrite: Vec::new(),
        host_command: None,
        target: crate::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::RunId::new(),
        agent_run_id: crate::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: crate::ToolCall::CommandUnguarded {
            program: program.into(),
            args: vec!["echo".into(), "canary".into()],
            cwd: String::new(),
            justification: "Inspect the command".into(),
            prefix_rule: None,
        },
        limits: crate::ToolLimits::default(),
        use_count: 1,
    };
    crate::ToolView {
        checkpoint: None,
        observation: Some(crate::ToolObservation {
            spill: None,
            intent: intent.clone(),
            disposition: crate::ToolDisposition::Denied,
            enforcement_cause: None,
            limit_cause: None,
            output: output.into(),
            attestation: None,
            approval_cause: None,
            guard_stage: None,
            cache_update: crate::CacheUpdate::NotApplicable,
        }),
        started_sequence: Some(0),
        active_time_reserved: false,
        narrowing_source: None,
        permission_verdict: Some(crate::permissions::PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision,
            mode: (decision == crate::permissions::Decision::Deny)
                .then_some(crate::permissions::PermissionMode::Plan),
            basis: source,
            source,
            approved: false,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
        intent,
        review_call_index: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: Some(1),
    }
}

pub async fn complete_chatgpt_probe_fixture(
    check: &mut crate::ChatGptCheck,
) -> Result<(), crate::StoreError> {
    for kind in [
        crate::CheckCallKind::Answer,
        crate::CheckCallKind::Delegate,
        crate::CheckCallKind::Compaction,
    ] {
        let token = check.journal.call_started(kind).await?;
        check
            .journal
            .call_finished(
                token,
                crate::CheckDisposition::Succeeded,
                Some(crate::CheckUsage {
                    input_tokens: 12,
                    output_tokens: 8,
                }),
                false,
            )
            .await?;
    }
    Ok(())
}
