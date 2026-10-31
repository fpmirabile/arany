use super::*;

fn tree(root: &Path, files: &[(&str, &[u8])]) -> Dir {
    for (path, bytes) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    Dir::open_ambient_dir(root, cap_std::ambient_authority()).unwrap()
}

fn capture_at(lower: &Dir, view: &Dir, limit: u64) -> Result<Vec<CacheChange>, ToolError> {
    super::capture(
        lower,
        view,
        &["cargo/registry/cache".into()],
        Instant::now() + crate::tools::selection::TEST_RUNTIME,
        limit,
    )
}

fn names(changes: &[CacheChange]) -> Vec<String> {
    changes
        .iter()
        .map(|change| match change {
            CacheChange::Remove(path) => format!("-{path}"),
            CacheChange::Directory(path) => format!("d{path}"),
            CacheChange::File(path) => format!("f{path}"),
        })
        .collect()
}

#[test]
fn clone_capture_publishes_only_changed_cache_entries_and_refuses_unsafe_output() {
    let temp = tempfile::tempdir().unwrap();
    let lower_path = temp.path().join("lower");
    let files: &[(&str, &[u8])] = &[
        ("target/kept", b"same"),
        ("target/edited", b"old"),
        ("target/gone/child", b"old"),
        ("target/converted/child", b"old"),
    ];
    let lower = tree(&lower_path, files);
    let view_path = temp.path().join("view");
    let copied = std::process::Command::new("/bin/cp")
        .args(["-cRp"])
        .arg(&lower_path)
        .arg(&view_path)
        .status()
        .unwrap();
    assert!(copied.success());
    std::fs::write(view_path.join("target/edited"), b"newer").unwrap();
    std::fs::remove_dir_all(view_path.join("target/gone")).unwrap();
    std::fs::remove_dir_all(view_path.join("target/converted")).unwrap();
    std::fs::write(view_path.join("target/converted"), b"file").unwrap();
    std::fs::create_dir_all(view_path.join("cargo/registry")).unwrap();
    std::os::unix::fs::symlink(temp.path(), view_path.join("cargo/registry/cache")).unwrap();
    std::fs::write(view_path.join("cargo/.package-cache"), b"transient").unwrap();
    let view = Dir::open_ambient_dir(&view_path, cap_std::ambient_authority()).unwrap();
    assert_eq!(
        names(&capture_at(&lower, &view, 1024).unwrap()),
        [
            "-target/gone/child",
            "-target/gone",
            "-target/converted/child",
            "-target/converted",
            "ftarget/converted",
            "ftarget/edited",
        ]
    );
    assert!(matches!(
        capture_at(&lower, &view, 8),
        Err(ToolError::Limit)
    ));
    std::os::unix::fs::symlink("kept", view_path.join("target/link")).unwrap();
    assert!(matches!(
        capture_at(&lower, &view, 1024),
        Err(ToolError::Path)
    ));
    std::fs::remove_file(view_path.join("target/link")).unwrap();
    std::fs::set_permissions(
        view_path.join("target/edited"),
        std::os::unix::fs::PermissionsExt::from_mode(0o4755),
    )
    .unwrap();
    assert!(matches!(
        capture_at(&lower, &view, 1024),
        Err(ToolError::Path)
    ));
}
