use super::super::format;
use super::{
    HISTORY_FILES, StateRoot, StoreError, check_private_record_file, checkpoints, nonblocking,
    prepare_database_file,
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, MetadataExt, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ARCHIVE_DIR: &str = "archive";
// The plan is durable before any move and stays until the notice is acknowledged.
const PLAN: &str = "pending.json";
const PLAN_TEMP: &str = "pending.tmp";
const MANIFEST: &str = "manifest.json";
const MAX_PLAN_BYTES: usize = 4096;

pub(in crate::store) type Identity = (u64, u64, u64);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    name: String,
    files: Vec<(String, Option<Identity>)>,
}

#[derive(Debug)]
#[must_use = "acknowledge after the notice is delivered"]
pub struct ArchivedHistory {
    archive: Dir,
    name: String,
    path: PathBuf,
}

impl ArchivedHistory {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn acknowledge(self) -> Result<(), StoreError> {
        let target = self.archive.open_dir_nofollow(&self.name)?;
        match rename(&self.archive, PLAN, &target, MANIFEST) {
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
        sync(&target)?;
        sync(&self.archive)
    }
}

impl StateRoot {
    pub fn archive_incompatible_history(&self) -> Result<Option<ArchivedHistory>, StoreError> {
        if self.archive_plan()?.is_none() {
            let _shared = self.lock_history(false)?;
            let _checkpoints = checkpoints::lock(self)?;
            if !format::checkpoint_journal_present(self)? {
                match check_history(self) {
                    Err(StoreError::InvalidHistory) => {}
                    result => return result.map(|()| None),
                }
            }
        }
        let _exclusive = self.lock_history(true)?;
        // An interrupted archive may have moved the database; readmission needs one.
        prepare_database_file(&self.dir, true)?;
        self.readmit()?;
        let _checkpoints = checkpoints::lock(self)?;
        checkpoints::records(self)?;
        let plan = match self.archive_plan()? {
            Some(plan) => plan,
            None => {
                if format::orphan_checkpoint_journal(self)? {
                    return Err(StoreError::InvalidStateDirectory);
                }
                match check_history(self) {
                    Err(StoreError::InvalidHistory) => {}
                    result => return result.map(|()| None),
                }
                self.write_archive_plan()?
            }
        };
        let archive = private_subdirectory(&self.dir, ARCHIVE_DIR)?;
        let target = private_subdirectory(&archive, &plan.name)?;
        sync(&archive)?;
        for (name, identity) in &plan.files {
            let archived = identity_of(&target, name)?;
            match (identity, archived) {
                (None, None) => {}
                (Some(expected), Some(archived)) if *expected == archived => {}
                (Some(expected), None) if identity_of(&self.dir, name)? == Some(*expected) => {
                    rename(&self.dir, name, &target, name)?;
                    sync(&target)?;
                    sync(&self.dir)?;
                }
                _ => return Err(StoreError::ArchiveConflict),
            }
        }
        prepare_database_file(&self.dir, true)?;
        check_history(self)?;
        sync(&self.dir)?;
        Ok(Some(ArchivedHistory {
            path: self.path.join(ARCHIVE_DIR).join(&plan.name),
            archive,
            name: plan.name,
        }))
    }

    fn archive_plan(&self) -> Result<Option<Plan>, StoreError> {
        let archive = match self.dir.open_dir_nofollow(ARCHIVE_DIR) {
            Ok(archive) => archive,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        nonblocking(&mut options);
        let file = match archive.open_with(PLAN, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        check_private_record_file(&file, MAX_PLAN_BYTES)?;
        let mut bytes = Vec::new();
        file.take(MAX_PLAN_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let plan: Plan = serde_json::from_slice(&bytes).map_err(|_| StoreError::ArchiveConflict)?;
        let known = HISTORY_FILES.iter().chain(checkpoints::FILES.iter().rev());
        if !valid_archive_name(&plan.name)
            || !plan
                .files
                .iter()
                .map(|(name, _)| name.as_str())
                .eq(known.copied())
        {
            return Err(StoreError::ArchiveConflict);
        }
        Ok(Some(plan))
    }

    fn write_archive_plan(&self) -> Result<Plan, StoreError> {
        // Sidecars move before their database so a fresh database never inherits a stale hot journal.
        let mut files = Vec::new();
        for name in HISTORY_FILES.iter().chain(checkpoints::FILES.iter().rev()) {
            files.push(((*name).to_owned(), identity_of(&self.dir, name)?));
        }
        let plan = Plan {
            name: archive_name(SystemTime::now()),
            files,
        };
        let archive = private_subdirectory(&self.dir, ARCHIVE_DIR)?;
        sync(&self.dir)?;
        if identity_of(&archive, &plan.name)?.is_some() {
            return Err(StoreError::HistoryChanged);
        }
        publish(
            &archive,
            PLAN_TEMP,
            PLAN,
            &serde_json::to_vec(&plan).map_err(|_| StoreError::ArchiveConflict)?,
        )?;
        Ok(plan)
    }
}

pub(in crate::store) fn publish(
    dir: &Dir,
    temp: &str,
    name: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(true)
        .follow(FollowSymlinks::No);
    cap_std::fs::OpenOptionsExt::mode(&mut options, 0o600);
    nonblocking(&mut options);
    let mut file = dir.open_with(temp, &options)?;
    check_private_record_file(&file, usize::MAX)?;
    file.write_all(bytes)?;
    step()?;
    file.sync_all()?;
    rename(dir, temp, dir, name)?;
    sync(dir)
}

fn check_history(root: &StateRoot) -> Result<(), StoreError> {
    use super::super::journal::open_connection;
    // The writable open still finishes SQLite recovery and enforces the journal pragmas.
    match format::inspect_history(root) {
        format::Inspection::Unsupported => Err(StoreError::InvalidHistory),
        format::Inspection::Current => {
            if format::checkpoint_incompatible(root)? {
                return Err(StoreError::InvalidHistory);
            }
            open_connection(root, false).map(drop)
        }
        format::Inspection::Inconclusive => {
            super::super::replay::validate_history(&open_connection(root, false)?)?;
            if format::checkpoint_incompatible(root)? {
                return Err(StoreError::InvalidHistory);
            }
            Ok(())
        }
    }
}

pub(in crate::store) fn private_subdirectory(parent: &Dir, name: &str) -> Result<Dir, StoreError> {
    let mut builder = DirBuilder::new();
    cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match parent.create_dir_with(name, &builder) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let dir = parent.open_dir_nofollow(name)?;
    crate::private_fs::check_directory(&dir, true)?;
    Ok(dir)
}

pub(in crate::store) fn identity_of(dir: &Dir, name: &str) -> Result<Option<Identity>, StoreError> {
    match dir.symlink_metadata(name) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino(), metadata.len()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(in crate::store) fn rename(
    from_dir: &Dir,
    from: &str,
    to_dir: &Dir,
    to: &str,
) -> Result<(), StoreError> {
    step()?;
    Ok(from_dir.rename(from, to_dir, to)?)
}

pub(in crate::store) fn sync(dir: &Dir) -> Result<(), StoreError> {
    step()?;
    dir.open_with(".", OpenOptions::new().read(true))?
        .sync_all()?;
    Ok(())
}

#[cfg(not(test))]
pub(in crate::store) fn step() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(in crate::store) static FAIL_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(in crate::store) fn step() -> io::Result<()> {
    FAIL_AT.with(|fail| match fail.get() {
        Some(0) => Err(io::Error::other("injected durability failure")),
        remaining => {
            fail.set(remaining.map(|steps| steps - 1));
            Ok(())
        }
    })
}

pub(in crate::store) fn valid_archive_name(name: &str) -> bool {
    name.len() == 28
        && name.starts_with("history-")
        && name.ends_with('Z')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-.".contains(&byte))
}

pub(in crate::store) fn archive_name(now: SystemTime) -> String {
    let elapsed = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs();
    let of_day = seconds % 86_400;
    let (year, month, day) = crate::utc_date(seconds);
    format!(
        "history-{year:04}{month:02}{day:02}T{:02}{:02}{:02}.{:03}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
        elapsed.subsec_millis()
    )
}

#[derive(Debug)]
pub struct ArchivedRecords {
    path: PathBuf,
    files: Vec<&'static str>,
}

impl ArchivedRecords {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn files(&self) -> &[&'static str] {
        &self.files
    }
}

impl StateRoot {
    // Moves each record whose readable version differs from its format into one archive folder.
    pub fn archive_incompatible_records(
        &self,
        formats: &[&super::RecordFormat],
    ) -> Result<Option<ArchivedRecords>, StoreError> {
        // The lock is taken only when something may move, so a concurrent account edit never blocks startup.
        let pending = formats.iter().any(|format| {
            self.record(format.kind)
                .read()
                .ok()
                .flatten()
                .is_some_and(|bytes| format.mismatched(&bytes))
        });
        if !pending {
            return Ok(None);
        }
        self.with_record_upgrade_lock(|| {
            let mut target: Option<(Dir, String)> = None;
            let mut files = Vec::new();
            for format in formats {
                let (name, _, limit, _) = format.kind.spec();
                let Some(file) = self.open_private_record(name, limit)? else {
                    continue;
                };
                let metadata = file.metadata()?;
                let identity = (metadata.dev(), metadata.ino(), metadata.len());
                let mut bytes = Vec::new();
                file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
                if !format.mismatched(&bytes) {
                    continue;
                }
                if target.is_none() {
                    let archive = private_subdirectory(&self.dir, ARCHIVE_DIR)?;
                    sync(&self.dir)?;
                    let folder = unused_records_archive_name(&archive)?;
                    let dir = private_subdirectory(&archive, &folder)?;
                    sync(&archive)?;
                    target = Some((dir, folder));
                }
                let (dir, _) = target.as_ref().ok_or(StoreError::ArchiveConflict)?;
                if identity_of(&self.dir, name)? != Some(identity)
                    || identity_of(dir, name)?.is_some()
                {
                    return Err(StoreError::ArchiveConflict);
                }
                rename(&self.dir, name, dir, name)?;
                sync(dir)?;
                sync(&self.dir)?;
                files.push(name);
            }
            Ok(target.map(|(_, folder)| ArchivedRecords {
                path: self.path.join(ARCHIVE_DIR).join(folder),
                files,
            }))
        })
    }
}

fn records_archive_name(now: SystemTime) -> String {
    archive_name(now).replacen("history-", "records-", 1)
}

// An interrupted attempt within the same millisecond leaves its folder; the retry takes the next suffix.
fn unused_records_archive_name(archive: &Dir) -> Result<String, StoreError> {
    const MAX_SAME_TIME_ARCHIVES: usize = 16;
    let base = records_archive_name(SystemTime::now());
    for attempt in 0..MAX_SAME_TIME_ARCHIVES {
        let folder = match attempt {
            0 => base.clone(),
            _ => format!("{base}-{attempt}"),
        };
        if identity_of(archive, &folder)?.is_none() {
            return Ok(folder);
        }
    }
    Err(StoreError::HistoryChanged)
}

impl StateRoot {
    pub(crate) fn archive_inactive_host_rules(&self) -> Result<bool, StoreError> {
        use super::RecordKind;
        const DIRECTORY: &str = "inactive-host-commands";
        const SOURCE: &str = "unguarded-commands.json";
        const PENDING: &str = "pending.json";
        const MANIFEST: &str = "manifest.json";
        self.with_record_upgrade_lock(|| {
            let bytes = self.record(RecordKind::UnguardedCommands).read()?;
            if bytes.is_none() && identity_of(&self.dir, ARCHIVE_DIR)?.is_none() {
                return Ok(false);
            }
            let archive = private_subdirectory(&self.dir, ARCHIVE_DIR)?;
            if bytes.is_none() && identity_of(&archive, DIRECTORY)?.is_none() {
                return Ok(false);
            }
            sync(&self.dir)?;
            let target = private_subdirectory(&archive, DIRECTORY)?;
            sync(&archive)?;
            if identity_of(&target, PENDING)?.is_some()
                || identity_of(&target, "pending.tmp")?.is_some()
            {
                return Err(StoreError::ReplaceUnconfirmed);
            }
            let archived = identity_of(&target, SOURCE)?;
            if let Some(bytes) = bytes {
                if archived.is_some() || identity_of(&target, MANIFEST)?.is_some() {
                    return Err(StoreError::ArchiveConflict);
                }
                let source = self
                    .open_private_record(SOURCE, RecordKind::UnguardedCommands.limit())?
                    .ok_or(StoreError::ArchiveConflict)?;
                let metadata = source.metadata()?;
                let identity = (metadata.dev(), metadata.ino(), metadata.len());
                let mut admitted = Vec::new();
                source
                    .take(RecordKind::UnguardedCommands.limit() as u64 + 1)
                    .read_to_end(&mut admitted)?;
                if admitted != bytes || identity_of(&self.dir, SOURCE)? != Some(identity) {
                    return Err(StoreError::ArchiveConflict);
                }
                let receipt = serde_json::to_vec(&(identity, crate::validate::digest(&bytes)))
                    .map_err(|_| StoreError::ArchiveConflict)?;
                publish(&target, "pending.tmp", PENDING, &receipt)?;
                // Once the durable marker exists, ambiguous completion refuses instead of retrying.
                rename(&self.dir, SOURCE, &target, SOURCE)?;
                sync(&target)?;
                sync(&self.dir)?;
                publish(&target, "pending.tmp", MANIFEST, &receipt)?;
                step()?;
                // Unsynced cleanup can only restore the blocking marker after a crash.
                target.remove_file(PENDING)?;
                return Ok(true);
            }
            Ok(archived.is_some())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DATABASE_FILE, RecordKind, StateRoot};
    use crate::{Store, StoreError};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    async fn seeded(path: &Path, workspace: &Path) -> Vec<u8> {
        let state = StateRoot::admit(path).unwrap();
        state
            .record(RecordKind::UiPreferences)
            .replace(b"ui")
            .unwrap();
        state
            .record(RecordKind::ModelPreferences)
            .replace(b"models")
            .unwrap();
        crate::create_session(state, workspace.to_path_buf(), None)
            .await
            .unwrap();
        let checkpoints = path.join("checkpoints.sqlite");
        std::fs::write(&checkpoints, b"synthetic-checkpoints").unwrap();
        std::fs::set_permissions(&checkpoints, std::fs::Permissions::from_mode(0o600)).unwrap();
        rusqlite::Connection::open(path.join(DATABASE_FILE))
            .unwrap()
            .execute_batch("UPDATE events SET kind = 'ToolPermissionDecided' WHERE sequence = 1")
            .unwrap();
        std::fs::read(path.join(DATABASE_FILE)).unwrap()
    }

    fn fail_after(steps: Option<usize>) {
        super::FAIL_AT.with(|fail| fail.set(steps));
    }

    fn archive_entries(path: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(path.join("archive"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[tokio::test]
    async fn incompatible_history_is_archived_intact_and_replaced_by_a_fresh_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let history = seeded(&path, &workspace).await;
        let state = StateRoot::open_existing(&path).unwrap();

        let open = Store::open(StateRoot::open_existing(&path).unwrap()).unwrap();
        assert!(matches!(
            state.archive_incompatible_history(),
            Err(StoreError::HistoryBusy)
        ));
        open.close().await.unwrap();
        assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), history);
        assert!(
            !path.join("archive").exists(),
            "busy history stays in place"
        );

        let archived = state.archive_incompatible_history().unwrap().unwrap();
        let archive = archived.path().to_path_buf();
        assert_eq!(archive.parent().unwrap(), path.join("archive"));
        let name = archive.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("history-") && name.ends_with('Z'),
            "{name}"
        );
        assert_eq!(std::fs::read(archive.join(DATABASE_FILE)).unwrap(), history);
        assert_eq!(
            std::fs::read(archive.join("checkpoints.sqlite")).unwrap(),
            b"synthetic-checkpoints"
        );
        for dir in [path.join("archive"), archive.clone()] {
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(!path.join("checkpoints.sqlite").exists());
        assert_eq!(
            state.record(RecordKind::UiPreferences).read().unwrap(),
            Some(b"ui".to_vec())
        );
        assert_eq!(
            state.record(RecordKind::ModelPreferences).read().unwrap(),
            Some(b"models".to_vec())
        );
        assert!(
            crate::list_sessions(StateRoot::open_existing(&path).unwrap(), workspace.clone())
                .await
                .unwrap()
                .is_empty()
        );
        let repeated = state.archive_incompatible_history().unwrap().unwrap();
        assert_eq!(repeated.path(), archive, "an unacknowledged notice repeats");
        drop(repeated);
        archived.acknowledge().unwrap();
        assert!(archive.join("manifest.json").exists());
        assert!(state.archive_incompatible_history().unwrap().is_none());
        assert_eq!(archive_entries(&path), [name]);
    }

    #[tokio::test]
    async fn every_interrupted_step_resumes_to_one_intact_archive() {
        let mut interrupted = 0;
        loop {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let history = seeded(&path, &workspace).await;
            let state = StateRoot::open_existing(&path).unwrap();
            fail_after(Some(interrupted));
            let first = state.archive_incompatible_history();
            fail_after(None);
            let archived = match first {
                Ok(archived) => {
                    archived.unwrap().acknowledge().unwrap();
                    break;
                }
                Err(StoreError::Io(_)) => state.archive_incompatible_history().unwrap().unwrap(),
                Err(error) => panic!("step {interrupted}: {error}"),
            };
            let archive = archived.path().to_path_buf();
            assert_eq!(std::fs::read(archive.join(DATABASE_FILE)).unwrap(), history);
            assert_eq!(
                std::fs::read(archive.join("checkpoints.sqlite")).unwrap(),
                b"synthetic-checkpoints",
                "step {interrupted}"
            );
            fail_after(Some(0));
            assert!(archived.acknowledge().is_err());
            fail_after(None);
            let archived = state.archive_incompatible_history().unwrap().unwrap();
            assert_eq!(archived.path(), archive, "step {interrupted}");
            archived.acknowledge().unwrap();
            assert!(state.archive_incompatible_history().unwrap().is_none());
            assert_eq!(archive_entries(&path).len(), 1, "step {interrupted}");
            assert_eq!(
                state.record(RecordKind::UiPreferences).read().unwrap(),
                Some(b"ui".to_vec())
            );
            interrupted += 1;
        }
        assert!(interrupted >= 9, "every durability step was interrupted");
    }

    #[tokio::test]
    async fn resumed_archive_never_takes_newer_history() {
        for newer_checkpoints in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let history = seeded(&path, &workspace).await;
            fail_after(Some(8));
            let interrupted = StateRoot::open_existing(&path)
                .unwrap()
                .archive_incompatible_history();
            fail_after(None);
            assert!(matches!(interrupted, Err(StoreError::Io(_))));
            assert!(!path.join(DATABASE_FILE).exists() && path.join("checkpoints.sqlite").exists());
            let fresh =
                crate::create_session(StateRoot::admit(&path).unwrap(), workspace.clone(), None)
                    .await
                    .unwrap();
            let sidecar = path.join("checkpoints.sqlite-journal");
            std::fs::write(&sidecar, b"newer-sidecar").unwrap();
            std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600)).unwrap();
            if newer_checkpoints {
                let checkpoints = path.join("checkpoints.sqlite");
                std::fs::remove_file(&checkpoints).unwrap();
                std::fs::write(&checkpoints, b"newer-checkpoints").unwrap();
                std::fs::set_permissions(&checkpoints, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }

            let resumed = StateRoot::open_existing(&path)
                .unwrap()
                .archive_incompatible_history();
            let archive = path.join("archive").join(
                archive_entries(&path)
                    .into_iter()
                    .find(|name| name.starts_with("history-"))
                    .unwrap(),
            );
            assert_eq!(std::fs::read(archive.join(DATABASE_FILE)).unwrap(), history);
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"newer-sidecar");
            if newer_checkpoints {
                assert!(matches!(resumed, Err(StoreError::ArchiveConflict)));
                assert_eq!(
                    std::fs::read(path.join("checkpoints.sqlite")).unwrap(),
                    b"newer-checkpoints"
                );
                assert!(!archive.join("checkpoints.sqlite").exists());
                assert!(
                    path.join("archive/pending.json").exists(),
                    "conflict keeps its plan"
                );
            } else {
                resumed.unwrap().unwrap().acknowledge().unwrap();
                assert_eq!(
                    std::fs::read(archive.join("checkpoints.sqlite")).unwrap(),
                    b"synthetic-checkpoints"
                );
            }
            let sessions =
                crate::list_sessions(StateRoot::open_existing(&path).unwrap(), workspace.clone())
                    .await
                    .unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, fresh);
        }
    }

    #[tokio::test]
    async fn damaged_and_unsafe_history_is_never_archived() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();

        let damaged = temp.path().join("damaged");
        StateRoot::admit(&damaged).unwrap();
        std::fs::write(damaged.join(DATABASE_FILE), b"synthetic-broken-history").unwrap();
        let error = StateRoot::open_existing(&damaged)
            .unwrap()
            .archive_incompatible_history()
            .unwrap_err();
        assert_eq!(error.history_recovery(), crate::HistoryRecovery::Damaged);
        assert_eq!(
            std::fs::read(damaged.join(DATABASE_FILE)).unwrap(),
            b"synthetic-broken-history"
        );
        assert!(!damaged.join("archive").exists());

        let unsafe_state = temp.path().join("unsafe");
        let history = seeded(&unsafe_state, &workspace).await;
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"outside-stays").unwrap();
        symlink(&outside, unsafe_state.join("events.sqlite3-journal")).unwrap();
        let error = StateRoot::open_existing(&unsafe_state)
            .unwrap()
            .archive_incompatible_history()
            .unwrap_err();
        assert_eq!(
            error.history_recovery(),
            crate::HistoryRecovery::Inaccessible
        );
        assert_eq!(
            std::fs::read(unsafe_state.join(DATABASE_FILE)).unwrap(),
            history
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"outside-stays");
        assert!(!unsafe_state.join("archive").exists());
    }
}
