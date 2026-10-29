use super::*;

const BUILD_CACHE_SCAN_LIMIT: usize = 16;

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux default and Build cache overlay, teardown and closed replay"
)]
fn cache_overlay_reuses_deletes_recreates_and_bounds_writes() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline(
        "default and Build cache overlay",
        SPILL_JOURNEY_WAIT.saturating_mul(2),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(journey());
}

async fn journey() {
    let fixture = SpillFixture::new();
    let mut outcomes = Vec::new();
    for class in [
        arany::ToolResourceClass::Default,
        arany::ToolResourceClass::Build,
    ] {
        outcomes.push(cache_run(&fixture, class).await);
    }
    fixture.engine.close().await.unwrap();
    for outcome in outcomes {
        let (_, replay) =
            arany::test_support::goals::replayed(&fixture.state, outcome.session_id).await;
        assert_eq!(replay.runs[0], outcome.run);
    }
    let state = StateRoot::open_existing(&fixture.state).unwrap();
    state
        .clear_workspace_build_cache(&fixture.workspace)
        .unwrap();
    let names = std::fs::read_dir(fixture.state.join("build-cache"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert!(
        names
            .iter()
            .all(|name| name == "aggregate.lock" || name == "namespace.lock")
    );
}

async fn cache_run(fixture: &SpillFixture, class: arany::ToolResourceClass) -> arany::RunOutcome {
    let mut scripts = [
        "set -eu; rm -rf \"$CARGO_TARGET_DIR/nested\"; mkdir -p \"$CARGO_TARGET_DIR/nested\"; printf old > \"$CARGO_TARGET_DIR/nested/old\"; printf stable > \"$CARGO_TARGET_DIR/stable\"; mkdir -p /cache/cargo /cache/.npm/_logs; printf transient > /cache/cargo/.package-cache; printf transient > /cache/.npm/_logs/entry",
        "set -eu; test ! -e /cache/cargo/.package-cache; test ! -e /cache/.npm/_logs/entry; test \"$(cat \"$CARGO_TARGET_DIR/nested/old\")\" = old; rm -rf \"$CARGO_TARGET_DIR/nested\"; mkdir \"$CARGO_TARGET_DIR/nested\"; printf new > \"$CARGO_TARGET_DIR/nested/new\"; rm \"$CARGO_TARGET_DIR/stable\"",
        "set -eu; test ! -e \"$CARGO_TARGET_DIR/nested/old\"; test ! -e \"$CARGO_TARGET_DIR/stable\"; test \"$(cat \"$CARGO_TARGET_DIR/nested/new\")\" = new; dd if=/dev/zero of=\"$CARGO_TARGET_DIR/exhausted\" bs=1048576 count=80",
        "set -eu; mkdir -p \"$CARGO_TARGET_DIR/inodes\"; i=0; while { : > \"$CARGO_TARGET_DIR/inodes/empty-$i\"; } 2>/dev/null; do i=$((i + 1)); done; test \"$(/usr/bin/stat -f -c %d /cache)\" -eq 0",
        // The unlimited default published the 80 MiB file; deleting it keeps Build under its set size.
        "set -eu; test -s \"$CARGO_TARGET_DIR/exhausted\"; rm \"$CARGO_TARGET_DIR/exhausted\"; test \"$(cat \"$CARGO_TARGET_DIR/nested/new\")\" = new; printf reused",
    ];
    let switched = format!(
        "set -eu; test \"$(cat \"$CARGO_TARGET_DIR/nested/new\")\" = new; {}",
        scripts[0]
    );
    if class == arany::ToolResourceClass::Build {
        scripts[0] = &switched;
        scripts[4] = "set -eu; test ! -e \"$CARGO_TARGET_DIR/exhausted\"; test \"$(cat \"$CARGO_TARGET_DIR/nested/new\")\" = new; printf reused";
    }
    let calls = scripts
        .iter()
        .map(|script| ToolCall::Command {
            program: "bash".into(),
            args: vec!["-c".into(), (*script).into()],
            cwd: "".into(),
            resource_class: None,
            timeout_ms: None,
        })
        .collect::<Vec<_>>();
    let mut steps = calls
        .iter()
        .cloned()
        .map(crate::tool_outcome)
        .collect::<Vec<_>>();
    steps.push(arany::test_support::goals::finish_with("Done"));
    add_build_plan(&mut steps, class, calls.last().unwrap().clone());
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(steps));
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let inspect = inspect_lifecycle(fixture, &mut inbox, scripts.len());
    let run = run_with(
        &fixture.engine,
        &provider,
        cache_request(&fixture.workspace, class),
        RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        },
    );
    let (outcome, ()) = tokio::join!(run, inspect);
    let outcome = outcome.unwrap();
    assert!(
        upper_handles().is_empty(),
        "publication closes the detached upper"
    );
    assert_observations(&outcome.run, scripts.len(), class);
    assert!(
        std::fs::read_dir(fixture.state.join("build-cache"))
            .unwrap()
            .all(|entry| {
                let path = entry.unwrap().path();
                !matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("arc" | "generation")
                )
            })
    );
    outcome
}

async fn inspect_lifecycle(
    fixture: &SpillFixture,
    inbox: &mut arany::ApprovalInbox,
    command_count: usize,
) {
    for index in 0..command_count {
        let action = inbox.next().await.unwrap();
        if index == 1 {
            let (reported, release_report) =
                arany::FaultPlan::gate_guard_cache_report(action.intent.id);
            let (publishing, release_publication) =
                arany::FaultPlan::gate_guard_cache_publication(action.intent.id);
            action.decide(true);
            reported.await.unwrap();
            let lower = cache_lower(&fixture.state);
            assert_eq!(
                std::fs::read(lower.join("target/nested/old")).unwrap(),
                b"old"
            );
            assert!(upper_handles().is_empty());
            let namespace = supervisor_namespace(&fixture.state);
            let state = StateRoot::open_existing(&fixture.state).unwrap();
            assert!(matches!(
                arany::recover_build_cache(&state, Some(&fixture.workspace)).await,
                Err(arany::StoreError::BuildCacheBusy)
            ));
            assert!(matches!(
                state.clear_workspace_build_cache(&fixture.workspace),
                Err(arany::StoreError::BuildCacheBusy)
            ));
            release_report.send(()).unwrap();
            publishing.await.unwrap();
            assert!(
                !namespace.exists(),
                "supervisor exited before applying the lower"
            );
            assert_eq!(
                upper_handles().len(),
                1,
                "only the detached upper remains pinned"
            );
            assert_eq!(
                std::fs::read(lower.join("target/nested/old")).unwrap(),
                b"old"
            );
            assert!(!lower.join("target/nested/new").exists());
            assert!(matches!(
                state.clear_workspace_build_cache(&fixture.workspace),
                Err(arany::StoreError::BuildCacheBusy)
            ));
            release_publication.send(()).unwrap();
        } else {
            action.decide(true);
        }
    }
}

fn assert_observations(
    run: &arany::RunView,
    command_count: usize,
    class: arany::ToolResourceClass,
) {
    assert_eq!(
        run.status,
        RunStatus::Finished,
        "{}",
        crate::safe_run_diagnostics(run)
    );
    assert_eq!(run.tools.len(), command_count);
    for (index, tool) in run.tools.iter().enumerate() {
        let observation = tool.observation.as_ref().unwrap();
        assert!(observation.attestation.is_some());
        assert_eq!(observation.intent.limits.resource_class, class);
        // The view is the disk headroom, so the 80 MiB write succeeds and only Build's set size
        // rejects it; the inode-exhausted upper exceeds the capture scan bound in both classes.
        assert_eq!(
            observation.cache_update,
            match (index, class) {
                (2, arany::ToolResourceClass::Build) | (3, _) => arany::CacheUpdate::Rejected,
                _ => arany::CacheUpdate::Published,
            },
            "command {index}"
        );
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        let result: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result["changeset"]["files"], json!([]));
    }
}

fn cache_lower(state: &Path) -> PathBuf {
    let trees = std::fs::read_dir(state.join("build-cache"))
        .unwrap()
        .take(BUILD_CACHE_SCAN_LIMIT)
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "tree")
        })
        .collect::<Vec<_>>();
    assert_eq!(trees.len(), 1);
    trees.into_iter().next().unwrap()
}

fn upper_handles() -> Vec<PathBuf> {
    const HANDLE_LIMIT: usize = 1024;
    let handles = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .take(HANDLE_LIMIT + 1)
        .collect::<Vec<_>>();
    assert!(handles.len() <= HANDLE_LIMIT);
    handles
        .into_iter()
        .filter_map(|entry| std::fs::read_link(entry.unwrap().path()).ok())
        .filter(|path| path.ends_with("view/upper"))
        .collect()
}

fn supervisor_namespace(state: &Path) -> PathBuf {
    let markers = std::fs::read_dir(state.join("build-cache"))
        .unwrap()
        .take(BUILD_CACHE_SCAN_LIMIT)
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "unit")
        })
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1);
    let id = uuid::Uuid::parse_str(&std::fs::read_to_string(&markers[0]).unwrap()).unwrap();
    let runtime = format!("/run/user/{}", nix::unistd::geteuid().as_raw());
    let output = Command::new("/usr/bin/systemctl")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus"),
        )
        .args([
            "--user",
            "--no-ask-password",
            "show",
            "--property=MainPID",
            "--value",
            &format!("arany-tool-{id}.service"),
        ])
        .stdin(Stdio::null())
        .bounded_output_for(Duration::from_secs(5), 1024)
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pid: u32 = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(pid, 0);
    let namespace = PathBuf::from(format!("/proc/{pid}/ns/mnt"));
    assert_ne!(
        std::fs::read_link(&namespace).unwrap(),
        std::fs::read_link("/proc/self/ns/mnt").unwrap()
    );
    namespace
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native cache completion loss and durable uncertainty"
)]
fn cache_completion_loss_retains_the_lower_and_stops_without_retry() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline(
        "cache completion loss",
        SPILL_JOURNEY_WAIT.saturating_mul(2),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for class in [
                arany::ToolResourceClass::Default,
                arany::ToolResourceClass::Build,
            ] {
                completion_loss(class).await;
            }
        });
}

async fn completion_loss(class: arany::ToolResourceClass) {
    let fixture = SpillFixture::new();
    let session = fixture
        .engine
        .create_session(fixture.workspace.clone(), None)
        .await
        .unwrap();
    let command = ToolCall::Command {
        program: "bash".into(),
        args: vec![
            "-c".into(),
            "mkdir -p /cache/target; printf discarded > /cache/target/partial".into(),
        ],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
    };
    let mut steps = vec![crate::tool_outcome(command.clone())];
    add_build_plan(&mut steps, class, command.clone());
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(steps));
    let mut request = cache_request(&fixture.workspace, class);
    request.session_id = Some(session);
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let inject = async {
        let action = inbox.next().await.unwrap();
        let entered = arany::FaultPlan::lose_guard_completion(action.intent.id);
        action.decide(true);
        entered.await.unwrap();
    };
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: Some(approvals),
        ..Default::default()
    };
    let (outcome, ()) = tokio::join!(
        run_with(&fixture.engine, &provider, request, controls),
        inject
    );
    assert!(matches!(
        outcome,
        Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
    ));
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1 + usize::from(class == arany::ToolResourceClass::Build)
    );
    assert!(upper_handles().is_empty());
    assert!(!cache_lower(&fixture.state).join("target/partial").exists());
    fixture.engine.close().await.unwrap();
    let (_, view) = arany::test_support::goals::replayed(&fixture.state, session).await;
    let run = &view.runs[0];
    assert_eq!(run.tools.len(), 1);
    let observation = run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.intent.call, command);
    assert_eq!(observation.disposition, ToolDisposition::Uncertain);
    assert_eq!(view.run_stop(run), Some(arany::RunStop::Uncertain));
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1 + usize::from(class == arany::ToolResourceClass::Build)
    );
}

fn cache_request(workspace: &Path, class: arany::ToolResourceClass) -> RunRequest {
    let mut request = proposal_request(workspace);
    if class == arany::ToolResourceClass::Build {
        request.goal = build_goal_request();
    }
    request
}

fn add_build_plan(
    steps: &mut Vec<arany::ProviderOutcome>,
    class: arany::ToolResourceClass,
    success: ToolCall,
) {
    if class == arany::ToolResourceClass::Build {
        steps.insert(
            0,
            arany::test_support::scenario::plan_outcome(arany::GoalPlanContract {
                mission: "Exercise bounded Build cache publication and recovery".into(),
                paths: vec!["src".into()],
                commands: vec!["bash".into()],
                resource_class: class,
                command_timeout_ms: None,
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
                    call: Box::new(success),
                },
                budget: arany::GoalBudget::default(),
            }),
        );
    }
}
