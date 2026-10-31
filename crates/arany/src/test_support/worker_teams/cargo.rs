use super::*;

pub(super) const CASE: &str = "cargo";
const SOURCE: &str = "src/lib.rs";
const EDITED: &str = concat!(
    include_str!("../../../tests/fixtures/offline-cargo/tests.rs"),
    "\n#[test]\nfn editor_edit_builds() {}\n"
);
const JOURNEY_DEADLINE: Duration = Duration::from_secs(600);
const COLD: &str = r#"set -eu
test "$CARGO_NET_OFFLINE" = true
command -v cargo
cargo build --locked
cargo test --locked --lib
"#;
// Unchanged sources must stay fresh against the cache the first command published.
const WARM: &str = "cargo test --locked --lib --verbose";

// The Workspace holds `offline_cargo_fixture`'s crate; the caller has remembered any profiles beyond the defaults.
pub async fn cargo_editor_journey(guard: &Path, directory: tempfile::TempDir) {
    let _deadline = super::super::engine::deadline(CASE, JOURNEY_DEADLINE);
    super::super::isolate_account_root();
    let rust =
        super::super::scenario::select_offline_cargo_profiles(&directory.path().join("workspace"));
    let state = directory.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    write_editor(&state, "Read, Write, Bash", "");
    let observed = run_live(
        directory,
        root,
        guard,
        CASE,
        LiveStart::New { padding: None },
    )
    .await;
    assert_cargo(&observed, &rust);
}

fn plan() -> GoalPlanContract {
    GoalPlanContract {
        mission: "Edit, build and test the crate".into(),
        paths: vec!["src".into()],
        commands: vec!["sh".into()],
        resource_class: ToolResourceClass::Build,
        command_timeout_ms: Some(crate::tools::types::MAX_COMMAND_TIMEOUT_MS),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
            path: SOURCE.into(),
            content: EDITED.into(),
        },
        budget: GoalBudget::default(),
    }
}

fn cargo(script: &str) -> ProviderOutcome {
    tool_outcome(ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: ".".into(),
        resource_class: Some(ToolResourceClass::Build),
        timeout_ms: None,
        additional_reads: None,
    })
}

impl LiveProvider {
    pub(super) fn cargo_outcome(&self, request: &ProviderRequest, step: usize) -> ProviderOutcome {
        match (request.phase, step) {
            (AgentPhase::RootPlan, 0) => super::super::scenario::plan_outcome(plan()),
            (AgentPhase::RootPlan, 1) => {
                let ProviderOutcome::Delegate(mut delegate) = self.delegate(request) else {
                    unreachable!()
                };
                let assignment = delegate.children[0].editor.as_mut().unwrap();
                assignment.mission = plan().mission;
                assignment.mode = WorkerMode::Background;
                assignment.lease = vec![SOURCE.into()];
                assignment.commands = vec!["sh".into()];
                assignment.criterion = plan().success_criterion;
                delegate.children[0].objective = assignment.mission.clone();
                ProviderOutcome::Delegate(delegate)
            }
            (AgentPhase::ChildWork, 0) => self.wave_write(SOURCE, EDITED),
            (AgentPhase::ChildWork, 1) => {
                assert_ne!(
                    std::fs::read_to_string(self.workspace.join(SOURCE)).unwrap(),
                    EDITED
                );
                cargo(COLD)
            }
            (AgentPhase::ChildWork, 2) => cargo(WARM),
            (AgentPhase::ChildWork, 3) => super::waves::read(SOURCE),
            (AgentPhase::ChildWork, _) => finish_with("Edit built and tested"),
            (AgentPhase::RootSynthesis, _) if request.input.child_results.is_empty() => {
                finish_with("Wait for the editor's build")
            }
            (AgentPhase::RootSynthesis, _)
                if !request
                    .input
                    .tools
                    .as_ref()
                    .unwrap()
                    .steps
                    .iter()
                    .flat_map(|step| &step.observations)
                    .any(
                        |o| matches!(&o.intent.call, ToolCall::Read { path, .. } if path == SOURCE),
                    ) =>
            {
                super::waves::read(SOURCE)
            }
            (AgentPhase::RootSynthesis, _) => finish_with("Primary verified the built edit"),
            (AgentPhase::ToolReview, _) => ProviderOutcome::Review(ReviewVerdict::Approve),
            (AgentPhase::RootPlan, _) => panic!("unexpected Cargo planning call"),
        }
    }
}

fn assert_cargo(observed: &TeamObservation, rust: &Path) {
    let workspace = observed.directory.path().join("workspace");
    let view = &observed.view;
    let run = &view.runs[0];
    let editor = &run.agents[1];
    let steps: Vec<_> = editor
        .tools
        .iter()
        .filter_map(|tool| tool.observation.as_ref())
        .map(|o| {
            (
                o.disposition,
                o.output.chars().take(2048).collect::<String>(),
            )
        })
        .collect();
    assert_eq!(
        run.status,
        RunStatus::Finished,
        "{:?}; {:?}; {steps:?}",
        run.failure,
        editor.assignment.as_ref().map(|a| &a.outcome)
    );
    assert_eq!(view.goals[0].status, GoalStatus::Completed);
    assert_eq!(editor.tools.len(), 5);
    let (write, commands, integration) = (&editor.tools[0], &editor.tools[1..3], &editor.tools[4]);
    assert!(matches!(write.intent.call, ToolCall::Write { .. }));
    for (command, warm) in commands.iter().zip([false, true]) {
        assert!(command.intent.target.editor() && command.checkpoint.is_none());
        assert_eq!(
            command.intent.limits.resource_class,
            ToolResourceClass::Build
        );
        assert!(command.intent.limits.cache_bytes > 0);
        let observation = command.observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{}",
            observation.output
        );
        assert!(matches!(
            observation.attestation,
            Some(Attestation::Guard(_))
        ));
        assert_eq!(observation.cache_update, CacheUpdate::Published);
        let result: crate::tools::types::CommandResult =
            serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(result.changeset.files.is_empty());
        // The editor's added test proves Cargo built its private workspace, not the Workspace.
        assert!(
            result.stdout.contains("2 passed; 0 failed; 1 ignored"),
            "{result:?}"
        );
        if warm {
            for package in ["offline-cargo-fixture", "arany-fixture-dependency"] {
                assert!(
                    result.stderr.contains(&format!("Fresh {package}")),
                    "{result:?}"
                );
            }
            assert!(!result.stderr.contains("Compiling"), "{result:?}");
        } else {
            assert_eq!(
                result.stdout.lines().next().map(Path::new),
                Some(rust.join("bin/cargo").as_path()),
                "{result:?}"
            );
        }
    }
    assert!(crate::session::editor_integration(&integration.intent.call));
    assert_eq!(integration.intent.target, EffectTarget::Workspace);
    assert!(integration.checkpoint.is_some());
    let published = integration.observation.as_ref().unwrap();
    assert_eq!(published.disposition, ToolDisposition::Succeeded);
    let files = crate::session::rewind::publication_files(run, published).unwrap();
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        [SOURCE]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(SOURCE)).unwrap(),
        EDITED
    );
    assert!(!workspace.join("target").exists());
    let history = &run.worker_history()[0];
    assert!(history.lease_released && history.verified);
}
