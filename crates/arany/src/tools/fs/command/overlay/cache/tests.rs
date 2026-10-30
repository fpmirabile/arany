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
            Instant::now() + crate::tools::selection::TEST_RUNTIME,
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
            Instant::now() + crate::tools::selection::TEST_RUNTIME,
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

#[test]
fn cache_capture_streams_an_upper_past_the_former_scan_bound() {
    let temp = tempfile::tempdir().unwrap();
    let lower_path = temp.path().join("lower");
    let files = temp.path().join("upper/target/inodes");
    std::fs::create_dir_all(&lower_path).unwrap();
    std::fs::create_dir_all(&files).unwrap();
    let lower = Dir::open_ambient_dir(&lower_path, cap_std::ambient_authority()).unwrap();
    let upper =
        Dir::open_ambient_dir(temp.path().join("upper"), cap_std::ambient_authority()).unwrap();
    let capture = || {
        super::capture(
            &lower,
            &upper,
            &[],
            Instant::now() + crate::tools::selection::TEST_RUNTIME,
            DEFAULT_COMMAND_PROJECT_BYTES,
        )
    };
    let mut created = 0;
    for count in [COMMAND_SCAN_ENTRIES / 4, COMMAND_SCAN_ENTRIES + 1] {
        for index in created..count {
            File::create(files.join(index.to_string())).unwrap();
        }
        created = count;
        assert_eq!(capture().unwrap().len(), count + 2, "{count} entries");
    }
}
