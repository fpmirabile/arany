use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd};

const CLONE_NOFOLLOW: u32 = 1;
const CLONE_NOOWNERCOPY: u32 = 2;

// APFS copy-on-write clone of an open file or directory tree into `parent`; another volume reports EXDEV.
pub(crate) fn clone_at(source: &impl AsFd, parent: &impl AsFd, name: &str) -> io::Result<()> {
    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // Both descriptors are borrowed for the call and the name is an owned NUL-terminated C string.
    let result = unsafe {
        nix::libc::fclonefileat(
            source.as_fd().as_raw_fd(),
            parent.as_fd().as_raw_fd(),
            name.as_ptr(),
            CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
