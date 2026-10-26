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
        check()?;
        if !crate::validate::valid_digest(reference) || bytes.len() > MAX_SPILL_RESULT_BYTES {
            return Err(StoreError::InvalidStateDirectory);
        }
        let _lease = writer_lock(&self.current)?;
        check()?;
        let name = format!("{reference}.txt");
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .follow(FollowSymlinks::No);
        nonblocking(&mut options);
        let mut file = self.current.open_with(&name, &options)?;
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
            let _ = self.current.remove_file(&name);
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

const SWEEP_TIME: std::time::Duration = std::time::Duration::from_secs(30);

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
    let deadline = std::time::Instant::now() + SWEEP_TIME;
    let mut remaining = crate::store::MAX_LIST_SCAN_EVENTS;
    let mut owner = connection
        .prepare("SELECT session_id FROM events WHERE run_id=?1 AND kind='RunStarted' LIMIT 1")?;
    let mut references = connection.prepare("SELECT json_extract(payload, '$.spill.ref') FROM events WHERE run_id=?1 AND kind='ToolFinished'")?;
    for entry in spill.entries()?.take(crate::store::MAX_LIST_SCAN_EVENTS) {
        if std::time::Instant::now() >= deadline {
            return Ok(());
        }
        let name = entry?.file_name();
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
        let directory = spill.open_dir_nofollow(&name)?;
        checked_child(&spill, &directory)?;
        let Ok(_writer) = writer_lock(&directory) else {
            continue;
        };
        let mut kept = BTreeSet::new();
        for reference in
            references.query_map([run.to_string()], |row| row.get::<_, Option<String>>(0))?
        {
            if remaining == 0 || std::time::Instant::now() >= deadline {
                return Ok(());
            }
            remaining -= 1;
            if let Some(reference) = reference?
                && crate::validate::valid_digest(&reference)
            {
                kept.insert(reference);
            }
        }
        for entry in directory.entries()? {
            if remaining == 0 || std::time::Instant::now() >= deadline {
                return Ok(());
            }
            remaining -= 1;
            let name = entry?.file_name();
            let Some(reference) = name.to_str().and_then(|name| name.strip_suffix(".txt")) else {
                continue;
            };
            if crate::validate::valid_digest(reference) && !kept.contains(reference) {
                directory.remove_file(name)?;
            }
        }
    }
    Ok(())
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
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn per_run_spill_cap_and_private_file_admission_remain_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let run = RunId::new();
        let access = state.admit_spills(run, &[]).unwrap();
        let bytes = vec![b'!'; MAX_SPILL_RESULT_BYTES];
        for index in 0..MAX_SPILL_RUN_BYTES / MAX_SPILL_RESULT_BYTES as u64 {
            assert!(fixture_write(&access, &format!("{index:064x}"), &bytes).unwrap());
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
}

#[cfg(test)]
#[test]
fn orphan_cleanup_keeps_committed_spills_and_skips_a_live_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).unwrap();
    let session = crate::SessionId::new();
    let mut lease = root.lock_session(session).unwrap();
    lease.keep();
    let run = RunId::new();
    let access = root.admit_spills(run, &[]).unwrap();
    let kept = "a".repeat(64);
    let orphan = "b".repeat(64);
    assert!(fixture_write(&access, &kept, b"committed output").unwrap());
    assert!(fixture_write(&access, &orphan, b"uncommitted output").unwrap());
    let connection = rusqlite::Connection::open(root.path().join("events.sqlite3")).unwrap();
    connection
        .execute_batch("CREATE TABLE events(session_id TEXT, run_id TEXT, kind TEXT, payload TEXT)")
        .unwrap();
    connection
        .execute(
            "INSERT INTO events VALUES (?1, ?2, 'RunStarted', '{}')",
            [session.to_string(), run.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events VALUES (?1, ?2, 'ToolFinished', ?3)",
            [
                session.to_string(),
                run.to_string(),
                serde_json::json!({"spill":{"ref":kept}}).to_string(),
            ],
        )
        .unwrap();
    sweep(&root, &connection).unwrap();
    assert!(access.read(&orphan).is_ok());
    drop(lease);
    sweep(&root, &connection).unwrap();
    assert!(access.read(&kept).is_ok());
    assert!(access.read(&orphan).is_err());
}

#[cfg(test)]
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
