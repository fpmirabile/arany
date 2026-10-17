use std::path::{Path, PathBuf};

/// Runs a fixture Git from the given program with a closed environment and fixed identity and dates.
pub fn host_git(program: &str, repository: &Path, home: &Path, args: &[&str]) {
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(repository)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_AUTHOR_NAME", "Arany Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Arany Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .status()
        .expect("host git");
    assert!(status.success(), "git {args:?}");
}

/// Every entry under `root` in sorted order: directories and symlink targets, or file bytes.
pub fn tree_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            let metadata = std::fs::symlink_metadata(&entry).unwrap();
            let relative = entry.strip_prefix(root).unwrap().to_owned();
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&entry).unwrap();
                out.push((relative, target.to_string_lossy().into_owned().into_bytes()));
            } else if metadata.is_dir() {
                out.push((relative, Vec::new()));
                walk(root, &entry, out);
            } else {
                out.push((relative, std::fs::read(&entry).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}
