use super::{HISTORY_FILES, StateRoot, StoreError, checkpoints, prepare_database_file};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const ARCHIVE_DIR: &str = "archive";
// One staging name: a crash mid-move leaves it behind, and the next admission completes the move.
const STAGING: &str = "history.partial";

impl StateRoot {
    pub fn archive_incompatible_history(&self) -> Result<Option<PathBuf>, StoreError> {
        if !self.staged_archive()? {
            let _shared = self.lock_history(false)?;
            match check_history(self) {
                Err(StoreError::InvalidHistory) => {}
                result => return result.map(|()| None),
            }
        }
        let _exclusive = self.lock_history(true)?;
        self.readmit()?;
        if !self.staged_archive()? {
            match check_history(self) {
                Err(StoreError::InvalidHistory) => {}
                result => return result.map(|()| None),
            }
        }
        let archive = private_subdirectory(&self.dir, ARCHIVE_DIR)?;
        let staging = private_subdirectory(&archive, STAGING)?;
        let _checkpoints = checkpoints::lock(self)?;
        checkpoints::records(self)?;
        // Sidecars move before their database so a fresh database never inherits a stale hot journal.
        for name in HISTORY_FILES.iter().chain(checkpoints::FILES.iter().rev()) {
            if exists(&staging, name)? || !exists(&self.dir, name)? {
                continue;
            }
            self.dir.rename(name, &staging, name)?;
        }
        sync(&staging)?;
        let name = archive_name(SystemTime::now());
        if exists(&archive, &name)? {
            return Err(StoreError::HistoryChanged);
        }
        archive.rename(STAGING, &archive, &name)?;
        sync(&archive)?;
        sync(&self.dir)?;
        prepare_database_file(&self.dir, true)?;
        check_history(self)?;
        sync(&self.dir)?;
        Ok(Some(self.path.join(ARCHIVE_DIR).join(name)))
    }

    fn staged_archive(&self) -> Result<bool, StoreError> {
        match self.dir.open_dir_nofollow(ARCHIVE_DIR) {
            Ok(archive) => exists(&archive, STAGING),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

fn check_history(root: &StateRoot) -> Result<(), StoreError> {
    let connection = super::super::journal::open_connection(root, false)?;
    super::super::replay::validate_history(&connection)
}

fn private_subdirectory(parent: &Dir, name: &str) -> Result<Dir, StoreError> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
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

fn exists(dir: &Dir, name: &str) -> Result<bool, StoreError> {
    match dir.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sync(dir: &Dir) -> Result<(), StoreError> {
    dir.open_with(".", OpenOptions::new().read(true))?
        .sync_all()?;
    Ok(())
}

fn archive_name(now: SystemTime) -> String {
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let seconds = millis / 1000;
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm, proleptic Gregorian).
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u128::from(month <= 2);
    format!(
        "history-{year:04}{month:02}{day:02}T{:02}{:02}{:02}.{:03}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
        millis % 1000
    )
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
            .execute_batch(
                "UPDATE events SET event_version = event_version - 1 WHERE kind = 'SessionStarted'",
            )
            .unwrap();
        std::fs::read(path.join(DATABASE_FILE)).unwrap()
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
        assert_eq!(archived.parent().unwrap(), path.join("archive"));
        let name = archived.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("history-") && name.ends_with('Z'),
            "{name}"
        );
        assert_eq!(
            std::fs::read(archived.join(DATABASE_FILE)).unwrap(),
            history
        );
        assert_eq!(
            std::fs::read(archived.join("checkpoints.sqlite")).unwrap(),
            b"synthetic-checkpoints"
        );
        for dir in [path.join("archive"), archived.clone()] {
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
        assert_eq!(state.archive_incompatible_history().unwrap(), None);
        assert_eq!(std::fs::read_dir(path.join("archive")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn an_interrupted_archive_completes_without_taking_newer_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let history = seeded(&path, &workspace).await;
        let staging = path.join("archive/history.partial");
        std::fs::create_dir_all(&staging).unwrap();
        for dir in [path.join("archive"), staging.clone()] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::rename(path.join(DATABASE_FILE), staging.join(DATABASE_FILE)).unwrap();
        let fresh =
            crate::create_session(StateRoot::admit(&path).unwrap(), workspace.clone(), None)
                .await
                .unwrap();

        let archived = StateRoot::open_existing(&path)
            .unwrap()
            .archive_incompatible_history()
            .unwrap()
            .unwrap();
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read(archived.join(DATABASE_FILE)).unwrap(),
            history
        );
        assert_eq!(
            std::fs::read(archived.join("checkpoints.sqlite")).unwrap(),
            b"synthetic-checkpoints"
        );
        let sessions =
            crate::list_sessions(StateRoot::open_existing(&path).unwrap(), workspace.clone())
                .await
                .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, fresh);
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
