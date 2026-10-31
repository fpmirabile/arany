use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) const SYSTEM_DIRECTORIES: [&str; 6] =
    ["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"];
pub(super) const SYSTEM_CONFIGURATION: [&str; 5] = [
    "/etc/alternatives",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/etc/ld.so.cache",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SystemEntry {
    Bind(PathBuf),
    Link(PathBuf, PathBuf),
}

impl SystemEntry {
    pub(super) fn arguments(&self, command: &mut tokio::process::Command) {
        match self {
            Self::Bind(path) => command.arg("--ro-bind").arg(path).arg(path),
            Self::Link(target, path) => command.arg("--symlink").arg(target).arg(path),
        };
    }
}

pub(in crate::tools::guard) fn targets() -> impl Iterator<Item = &'static str> {
    SYSTEM_DIRECTORIES.into_iter().chain(SYSTEM_CONFIGURATION)
}

// Top-level links such as usrmerge's /lib -> usr/lib are copied verbatim; only root-owned entries
// that no group or other user can write are bound, and anything else is left out.
pub(super) fn entries(root: &Path, owner: u32) -> Vec<SystemEntry> {
    let mut entries = Vec::new();
    for path in targets() {
        let host = root.join(path.trim_start_matches('/'));
        let Ok(metadata) = std::fs::symlink_metadata(&host) else {
            continue;
        };
        if metadata.uid() != owner {
            continue;
        }
        if metadata.is_symlink() {
            if SYSTEM_DIRECTORIES.contains(&path)
                && let Ok(target) = std::fs::read_link(&host)
            {
                entries.push(SystemEntry::Link(target, path.into()));
            }
        } else if metadata.mode() & 0o022 == 0 && (metadata.is_dir() || metadata.is_file()) {
            entries.push(SystemEntry::Bind(path.into()));
        }
    }
    entries
}

// The owner of `/`: root on a host, the overflow ID inside an unprivileged user namespace.
pub(super) fn root_owner() -> u32 {
    std::fs::metadata("/").map_or(0, |metadata| metadata.uid())
}

// Inside the namespace every listed path that is not a link came from a read-only bind.
pub(super) fn mounted() -> Vec<PathBuf> {
    targets()
        .map(PathBuf::from)
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_symlink()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_entries_bind_owned_directories_and_copy_top_level_links() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let owner = std::fs::metadata(root.path()).unwrap().uid();
        let path = |name: &str| root.path().join(name);
        std::fs::create_dir(path("bin")).unwrap();
        std::os::unix::fs::symlink("usr/lib", path("lib")).unwrap();
        std::fs::create_dir(path("sbin")).unwrap();
        std::fs::set_permissions(path("sbin"), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::create_dir_all(path("etc/ssl")).unwrap();
        std::fs::write(path("etc/ld.so.cache"), b"").unwrap();
        std::fs::set_permissions(
            path("etc/ld.so.cache"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::os::unix::fs::symlink("/elsewhere", path("etc/pki")).unwrap();

        assert_eq!(
            entries(root.path(), owner),
            [
                SystemEntry::Bind("/bin".into()),
                SystemEntry::Link("usr/lib".into(), "/lib".into()),
                SystemEntry::Bind("/etc/ssl".into()),
                SystemEntry::Bind("/etc/ld.so.cache".into()),
            ]
        );
        assert!(entries(root.path(), owner + 1).is_empty());
    }
}
