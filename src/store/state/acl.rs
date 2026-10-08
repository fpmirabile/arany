use super::StoreError;
use std::{
    ffi::c_void,
    os::fd::{AsRawFd, BorrowedFd},
    ptr,
};

unsafe extern "C" {
    fn acl_get_fd_np(fd: i32, kind: u32) -> *mut c_void;
    fn acl_free(acl: *mut c_void) -> i32;
    fn acl_valid(acl: *mut c_void) -> i32;
    fn acl_get_entry(acl: *mut c_void, index: i32, entry: *mut *mut c_void) -> i32;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut u32) -> i32;
    fn acl_get_permset_mask_np(entry: *mut c_void, permissions: *mut u64) -> i32;
}

struct Acl(*mut c_void);

const ACL_TYPE_EXTENDED: u32 = 0x100;
const ACL_EXTENDED_ALLOW: u32 = 1;
const ACL_EXTENDED_DENY: u32 = 2;
const MAX_ACL_ENTRIES: usize = 128;
const KNOWN_PERMISSIONS: u64 = 0x3ffe | (1 << 20);

impl Drop for Acl {
    fn drop(&mut self) {
        // This non-null allocation is owned here and freed exactly once with its native allocator.
        unsafe {
            acl_free(self.0);
        }
    }
}

pub(super) fn check(fd: BorrowedFd<'_>, private: bool) -> Result<(), StoreError> {
    // The descriptor remains borrowed; entries and out-pointers live within the owned ACL allocation.
    unsafe {
        let acl = acl_get_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED);
        if acl.is_null() {
            // Darwin reports ENOENT when a valid opened object has no extended ACL.
            if std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::ENOENT) {
                return Ok(());
            }
            return Err(StoreError::StateNotPrivate);
        }
        let acl = Acl(acl);
        if acl_valid(acl.0) != 0 {
            return Err(StoreError::StateNotPrivate);
        }
        const MUTATION: u64 = (1 << 2)
            | (1 << 4)
            | (1 << 5)
            | (1 << 6)
            | (1 << 8)
            | (1 << 10)
            | (1 << 12)
            | (1 << 13);
        for index in 0..=MAX_ACL_ENTRIES {
            let mut entry = ptr::null_mut();
            if acl_get_entry(acl.0, if index == 0 { 0 } else { -1 }, &mut entry) != 0 {
                return if std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::EINVAL)
                {
                    Ok(())
                } else {
                    Err(StoreError::StateNotPrivate)
                };
            }
            if index == MAX_ACL_ENTRIES || entry.is_null() {
                return Err(StoreError::StateNotPrivate);
            }
            let mut tag = 0;
            let mut permissions = 0;
            if acl_get_tag_type(entry, &mut tag) != 0
                || acl_get_permset_mask_np(entry, &mut permissions) != 0
                || !matches!(tag, ACL_EXTENDED_ALLOW | ACL_EXTENDED_DENY)
                || permissions & !KNOWN_PERMISSIONS != 0
                || tag == ACL_EXTENDED_ALLOW && (private || permissions & MUTATION != 0)
            {
                return Err(StoreError::StateNotPrivate);
            }
        }
    }
    Err(StoreError::StateNotPrivate)
}

#[cfg(test)]
mod tests {
    use crate::test_process::BoundedOutput;
    use crate::{SessionId, StateRoot, StoreError};
    use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

    fn chmod(path: &Path, args: &[&str]) {
        let output = Command::new("/bin/chmod")
            .args(args)
            .arg(path)
            .env_clear()
            .bounded_output_for(std::time::Duration::from_secs(5), 1024)
            .unwrap();
        assert!(output.status.success(), "native fixture ACL update failed");
    }

    #[test]
    fn private_state_rejects_native_acl_grants_through_opened_handles() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let state = StateRoot::admit(&path).unwrap();
        state.replace_saved_account_record(b"synthetic").unwrap();
        let id = SessionId::new();
        let mut lock = state.lock_session(id).unwrap();
        lock.keep();
        drop(lock);
        state
            .with_account_replacement_lock(&workspace, || ())
            .unwrap();
        for (name, grant) in [
            ("account-credentials.json".to_owned(), "everyone allow read"),
            (
                "account-credentials.pending".to_owned(),
                "everyone allow write",
            ),
            ("events.sqlite3".to_owned(), "everyone allow read"),
            (
                "account-credentials.lock".to_owned(),
                "everyone allow write",
            ),
            (format!("session-{id}.lock"), "everyone allow write"),
        ] {
            let file = path.join(&name);
            if !file.exists() {
                std::fs::write(&file, b"").unwrap();
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            chmod(&file, &["+a", grant]);
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o077,
                0,
                "fixture must defeat mode-only admission"
            );
            let rejected = match name.as_str() {
                "account-credentials.json" => {
                    matches!(
                        state.read_saved_account_record(),
                        Err(StoreError::StateNotPrivate)
                    ) && matches!(
                        state.saved_account_record_present(),
                        Err(StoreError::StateNotPrivate)
                    )
                }
                "account-credentials.pending" => matches!(
                    state.replace_saved_account_record(b"replacement"),
                    Err(StoreError::StateNotPrivate)
                ),
                "events.sqlite3" => matches!(
                    StateRoot::open_existing(&path),
                    Err(StoreError::StateNotPrivate)
                ),
                "account-credentials.lock" => matches!(
                    state.with_account_replacement_lock(&workspace, || ()),
                    Err(StoreError::StateNotPrivate)
                ),
                _ => matches!(state.lock_session(id), Err(StoreError::StateNotPrivate)),
            };
            assert!(rejected, "native ACL must reject {name}");
            chmod(&file, &["-N"]);
        }
        chmod(&path, &["+a", "everyone allow read,search"]);
        assert!(matches!(
            StateRoot::open_existing(&path),
            Err(StoreError::StateNotPrivate)
        ));
        chmod(&path, &["-N"]);
        chmod(
            temp.path(),
            &[
                "+a",
                "everyone allow add_file,add_subdirectory,delete_child",
            ],
        );
        assert!(matches!(
            StateRoot::open_existing(&path),
            Err(StoreError::StateNotPrivate)
        ));
        chmod(temp.path(), &["-N"]);
        chmod(temp.path(), &["+a", "everyone deny delete"]);
        assert!(
            StateRoot::open_existing(&path).is_ok(),
            "restrictive ancestor ACL is valid"
        );
        chmod(temp.path(), &["-N"]);
        assert_eq!(
            state.read_saved_account_record().unwrap(),
            Some(b"synthetic".to_vec())
        );
    }
}
