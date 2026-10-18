use arany::test_support::worker_teams::run_case;

#[tokio::test]
async fn worker_teams_completion_failure_caps_leases_background_waves_and_cancellation() {
    let executable = std::env::var_os("ARANY_TEST_EXE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("arany")
        });
    assert!(
        executable.is_file(),
        "build arany before the Engine worker scenarios"
    );
    for case in [
        "order",
        "fallback",
        "failure",
        "mixed",
        "c1_failure",
        "cap",
        "hard_cap",
        "tokens",
        "leased_edit",
        "background",
        "command",
        "waves",
        "reassign",
        "cancel_foreground",
        "cancel_background",
        "cancel_mixed",
    ] {
        tokio::time::timeout(
            std::time::Duration::from_secs(180),
            run_case(&executable, case),
        )
        .await
        .expect(case);
    }
}
