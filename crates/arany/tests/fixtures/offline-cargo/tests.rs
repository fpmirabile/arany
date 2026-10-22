#[test]
fn dependency_and_native_build_execute() {
    assert_eq!(arany_fixture_dependency::answer(), 42);
    assert!(std::env::var_os("ARANY_FIXTURE_FAIL").is_none());
}

// Skipped in the Guard: local test IPC capability is missing.
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
#[test]
fn local_test_ipc() {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
}
