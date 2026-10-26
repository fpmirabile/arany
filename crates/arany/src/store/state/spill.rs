use super::{StateRoot, StoreError, check_private_record_file, nonblocking, private_subdirectory};
use crate::session::{Event, EventEnvelope, RunId};
use crate::tools::spill::{MAX_SPILL_RESULT_BYTES, MAX_SPILL_RUN_BYTES};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, MetadataExt, OpenOptions, OpenOptionsExt};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct SpillAccess {
    current: Arc<Dir>,
    parents: [Dir; 2],
    files: Mutex<BTreeMap<String, Arc<Dir>>>,
    bytes: Mutex<u64>,
}

impl StateRoot {
    pub(crate) fn admit_spills(
        &self,
        run: RunId,
        history: &[EventEnvelope],
    ) -> Result<SpillAccess, StoreError> {
        self.readmit()?;
        let root = private_subdirectory(&self.dir, "spill")?;
        checked_child(&self.dir, &root)?;
        let current = Arc::new(private_subdirectory(&root, &run.to_string())?);
        checked_child(&root, &current)?;
        let mut files = BTreeMap::new();
        let mut directories = BTreeMap::new();
        for event in history {
            if let Event::ToolFinished {
                run_id,
                observation,
                ..
            } = &event.event
                && let Some(spill) = &observation.spill
            {
                let directory = directories.entry(*run_id).or_insert_with(|| {
                    root.open_dir_nofollow(run_id.to_string())
                        .ok()
                        .filter(|dir| checked_child(&root, dir).is_ok())
                        .map(Arc::new)
                });
                if let Some(directory) = directory {
                    files.insert(spill.r#ref.clone(), Arc::clone(directory));
                }
            }
        }
        Ok(SpillAccess {
            current,
            parents: [self.dir.try_clone()?, root],
            files: Mutex::new(files),
            bytes: Mutex::new(0),
        })
    }
}

impl SpillAccess {
    pub(crate) fn reserve(&self, bytes: usize) -> Result<bool, StoreError> {
        let mut used = self.bytes.lock().map_err(|_| StoreError::Closed)?;
        if bytes > MAX_SPILL_RESULT_BYTES || used.saturating_add(bytes as u64) > MAX_SPILL_RUN_BYTES
        {
            return Ok(false);
        }
        *used += bytes as u64;
        Ok(true)
    }

    pub(crate) fn release(&self, bytes: usize) {
        if let Ok(mut used) = self.bytes.lock() {
            *used = used.saturating_sub(bytes as u64);
        }
    }

    pub(crate) fn write(
        &self,
        reference: &str,
        bytes: &[u8],
        cancelled: &AtomicBool,
        gate: &mut crate::fault::SpillWriteGate,
    ) -> Result<(), StoreError> {
        let check = || {
            if cancelled.load(Ordering::Acquire) {
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        };
        let prepared = (|| -> Result<_, StoreError> {
            check()?;
            if !crate::validate::valid_digest(reference) || bytes.len() > MAX_SPILL_RESULT_BYTES {
                return Err(StoreError::InvalidStateDirectory);
            }
            let lease = writer_lock(&self.current)?;
            check()?;
            let name = format!("{reference}.txt");
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .mode(0o600)
                .follow(FollowSymlinks::No);
            nonblocking(&mut options);
            let file = self.current.open_with(&name, &options)?;
            Ok((lease, file, name))
        })();
        let (_lease, mut file, name) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.release(bytes.len());
                return Err(error);
            }
        };
        let result = (|| {
            check()?;
            file.write_all(bytes)?;
            gate.wait()?;
            check()?;
            file.sync_all()?;
            check()?;
            self.current.open(".")?.sync_all()?;
            for parent in &self.parents {
                check()?;
                parent.open(".")?.sync_all()?;
            }
            check()
        })();
        if let Err(error) = result {
            if self.current.remove_file(&name).is_ok() {
                self.release(bytes.len());
            }
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) fn retain(&self, reference: &str) -> Result<(), StoreError> {
        self.files
            .lock()
            .map_err(|_| StoreError::Closed)?
            .insert(reference.into(), Arc::clone(&self.current));
        Ok(())
    }

    pub(crate) fn read(&self, reference: &str) -> Result<std::fs::File, StoreError> {
        if !crate::validate::valid_digest(reference) {
            return Err(StoreError::InvalidStateDirectory);
        }
        let root = self
            .files
            .lock()
            .map_err(|_| StoreError::Closed)?
            .get(reference)
            .cloned()
            .ok_or(StoreError::InvalidStateDirectory)?;
        crate::private_fs::check_directory(&root, true)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        nonblocking(&mut options);
        let file = root.open_with(format!("{reference}.txt"), &options)?;
        check_private_record_file(&file, MAX_SPILL_RESULT_BYTES)?;
        if file.metadata()?.dev() != root.dir_metadata()?.dev() {
            return Err(StoreError::InvalidStateDirectory);
        }
        Ok(file.into_std())
    }
}

struct WriterLock(cap_std::fs::File);

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

fn writer_lock(dir: &Dir) -> Result<WriterLock, StoreError> {
    let (file, _) = super::lock::open_private_lock_file(dir, "writer.lock", || Ok(()))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(std::io::Error::from)?;
    let lease = WriterLock(file);
    let held = lease.0.metadata()?;
    let current = dir.symlink_metadata("writer.lock")?;
    if held.dev() != current.dev() || held.ino() != current.ino() {
        return Err(StoreError::InvalidStateDirectory);
    }
    Ok(lease)
}

fn checked_child(parent: &Dir, child: &Dir) -> Result<(), StoreError> {
    crate::private_fs::check_directory(child, true)?;
    if parent.dir_metadata()?.dev() != child.dir_metadata()?.dev() {
        return Err(StoreError::InvalidStateDirectory);
    }
    Ok(())
}

pub(crate) const SWEEP_TIME_MS: u64 = 30_000;

struct SweepBudget {
    deadline: std::time::Instant,
    remaining: usize,
}

impl SweepBudget {
    fn spend(&mut self) -> bool {
        if self.remaining == 0 || std::time::Instant::now() >= self.deadline {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

enum SweepProgress {
    Continue,
    BudgetExhausted,
}

pub(in crate::store) fn sweep(
    root: &StateRoot,
    connection: &rusqlite::Connection,
) -> Result<(), StoreError> {
    let spill = match root.dir.open_dir_nofollow("spill") {
        Ok(spill) => spill,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    checked_child(&root.dir, &spill)?;
    let mut budget = SweepBudget {
        deadline: std::time::Instant::now() + std::time::Duration::from_millis(SWEEP_TIME_MS),
        remaining: crate::store::MAX_LIST_SCAN_EVENTS,
    };
    let mut owner = connection
        .prepare("SELECT session_id FROM events WHERE run_id=?1 AND kind='RunStarted' LIMIT 1")?;
    let mut references = connection.prepare("SELECT json_extract(payload, '$.spill.ref') FROM events WHERE run_id=?1 AND kind='ToolFinished'")?;
    for entry in spill.entries()?.take(crate::store::MAX_LIST_SCAN_EVENTS) {
        if std::time::Instant::now() >= budget.deadline {
            return Ok(());
        }
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(run) = name.to_str().and_then(|name| name.parse::<RunId>().ok()) else {
            continue;
        };
        let session = owner.query_row([run.to_string()], |row| row.get::<_, String>(0));
        let Ok(session) = session else {
            continue;
        };
        let Ok(session) = session.parse() else {
            continue;
        };
        let Ok(_lease) = root.lock_session_read_only(session) else {
            continue;
        };
        let directory = match spill.open_dir_nofollow(&name) {
            Ok(directory) => directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) || error.raw_os_error() == Some(nix::libc::ELOOP) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        match checked_child(&spill, &directory) {
            Ok(()) => {}
            Err(StoreError::StateNotPrivate | StoreError::InvalidStateDirectory) => continue,
            Err(error) => return Err(error),
        }
        match sweep_run(&directory, &mut references, run, &mut budget)? {
            SweepProgress::Continue => {}
            SweepProgress::BudgetExhausted => return Ok(()),
        }
    }
    Ok(())
}

fn sweep_run(
    directory: &Dir,
    references: &mut rusqlite::Statement<'_>,
    run: RunId,
    budget: &mut SweepBudget,
) -> Result<SweepProgress, StoreError> {
    let Ok(_writer) = writer_lock(directory) else {
        return Ok(SweepProgress::Continue);
    };
    let mut kept = BTreeSet::new();
    match committed_references(references, run, &mut kept, budget)? {
        SweepProgress::Continue => remove_orphans(directory, &kept, budget),
        SweepProgress::BudgetExhausted => Ok(SweepProgress::BudgetExhausted),
    }
}

fn committed_references(
    references: &mut rusqlite::Statement<'_>,
    run: RunId,
    kept: &mut BTreeSet<String>,
    budget: &mut SweepBudget,
) -> Result<SweepProgress, StoreError> {
    for reference in
        references.query_map([run.to_string()], |row| row.get::<_, Option<String>>(0))?
    {
        if !budget.spend() {
            return Ok(SweepProgress::BudgetExhausted);
        }
        if let Some(reference) = reference?
            && crate::validate::valid_digest(&reference)
        {
            kept.insert(reference);
        }
    }
    Ok(SweepProgress::Continue)
}

fn remove_orphans(
    directory: &Dir,
    kept: &BTreeSet<String>,
    budget: &mut SweepBudget,
) -> Result<SweepProgress, StoreError> {
    for entry in directory.entries()? {
        if !budget.spend() {
            return Ok(SweepProgress::BudgetExhausted);
        }
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(reference) = name.to_str().and_then(|name| name.strip_suffix(".txt")) else {
            continue;
        };
        if crate::validate::valid_digest(reference) && !kept.contains(reference) {
            match directory.remove_file(&name) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        || directory
                            .symlink_metadata(&name)
                            .is_ok_and(|metadata| !metadata.is_file()) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(SweepProgress::Continue)
}

#[cfg(test)]
fn fixture_write(access: &SpillAccess, reference: &str, bytes: &[u8]) -> Result<bool, StoreError> {
    if !access.reserve(bytes.len())? {
        return Ok(false);
    }
    access.write(
        reference,
        bytes,
        &AtomicBool::new(false),
        &mut crate::fault::SpillWriteGate::default(),
    )?;
    access.retain(reference)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::types::{CommandResult, OutputTotal};
    use crate::tools::{
        Attestation, CacheUpdate, EffectIntent, OutputSpill, ToolCall, ToolDisposition, ToolLimits,
        ToolObservation,
    };
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn spill_observation(run: RunId, content: &[u8]) -> ToolObservation {
        let intent = EffectIntent {
            host: None,
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: run,
            agent_run_id: crate::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Command {
                program: "echo".into(),
                args: Vec::new(),
                cwd: String::new(),
            },
            limits: ToolLimits::default(),
            use_count: 1,
        };
        let total = OutputTotal {
            bytes: content.len() as u64,
            lines: 1,
        };
        ToolObservation {
            spill: Some(OutputSpill {
                r#ref: crate::validate::hex_digest(&intent.digest()),
                bytes: total.bytes,
                truncated: false,
                captured: None,
            }),
            attestation: Some(Attestation::for_site(&intent)),
            intent,
            disposition: ToolDisposition::Succeeded,
            enforcement_cause: None,
            limit_cause: None,
            output: serde_json::to_string(&CommandResult {
                exit_code: Some(0),
                stdout: format!(
                    "\n…[{} bytes, 1 lines omitted; {} bytes, 1 lines total]…\n",
                    total.bytes, total.bytes
                ),
                stderr: String::new(),
                stdout_total: total,
                stderr_total: OutputTotal::default(),
                changeset: Default::default(),
                selection: None,
            })
            .unwrap(),
            approval_cause: None,
            cache_update: CacheUpdate::Discarded,
        }
    }

    #[test]
    fn per_run_spill_cap_and_private_file_admission_remain_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let run = RunId::new();
        let access = state.admit_spills(run, &[]).unwrap();
        let bytes = vec![b'!'; MAX_SPILL_RESULT_BYTES];
        for (reference, cancelled) in [
            ("a".repeat(64), true),
            ("invalid".into(), false),
            ("b".repeat(64), false),
        ] {
            let (_entered, release, _finished) =
                crate::fault::FaultPlan::gate_spill_write(state.path().to_owned());
            drop(release);
            let mut gate = crate::fault::spill_write_gate(state.path());
            assert!(access.reserve(bytes.len()).unwrap());
            assert!(
                access
                    .write(&reference, &bytes, &AtomicBool::new(cancelled), &mut gate)
                    .is_err()
            );
            assert!(
                access
                    .current
                    .symlink_metadata(format!("{reference}.txt"))
                    .is_err()
            );
        }
        for index in 0..MAX_SPILL_RUN_BYTES / MAX_SPILL_RESULT_BYTES as u64 {
            let reference = format!("{index:064x}");
            assert!(fixture_write(&access, &reference, &bytes).unwrap());
            if index == 0 {
                assert!(fixture_write(&access, &reference, &bytes).is_err());
            }
        }
        assert!(!fixture_write(&access, &"f".repeat(64), b"overflow").unwrap());
        let reference = "0".repeat(64);
        let path = state
            .path()
            .join("spill")
            .join(run.to_string())
            .join(format!("{reference}.txt"));
        assert_eq!(
            access.read(&reference).unwrap().metadata().unwrap().len(),
            MAX_SPILL_RESULT_BYTES as u64
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(access.read(&reference).is_err());
        std::fs::remove_file(&path).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"OUTSIDE_SPILL_CANARY").unwrap();
        symlink(&outside, &path).unwrap();
        assert!(access.read(&reference).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert!(access.read(&reference).is_err());
        std::fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(access.read(&reference).is_err());
        assert!(access.read(&"e".repeat(64)).is_err());
    }

    #[test]
    fn written_spill_has_no_read_authority_until_its_reference_is_retained() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let access = state.admit_spills(RunId::new(), &[]).unwrap();
        let reference = "a".repeat(64);
        assert!(access.reserve(3).unwrap());
        access
            .write(
                &reference,
                b"one",
                &AtomicBool::new(false),
                &mut crate::fault::SpillWriteGate::default(),
            )
            .unwrap();
        assert!(access.read(&reference).is_err());
        access.retain(&reference).unwrap();
        assert!(access.read(&reference).is_ok());
    }

    #[test]
    fn orphan_cleanup_keeps_committed_spills_and_skips_a_live_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let session = crate::SessionId::new();
        let mut lease = root.lock_session(session).unwrap();
        lease.keep();
        let run = RunId::new();
        let access = root.admit_spills(run, &[]).unwrap();
        let observation = spill_observation(run, b"committed output");
        let kept = observation.spill.as_ref().unwrap().r#ref.clone();
        let orphan = "b".repeat(64);
        assert!(fixture_write(&access, &kept, b"committed output").unwrap());
        assert!(fixture_write(&access, &orphan, b"uncommitted output").unwrap());
        let spill = root.path().join("spill");
        let stray_file = RunId::new();
        let stray_link = RunId::new();
        let unadmitted_run = RunId::new();
        let unadmitted_directory = spill.join(unadmitted_run.to_string());
        std::fs::create_dir(&unadmitted_directory).unwrap();
        std::fs::set_permissions(
            &unadmitted_directory,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let unadmitted_file = unadmitted_directory.join(format!("{orphan}.txt"));
        std::fs::write(&unadmitted_file, b"unadmitted output").unwrap();
        std::fs::write(spill.join(stray_file.to_string()), b"not a directory").unwrap();
        symlink(run.to_string(), spill.join(stray_link.to_string())).unwrap();
        let run_directory = spill.join(run.to_string());
        let nested = run_directory.join(format!("{}.txt", "c".repeat(64)));
        let linked = run_directory.join(format!("{}.txt", "d".repeat(64)));
        let fifo = run_directory.join(format!("{}.txt", "e".repeat(64)));
        std::fs::create_dir(&nested).unwrap();
        symlink(format!("{kept}.txt"), &linked).unwrap();
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
        let connection = rusqlite::Connection::open(root.path().join("events.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE events(session_id TEXT, run_id TEXT, kind TEXT, payload TEXT)",
            )
            .unwrap();
        for run in [run, stray_file, stray_link, unadmitted_run] {
            connection
                .execute(
                    "INSERT INTO events VALUES (?1, ?2, 'RunStarted', '{}')",
                    [session.to_string(), run.to_string()],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO events VALUES (?1, ?2, 'ToolFinished', ?3)",
                [
                    session.to_string(),
                    run.to_string(),
                    Event::ToolFinished {
                        checkpoint: None,
                        run_id: run,
                        agent_run_id: observation.intent.agent_run_id,
                        observation,
                    }
                    .payload()
                    .unwrap(),
                ],
            )
            .unwrap();
        sweep(&root, &connection).unwrap();
        assert!(access.read(&orphan).is_ok());
        drop(lease);
        sweep(&root, &connection).unwrap();
        assert!(access.read(&kept).is_ok());
        assert!(access.read(&orphan).is_err());
        assert_eq!(
            std::fs::read(unadmitted_file).unwrap(),
            b"unadmitted output"
        );
        assert!(!unadmitted_directory.join("writer.lock").exists());
        assert!(nested.is_dir());
        assert!(std::fs::symlink_metadata(linked).unwrap().is_symlink());
        assert!(std::fs::symlink_metadata(fifo).is_ok());
        assert_eq!(
            std::fs::read(spill.join(stray_file.to_string())).unwrap(),
            b"not a directory"
        );
        assert!(
            std::fs::symlink_metadata(spill.join(stray_link.to_string()))
                .unwrap()
                .is_symlink()
        );
    }
}
