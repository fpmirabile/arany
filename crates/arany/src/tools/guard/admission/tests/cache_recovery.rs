use super::*;
use crate::store::{BuildCacheIdentity, BuildCacheLease, StateRoot};

pub(super) fn owned_cache(path: &Path, id: uuid::Uuid) -> BuildCacheLease {
    use std::os::unix::fs::MetadataExt;
    let state = StateRoot::admit(&path.join("state")).unwrap();
    let workspace = std::fs::canonicalize(path.join("source")).unwrap();
    let metadata = std::fs::metadata(&workspace).unwrap();
    let identity = BuildCacheIdentity::new(&workspace, metadata.dev(), metadata.ino()).unwrap();
    let lease = state.begin_build_cache(identity).unwrap();
    lease.guard_started(id).unwrap();
    lease
}

#[test]
#[cfg_attr(
    any(arany_guard, not(arany_ci)),
    ignore = "native abandoned cache controller recovery"
)]
fn cache_recovery_stops_the_abandoned_unit_before_clear_and_releases_storage() {
    if crate::test_process::isolate_test() {
        return;
    }
    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = crate::test_support::engine::deadline(
        "abandoned cache controller",
        Duration::from_secs(60),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(journey());
}

async fn journey() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let listener = UnixListener::bind(fixture.path().join("control")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let id = uuid::Uuid::now_v7();
    let (mut controller, _control) =
        start_seed_controller(fixture.path(), &listener, "cache", id, false).await;
    let workspace = std::fs::canonicalize(fixture.path().join("source")).unwrap();
    let state_path = fixture.path().join("state");
    let state = StateRoot::open_existing(&state_path).unwrap();
    let group = super::super::super::linux::seed_unit_group(id).await;
    assert!(matches!(
        super::super::super::recover_build_cache(&state, Some(&workspace)).await,
        Err(crate::StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_workspace_build_cache(&workspace),
        Err(crate::StoreError::BuildCacheBusy)
    ));
    controller.child.kill().unwrap();
    let observed =
        crate::test_process::capture(&mut controller.child, Duration::from_secs(5), 8192);
    assert_eq!(observed.status.signal(), Some(9));
    let state = StateRoot::open_existing(&state_path).unwrap();
    assert!(matches!(
        state.clear_workspace_build_cache(&workspace),
        Err(crate::StoreError::BuildCacheBusy)
    ));
    assert!(
        group
            .read_to_string("cgroup.events")
            .unwrap()
            .lines()
            .any(|line| line == "populated 1")
    );
    super::super::super::recover_build_cache(&state, Some(&workspace))
        .await
        .unwrap();
    match group.read_to_string("cgroup.events") {
        Ok(events) => assert!(events.lines().any(|line| line == "populated 0")),
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
    }
    assert!(!Path::new("/tmp").join(format!("arany-tools-{id}")).exists());
    let registry = StateRoot::account_path().unwrap().join("guard");
    for suffix in ["unit", "seed", "seed.next"] {
        assert!(!registry.join(format!("{id}.{suffix}")).exists());
    }
    state.clear_workspace_build_cache(&workspace).unwrap();
    super::super::super::recover_build_cache(&state, Some(&workspace))
        .await
        .unwrap();
    let names = std::fs::read_dir(state_path.join("build-cache"))
        .unwrap()
        .take(16)
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert!(
        names
            .iter()
            .all(|name| name == "namespace.lock" || name == "aggregate.lock")
    );
}
