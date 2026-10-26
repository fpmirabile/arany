use super::*;
use arany::test_support::scenario::request;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepositoryPhase {
    Cold,
    Warm,
}

pub(super) async fn run(root: &Path) {
    let registry = Path::new(&std::env::var_os("ARANY_TEST_CARGO_REGISTRY").unwrap()).to_owned();
    let destination = root.join("home/.cargo/registry");
    std::fs::create_dir_all(&destination).unwrap();
    for part in ["cache", "src", "index"] {
        let copied = std::process::Command::new("/bin/cp")
            .arg("-cR")
            .arg(registry.join(part))
            .arg(&destination)
            .env_clear()
            .current_dir(root)
            .bounded_output_for(Duration::from_secs(120), 4096)
            .unwrap();
        assert!(copied.status.success(), "registry clone: {copied:?}");
    }
    let workspace =
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let choices = permissions.runtime_profile_choices();
    let tokens = choices
        .iter()
        .filter(|choice| choice.selected)
        .map(|choice| choice.token.clone())
        .collect::<Vec<_>>();
    permissions
        .remember_profiles(true, arany::ApprovalMode::Request, &tokens)
        .unwrap();
    for phase in [RepositoryPhase::Cold, RepositoryPhase::Warm] {
        run_case(root, &workspace, phase).await;
    }
}

async fn run_case(root: &Path, workspace: &Path, phase: RepositoryPhase) {
    let state = root.join("repository-state");
    let script = format!(
        "set -eu; {}printf retained > \"$CARGO_TARGET_DIR/repository-progress\"; cargo test --locked --offline -p arany --lib",
        match phase {
            RepositoryPhase::Cold => "",
            RepositoryPhase::Warm => {
                "test \"$(cat \"$CARGO_TARGET_DIR/repository-progress\")\" = retained; "
            }
        }
    );
    let call = ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), script],
        cwd: String::new(),
    };
    let mut requested = request(workspace, "Run repository library tests");
    requested.include_paths.clear();
    requested.goal.phase = Some(arany::GoalPhase::Implementation);
    let started = std::time::Instant::now();
    let (outcome, _) = run_offline_build_case(
        &state,
        requested,
        arany::WorkspacePermissions::open(workspace).unwrap(),
        call,
        arany::ToolResourceSettings::default().max_command_timeout_ms,
    )
    .await;
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(
        SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap()
            .runs
            .as_slice(),
        std::slice::from_ref(&outcome.run)
    );
    report(&outcome, phase, started.elapsed());
}

fn report(outcome: &arany::RunOutcome, phase: RepositoryPhase, elapsed: Duration) {
    let phase_name = match phase {
        RepositoryPhase::Cold => "cold",
        RepositoryPhase::Warm => "warm",
    };
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
    let receipt = observation
        .attestation
        .as_ref()
        .and_then(arany::Attestation::guard)
        .expect("repository Guard receipt");
    assert_eq!(receipt.limits, observation.intent.limits);
    assert_eq!(observation.cache_update, arany::CacheUpdate::Published);
    assert_eq!(result["changeset"]["files"], json!([]));
    if observation.disposition == ToolDisposition::Limit {
        assert_eq!(observation.limit_cause, Some(arany::LimitCause::Timeout));
        assert_eq!(
            phase,
            RepositoryPhase::Cold,
            "warm repository tests did not complete"
        );
        println!(
            "repository {phase_name}: timeout; elapsed={elapsed:?}; jobs={}",
            receipt.limits.build_jobs
        );
    } else {
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{result}"
        );
        let stdout = result["stdout"].as_str().unwrap();
        let summary = stdout
            .lines()
            .find(|line| line.starts_with("test result: ok."))
            .expect("actual libtest counts");
        let fields = summary.split_whitespace().collect::<Vec<_>>();
        assert!(fields[3].parse::<usize>().unwrap() > 0);
        assert_eq!(fields[5], "0");
        assert!(fields[7].parse::<usize>().unwrap() > 0);
        println!(
            "repository {phase_name}: {summary}; elapsed={elapsed:?}; jobs={}",
            receipt.limits.build_jobs
        );
    }
}
