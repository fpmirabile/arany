use super::StoreError;
use std::{
    ffi::c_void,
    marker::PhantomData,
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
const ACL_READ_DATA: u64 = 1 << 1;
const ACL_WRITE_DATA: u64 = 1 << 2;
const ACL_EXECUTE: u64 = 1 << 3;
const ACL_DELETE: u64 = 1 << 4;
const ACL_APPEND_DATA: u64 = 1 << 5;
const ACL_DELETE_CHILD: u64 = 1 << 6;
const ACL_READ_ATTRIBUTES: u64 = 1 << 7;
const ACL_WRITE_ATTRIBUTES: u64 = 1 << 8;
const ACL_READ_EXTATTRIBUTES: u64 = 1 << 9;
const ACL_WRITE_EXTATTRIBUTES: u64 = 1 << 10;
const ACL_READ_SECURITY: u64 = 1 << 11;
const ACL_WRITE_SECURITY: u64 = 1 << 12;
const ACL_CHANGE_OWNER: u64 = 1 << 13;
const ACL_SYNCHRONIZE: u64 = 1 << 20;
const KNOWN_PERMISSIONS: u64 = ACL_READ_DATA
    | ACL_WRITE_DATA
    | ACL_EXECUTE
    | ACL_DELETE
    | ACL_APPEND_DATA
    | ACL_DELETE_CHILD
    | ACL_READ_ATTRIBUTES
    | ACL_WRITE_ATTRIBUTES
    | ACL_READ_EXTATTRIBUTES
    | ACL_WRITE_EXTATTRIBUTES
    | ACL_READ_SECURITY
    | ACL_WRITE_SECURITY
    | ACL_CHANGE_OWNER
    | ACL_SYNCHRONIZE;
const MUTATION: u64 = ACL_WRITE_DATA
    | ACL_DELETE
    | ACL_APPEND_DATA
    | ACL_DELETE_CHILD
    | ACL_WRITE_ATTRIBUTES
    | ACL_WRITE_EXTATTRIBUTES
    | ACL_WRITE_SECURITY
    | ACL_CHANGE_OWNER;

const ACL_FIRST_ENTRY: i32 = 0;
const ACL_NEXT_ENTRY: i32 = -1;

/// An entry borrowed from its owning `Acl`; the pointer is non-null and valid for `'a`.
struct AclEntry<'a>(*mut c_void, PhantomData<&'a Acl>);

impl Acl {
    /// Reads the extended ACL of `fd`; `Ok(None)` means the object has no extended ACL.
    fn from_fd(fd: BorrowedFd<'_>) -> Result<Option<Self>, std::io::Error> {
        // The descriptor stays borrowed for the call; a non-null result is owned by the caller.
        let raw = unsafe { acl_get_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if raw.is_null() {
            let error = std::io::Error::last_os_error();
            // Darwin reports ENOENT when a valid opened object has no extended ACL.
            return if error.raw_os_error() == Some(nix::libc::ENOENT) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(Self(raw)))
    }

    fn is_valid(&self) -> bool {
        // `self.0` is a live, non-null ACL allocation owned by this wrapper.
        unsafe { acl_valid(self.0) == 0 }
    }

    fn first_entry(&self) -> Result<Option<AclEntry<'_>>, ()> {
        self.entry(ACL_FIRST_ENTRY)
    }

    fn next_entry(&self) -> Result<Option<AclEntry<'_>>, ()> {
        self.entry(ACL_NEXT_ENTRY)
    }

    /// `Ok(None)` marks the end of the entries (EINVAL); any other failure or null entry is `Err`.
    fn entry(&self, which: i32) -> Result<Option<AclEntry<'_>>, ()> {
        let mut entry = ptr::null_mut();
        // `self.0` is live and `entry` is a valid out-pointer; the entry borrows from the ACL.
        if unsafe { acl_get_entry(self.0, which, &mut entry) } != 0 {
            return if std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::EINVAL) {
                Ok(None)
            } else {
                Err(())
            };
        }
        if entry.is_null() {
            return Err(());
        }
        Ok(Some(AclEntry(entry, PhantomData)))
    }
}

impl AclEntry<'_> {
    fn tag_and_permissions(&self) -> Option<(u32, u64)> {
        let mut tag = 0;
        let mut permissions = 0;
        // `self.0` is a non-null entry of a live ACL; both out-pointers are valid locals.
        let ok = unsafe {
            acl_get_tag_type(self.0, &mut tag) == 0
                && acl_get_permset_mask_np(self.0, &mut permissions) == 0
        };
        ok.then_some((tag, permissions))
    }
}

impl Drop for Acl {
    fn drop(&mut self) {
        // This non-null allocation is owned here and freed exactly once with its native allocator.
        unsafe {
            acl_free(self.0);
        }
    }
}

pub(super) fn check(fd: BorrowedFd<'_>, private: bool) -> Result<(), StoreError> {
    let acl = match Acl::from_fd(fd) {
        Ok(Some(acl)) => acl,
        Ok(None) => return Ok(()),
        Err(_) => return Err(StoreError::StateNotPrivate),
    };
    if !acl.is_valid() {
        return Err(StoreError::StateNotPrivate);
    }
    for index in 0..=MAX_ACL_ENTRIES {
        let entry = if index == 0 {
            acl.first_entry()
        } else {
            acl.next_entry()
        };
        let entry = match entry {
            Ok(Some(entry)) if index < MAX_ACL_ENTRIES => entry,
            Ok(None) => return Ok(()),
            _ => return Err(StoreError::StateNotPrivate),
        };
        let Some((tag, permissions)) = entry.tag_and_permissions() else {
            return Err(StoreError::StateNotPrivate);
        };
        if !matches!(tag, ACL_EXTENDED_ALLOW | ACL_EXTENDED_DENY)
            || permissions & !KNOWN_PERMISSIONS != 0
            || tag == ACL_EXTENDED_ALLOW && (private || permissions & MUTATION != 0)
        {
            return Err(StoreError::StateNotPrivate);
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
