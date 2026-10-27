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
                let preferences = serde_json::json!({"version":7,"preferences":{
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

mod approval_reply_tests {
    use super::super::approval::linear_approval_choice;
    use crate::presentation::ToolApprovalChoice::{
        AutoEdits, ExactCommand, GitPrefix, HostPrefix, No, Yes,
    };

    #[test]
    fn session_hint_follows_live_auto_edits_mode_without_changing_choices() {
        use crate::presentation::ToolApprovalPrompt;
        use arany::ApprovalMode::{Auto, AutoEdits, Request};
        for unguarded in [false, true] {
            for choices in [vec![Yes, No], vec![Yes, ExactCommand, No]] {
                let prompt = ToolApprovalPrompt {
                    intent_id: uuid::Uuid::now_v7(),
                    lines: vec!["Complete escaped action".into()],
                    choices,
                    unguarded,
                };
                for mode in [
                    Some(AutoEdits),
                    Some(Request),
                    Some(Auto),
                    None,
                    Some(AutoEdits),
                ] {
                    let displayed = prompt.for_mode(mode);
                    let hint = unguarded
                        && mode == Some(AutoEdits)
                        && !prompt.choices.contains(&ExactCommand);
                    assert_eq!(
                        displayed.lines.len(),
                        prompt.lines.len() + usize::from(hint)
                    );
                    assert_eq!(displayed.lines[0], prompt.lines[0]);
                    assert_eq!(displayed.choices, prompt.choices);
                    assert_eq!(displayed.intent_id, prompt.intent_id);
                    assert_eq!(
                        displayed.lines.last().unwrap()
                            == "Session approval is offered in Request approvals (Shift+Tab)",
                        hint
                    );
                }
                assert_eq!(prompt.lines, ["Complete escaped action"]);
            }
        }
    }

    #[test]
    fn numbered_replies_use_choice_identity_and_refuse_unavailable_choices() {
        let prefix = GitPrefix(arany::permissions::TypedGitPrefix::Branch);
        for choices in [
            vec![Yes, prefix, No],
            vec![Yes, No],
            vec![Yes, AutoEdits, No],
            vec![Yes, ExactCommand, No],
            vec![Yes, HostPrefix, No],
        ] {
            for (index, choice) in choices.iter().enumerate() {
                assert_eq!(
                    linear_approval_choice(&(index + 1).to_string(), &choices),
                    Some(*choice)
                );
            }
            for (answer, expected) in [
                ("", Some(No)),
                ("yes", Some(Yes)),
                (" YES ", Some(Yes)),
                ("Yes, for this session", None),
                ("no", Some(No)),
                ("allow", None),
                ("always", None),
                ("0", None),
                ("4", None),
            ] {
                assert_eq!(linear_approval_choice(answer, &choices), expected);
            }
        }
        assert_eq!(
            linear_approval_choice("yes", &[Yes, ExactCommand, No])
                .unwrap()
                .reply(),
            arany::ApprovalReply::AllowOnce
        );
        assert_eq!(AutoEdits.reply(), arany::ApprovalReply::AllowOnce);
        assert_eq!(ExactCommand.reply(), arany::ApprovalReply::AllowForSession);
        assert_eq!(No.reply(), arany::ApprovalReply::Deny);
        assert_eq!(prefix.reply(), arany::ApprovalReply::AllowPrefixInFolder);
        assert_eq!(
            HostPrefix.reply(),
            arany::ApprovalReply::AllowPrefixInFolder
        );
    }
}

mod driver_tests {
    use super::super::super::active::session_with_goal;
    use super::super::*;
    use crate::terminal::ComposerEdit;
    use arany::SteeringRefused;

    #[test]
    fn end_of_input_waits_for_only_the_current_run_to_settle() {
        let mut view = session_with_goal(false);
        let starting_sequence = view.last_sequence;
        assert!(!run_settled(&view, starting_sequence), "no current Run");
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Finished,
            accepted_sequence: starting_sequence,
            finished_sequence: Some(starting_sequence),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        view.runs.push(run.clone());
        assert!(
            !run_settled(&view, starting_sequence),
            "an earlier Run cannot defer exit"
        );
        run.accepted_sequence = starting_sequence + 1;
        for (status, settled) in [
            (arany::RunStatus::Pending, false),
            (arany::RunStatus::Active, false),
            (arany::RunStatus::Finished, true),
            (arany::RunStatus::Failed, true),
            (arany::RunStatus::Cancelled, true),
        ] {
            run.status = status;
            *view.runs.last_mut().unwrap() = run.clone();
            assert_eq!(run_settled(&view, starting_sequence), settled, "{status:?}");
        }
    }

    #[test]
    fn exit_waits_for_only_the_current_run_to_settle() {
        let mut view = session_with_goal(false);
        let starting_sequence = view.last_sequence;
        let typed = |text: &str| {
            let mut composer = Composer::default();
            for character in text.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            composer
        };
        let exit = typed("/exit");
        assert!(
            !exits_after_settled_run(&exit, &view, starting_sequence),
            "no current Run"
        );
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Active,
            accepted_sequence: starting_sequence + 1,
            finished_sequence: None,
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        view.runs.push(run.clone());
        assert!(
            !exits_after_settled_run(&exit, &view, starting_sequence),
            "an active Run keeps /exit refused"
        );
        run.status = arany::RunStatus::Finished;
        *view.runs.last_mut().unwrap() = run;
        assert!(exits_after_settled_run(&exit, &view, starting_sequence));
        for other in ["/exit now", "/status", "exit"] {
            assert!(
                !exits_after_settled_run(&typed(other), &view, starting_sequence),
                "{other}"
            );
        }
    }

    #[test]
    fn steering_refusals_name_the_retained_draft_without_echoing_text() {
        assert_eq!(
            super::super::input::steering_refusal(SteeringRefused::TooLarge, false),
            "Steering note exceeds 8.0 KB; draft retained"
        );
        assert_eq!(
            super::super::input::steering_refusal(SteeringRefused::Unavailable, true),
            "Steering note unavailable; draft retained"
        );
        assert_eq!(
            super::super::input::steering_refusal(SteeringRefused::RunFull, true),
            "No room for more steering notes this Run. Draft retained; press Enter after this Goal to submit"
        );
    }

    #[test]
    fn recorded_note_clears_only_an_unchanged_composer() {
        let mut seen = 0;
        let mut composer = Composer::default();
        for character in "first note".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut offered = Some(composer.text().to_owned());
        assert!(!settle_recorded_note(
            0,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert!(settle_recorded_note(
            1,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert!(
            composer.is_empty(),
            "an unchanged offer clears the composer"
        );
        assert!(offered.is_none());
        assert!(!settle_recorded_note(
            1,
            &mut seen,
            &mut offered,
            &mut composer
        ));

        // An edit after the Engine took the note keeps the user's text as a retained draft.
        for character in "second".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut offered = Some("second".to_owned());
        composer.apply(TerminalInput::Character('!'));
        assert!(settle_recorded_note(
            2,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert_eq!(composer.text(), "second!");
    }

    #[test]
    fn held_linear_draft_waits_aside_and_returns_before_later_text() {
        let mut composer = Composer::default();
        for character in "kept".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let held = hold_draft(&mut composer).expect("refused draft is held");
        assert!(
            composer.is_empty(),
            "the invisible draft leaves the composer"
        );
        assert!(hold_draft(&mut composer).is_none());

        for character in "later".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(release_held_draft(&mut composer, held));
        assert_eq!(composer.text(), "kept\nlater");
    }

    #[test]
    fn overflowing_release_keeps_the_newer_draft() {
        let mut composer = Composer::default();
        composer.insert_paste(&"a".repeat(64 * 1024)).unwrap();
        let held = hold_draft(&mut composer).expect("refused draft is held");
        for character in std::iter::repeat_n('b', 4000) {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(!release_held_draft(&mut composer, held));
        assert_eq!(composer.text(), "b".repeat(4000));
        assert_eq!(
            composer.apply(TerminalInput::Character('c')),
            ComposerEdit::AtCapacity
        );
        composer.clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.text(), "a".repeat(64 * 1024));
        assert!(composer.has_text_pastes());
    }

    #[test]
    fn retained_draft_names_the_autonomous_goal_boundary() {
        assert_eq!(
            draft_retained_notice(true),
            "Draft retained; press Enter after this Goal to submit"
        );
        assert_eq!(
            draft_retained_notice(false),
            "Draft retained; press Enter after this Run to submit"
        );
    }
}
