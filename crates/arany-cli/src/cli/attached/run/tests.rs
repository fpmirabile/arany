mod budget_tests {
    use super::super::*;
    use arany::AdmittedProviders;
    use arany::StateRoot;
    use arany::test_support::{
        engine::open,
        goals::{assert_long_turn, facts_workspace, long_turn_provider, replayed},
        scenario::request,
    };
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    #[tokio::test]
    async fn attached_defaults_complete_past_64_calls_and_30_active_minutes() {
        for finite in [false, true] {
            let temp = tempfile::tempdir().expect("private root");
            let workspace = facts_workspace(temp.path(), "Fact");
            let state = temp.path().join("state");
            let engine = open(
                StateRoot::admit(&state).expect("State"),
                arany::EngineConfig {
                    guard_executable: Some(std::env::current_exe().expect("test executable")),
                    ..arany::EngineConfig::default()
                },
            )
            .expect("Engine");
            let config = state.join("tools.json");
            std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).expect("config");
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .expect("private config");
            if finite {
                let preferences = serde_json::json!({"version":6,"preferences":{
                    "completion":"tabs","view":"fullscreen",
                    "goal_budget":arany::GoalBudget { max_provider_calls:Some(1), ..Default::default() },
                    "resources":arany::ToolResourceSettings::default(),"copy_on_select":true,"newline":"shift_enter"
                }});
                StateRoot::open_existing(&state)
                    .expect("State")
                    .record(arany::RecordKind::UiPreferences)
                    .replace(&serde_json::to_vec(&preferences).expect("settings"))
                    .expect("finite settings");
            }
            let engine = Arc::new(engine);
            let id = engine
                .create_session(workspace.clone(), None)
                .await
                .expect("Session");
            let (_, view) = replayed(&state, id).await;
            let settings = super::super::super::controls::restore_settings_at(&state)
                .await
                .expect("missing settings");
            let mut input = request(&workspace, "Inspect the folder");
            input.session_id = Some(id);
            input.title = None;
            input.goal = super::super::super::active::goal_request(
                &view,
                super::super::super::active::GoalStart::Plain,
                settings,
            )
            .expect("attached request");
            let provider = Arc::new(long_turn_provider());

            let result = embedded::run_with_providers(
                &engine,
                AdmittedProviders::single(provider.clone()),
                input,
                &state,
                true,
                None,
                arany::ApprovalMode::AutoEdits,
            )
            .await
            .expect("attached admission");
            let outcome = finish_attempt(Ok(result), false).expect("attached completion");
            assert_eq!(
                outcome.run.assistant_message.as_deref(),
                if finite {
                    None
                } else {
                    Some("Long turn completed")
                }
            );
            assert_eq!(
                outcome.run.status,
                if finite {
                    arany::RunStatus::Failed
                } else {
                    arany::RunStatus::Finished
                }
            );
            Arc::try_unwrap(engine)
                .ok()
                .expect("Engine released")
                .close()
                .await
                .expect("shutdown");
            let (events, view) = replayed(&state, id).await;
            if finite {
                assert_eq!(
                    crate::presentation::run_feedback(&outcome.run).as_deref(),
                    Some(
                        "Error: Run failed. The Goal budget refused the next action. No answer was committed. Use /agents to inspect this Run."
                    )
                );
                assert_eq!(view.goals[0].budget.max_provider_calls, Some(1));
                assert_eq!(view.goals[0].provider_calls, 1);
                assert!(matches!(
                    events.last().expect("receipt").event,
                    arany::Event::GoalFinished {
                        exhausted: true,
                        ..
                    }
                ));
                assert!(view.runs[0].tools.is_empty());
            } else {
                assert_long_turn(&events, &view);
                assert!(provider.responses.lock().expect("script").is_empty());
            }
            assert_eq!(
                provider.requests.lock().expect("requests").len(),
                if finite { 1 } else { 65 }
            );
        }
    }
}

mod escape_tests {
    use super::super::*;

    #[test]
    fn run_end_notice_never_claims_a_dropped_note_was_kept() {
        assert_eq!(
            run_end_notice(false, true, false),
            Some(
                "Drafts exceed the combined limit; both kept; submit or clear this draft, then press Up"
            )
        );
        assert_eq!(
            run_end_notice(false, false, false),
            Some(
                "Drafts exceed the combined limit; both kept; submit or clear this draft, then press Up"
            )
        );
        assert_eq!(
            run_end_notice(false, true, true),
            Some("Steering note withdrawn; draft retained")
        );
        assert_eq!(run_end_notice(true, false, true), Some(STEERING_SENT));
        assert_eq!(run_end_notice(false, false, true), None);
    }

    #[test]
    fn escape_cancels_only_when_nothing_else_claims_it() {
        let idle = |input, menu, clipboard, stop| active_escape(input, menu, clipboard, stop);
        assert_eq!(
            idle(TerminalInput::Escape, false, false, ActiveStop::Running),
            TerminalInput::Interrupt
        );
        assert_eq!(
            idle(TerminalInput::Escape, true, false, ActiveStop::Running),
            TerminalInput::Escape,
            "an open menu closes first"
        );
        assert_eq!(
            idle(TerminalInput::Escape, false, true, ActiveStop::Running),
            TerminalInput::Escape,
            "a pending clipboard read is cancelled, not the Run"
        );
        assert_eq!(
            idle(TerminalInput::Escape, false, false, ActiveStop::Cancelling),
            TerminalInput::Escape,
            "a second Escape never forces shutdown"
        );
        assert_eq!(
            idle(TerminalInput::Interrupt, false, false, ActiveStop::Running),
            TerminalInput::Interrupt
        );
    }
}
