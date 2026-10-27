use super::*;

#[test]
fn cache_capture_recreates_subtrees_preserves_host_exclusions_and_rejects_unsafe_output() {
    let temp = tempfile::tempdir().unwrap();
    let lower_path = temp.path().join("lower");
    let upper_path = temp.path().join("upper");
    for path in [&lower_path, &upper_path] {
        std::fs::create_dir_all(path.join("target/nested")).unwrap();
        std::fs::create_dir_all(path.join("cargo/registry/cache")).unwrap();
    }
    std::fs::write(lower_path.join("target/nested/old"), b"old").unwrap();
    std::fs::write(upper_path.join("target/nested/new"), b"new").unwrap();
    std::fs::write(lower_path.join("cargo/registry/cache/private"), b"retained").unwrap();
    std::fs::write(upper_path.join("cargo/registry/cache/host"), b"host").unwrap();
    std::fs::write(upper_path.join("cargo/.package-cache"), b"transient").unwrap();
    std::fs::create_dir_all(upper_path.join(".npm/_logs")).unwrap();
    std::fs::write(upper_path.join(".npm/_logs/entry"), b"transient").unwrap();
    let nested = File::open(upper_path.join("target/nested")).unwrap();
    rustix::fs::fsetxattr(
        &nested,
        "user.overlay.opaque",
        b"y",
        rustix::fs::XattrFlags::CREATE,
    )
    .unwrap();
    let lower = Dir::open_ambient_dir(&lower_path, cap_std::ambient_authority()).unwrap();
    let upper = Dir::open_ambient_dir(&upper_path, cap_std::ambient_authority()).unwrap();
    let exclusions = ["cargo/registry/cache".into()];
    let capture = || {
        super::capture(
            &lower,
            &upper,
            &exclusions,
            Instant::now() + CAPTURE_TIME,
            DEFAULT_COMMAND_PROJECT_BYTES,
        )
    };
    let changes = capture().unwrap();
    assert!(
        changes.iter().any(
            |change| matches!(change, CacheChange::Remove(path) if path == "target/nested/old")
        )
    );
    assert!(
        changes
            .iter()
            .any(|change| matches!(change, CacheChange::File(path) if path == "target/nested/new"))
    );
    assert!(!changes.iter().any(|change| match change {
        CacheChange::Remove(path) | CacheChange::File(path) | CacheChange::Directory(path) =>
            path.starts_with("cargo/") || path.starts_with(".npm/"),
    }));
    assert!(matches!(
        super::capture(
            &lower,
            &upper,
            &exclusions,
            Instant::now() + CAPTURE_TIME,
            1
        ),
        Err(ToolError::Limit)
    ));
    rustix::fs::fsetxattr(
        &nested,
        "user.overlay.opaque",
        b"x",
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    assert!(capture().is_err());
    rustix::fs::fremovexattr(&nested, "user.overlay.opaque").unwrap();
    std::os::unix::fs::symlink("/usr", upper_path.join(".npm/_logs/unsafe")).unwrap();
    assert!(capture().is_err());
}
