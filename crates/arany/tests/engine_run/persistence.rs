use super::*;

#[test]
fn persistence_capacity_is_admitted_before_provider_usage() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "Fact");
        // Each admitted action reserves its worst-case Event envelope before dispatch, including three
        // same-route retry facts per call; the boundary is the largest prefix that still admits the Run.
        // A single plain Run commits 11 Events and one Team child 17.
        for (index, (prefix, policy, admitted, tools, branch, previews)) in [
            (9987, CollaborationPolicy::Single, false, false, false, false),
            (9986, CollaborationPolicy::Single, true, false, false, false),
            (9977, CollaborationPolicy::Team { max_active_children: 1 }, false, false, false, false),
            (9975, CollaborationPolicy::Team { max_active_children: 1 }, true, false, false, false),
            (9986, CollaborationPolicy::Single, false, true, false, false),
            (9985, CollaborationPolicy::Single, true, true, false, false),
            (9986, CollaborationPolicy::Single, false, false, true, false),
            (9985, CollaborationPolicy::Single, true, false, true, false),
            (9923, CollaborationPolicy::Single, false, false, false, true),
            (9922, CollaborationPolicy::Single, true, false, false, true),
        ].into_iter().enumerate() {
            let path = temp.path().join(format!("state-{index}"));
            let finish = || ProviderOutcome::Answer(Answer { text: "answer".into() });
            let provider = Arc::new(ScriptedProvider::new(vec![finish()]));
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let id = run_to_end(&engine, &provider, request(&workspace, "Initial")).await.unwrap().session_id;
            engine.close().await.unwrap();
            pad_capacity_history(&path, id, i64::from(prefix), branch);
            let responses = if matches!(policy, CollaborationPolicy::Team { .. }) {
                vec![delegate(&[("Child", None)]), finish(), finish()]
            } else { vec![finish()] };
            let provider = ScriptedProvider::new(responses);
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig { guard_executable: Some(std::env::current_exe().expect("test executable")), ..Default::default() }).unwrap();
            let id = if branch { engine.fork_session(id, workspace.clone(), None).await.unwrap() } else { id };
            if tools {
                use std::os::unix::fs::PermissionsExt;
                let config = path.join("tools.json");
                std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
                std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let max_provider_calls = match &policy {
                CollaborationPolicy::Single => 1,
                CollaborationPolicy::Auto { max_active_children }
                | CollaborationPolicy::Team { max_active_children } => {
                    u32::from(*max_active_children) + 2
                }
            };
            let mut next = request(&workspace, "Next");
            next.session_id = Some(id);
            next.title = None;
            next.policy = policy;
            next.goal.budget = Some(arany::GoalBudget {
                max_provider_calls: Some(max_provider_calls),
                max_tokens: None,
                max_active_ms: Some(30 * 60 * 1000),
                max_runs: 1,
            });
            let controls = RunControls {
                tools: if tools { ToolGrant::Configured } else { ToolGrant::None },
                hints: previews.then(RunHints::with_previews),
                ..Default::default()
            };
            let result = run_with(&engine, &provider, next, controls).await;
            if admitted {
                assert_eq!(result.unwrap().run.status, RunStatus::Finished, "case {index}");
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "case {index}: {result:?}");
            }
            engine.close().await.unwrap();
            let phases: Vec<_> = requests.lock().unwrap().iter().map(|request| request.phase).collect();
            // Each action is admitted before its own dispatch: a Team refused at its child wave has already dispatched the root plan.
            let expected_phases = match (admitted, policy) {
                (true, CollaborationPolicy::Single) => vec![AgentPhase::RootPlan],
                (true, _) => vec![AgentPhase::RootPlan, AgentPhase::ChildWork, AgentPhase::RootSynthesis],
                (false, CollaborationPolicy::Single) => vec![],
                (false, _) => vec![AgentPhase::RootPlan],
            };
            assert_eq!(phases, expected_phases, "case {index}");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            let expected_event_count = if branch {
                // The fork's own Event plus the Single Run's admitted or refused prefix.
                if admitted { 12 } else { 1 }
            } else if admitted {
                prefix as usize
                    + if matches!(policy, CollaborationPolicy::Single) {
                        11
                    } else {
                        17
                    }
            } else {
                // A Single Run refused at Run start commits nothing; a Team refused at its child wave keeps its
                // Run start and root plan but no child AgentSpawned.
                prefix as usize
                    + match policy {
                        CollaborationPolicy::Single => 0,
                        _ => 7,
                    }
            };
            assert_eq!(
                store.load_session(id).await.unwrap().len(),
                expected_event_count,
                "case {index}: exact committed Event count"
            );
            let view = store.load_view(id).await.unwrap().unwrap();
            let linked = if admitted || !matches!(policy, CollaborationPolicy::Single) { 2 } else { 1 };
            assert_eq!(view.runs.len(), linked, "case {index}: a refused Single Run is never linked");
            assert_eq!(view.runs[0].status, RunStatus::Finished);
            if linked == 2 {
                // A refused Run is an interrupted prefix: replay labels it interrupted, never Finished.
                assert_eq!(view.runs[1].status == RunStatus::Finished, admitted, "case {index}");
            }
            store.close().await.unwrap();
        }
        // Two children in two slots form one wave with a 5N+6 = 16 Event tail (each child call may also record a
        // steering note) plus three retry facts for each child call and the synthesis call (23): 9967 admits and
        // the root call reserves one more for a recorded plan.
        for (prefix, admitted) in [(9967_i64, true), (9969, false)] {
            let path = temp.path().join(format!("two-child-{prefix}"));
            let finish = || ProviderOutcome::Answer(Answer { text: "answer".into() });
            let provider = Arc::new(ScriptedProvider::new(vec![finish()]));
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let id = run_to_end(&engine, &provider, request(&workspace, "Initial")).await.unwrap().session_id;
            engine.close().await.unwrap();
            let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            let first_padding_sequence: i64 = transaction
                .query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM events", [], |row| row.get(0))
                .unwrap();
            for sequence in first_padding_sequence..=prefix {
                transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',?3,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, id.to_string(), arany::test_support::payload_version("SessionRenamed")]).unwrap();
            }
            transaction.commit().unwrap();
            drop(connection);
            let provider = ScriptedProvider::new(vec![
                delegate(&[("Child one", None), ("Child two", None)]),
                finish(),
                finish(),
                finish(),
            ]);
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let mut next = request(&workspace, "Next");
            next.session_id = Some(id);
            next.title = None;
            next.policy = CollaborationPolicy::Team { max_active_children: 2 };
            next.goal.budget = Some(arany::GoalBudget {
                max_provider_calls: Some(4),
                max_tokens: None,
                max_active_ms: Some(30 * 60 * 1000),
                max_runs: 1,
            });
            let result = run_to_end(&engine, &provider, next).await;
            if admitted {
                assert_eq!(result.unwrap().run.status, RunStatus::Finished, "two children at {prefix}");
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "two children at {prefix}");
            }
            engine.close().await.unwrap();
            let phases: Vec<_> = requests.lock().unwrap().iter().map(|request| request.phase).collect();
            let expected_phases = if admitted {
                vec![AgentPhase::RootPlan, AgentPhase::ChildWork, AgentPhase::ChildWork, AgentPhase::RootSynthesis]
            } else {
                vec![AgentPhase::RootPlan]
            };
            assert_eq!(phases, expected_phases, "two children at {prefix}");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            let events = store.load_session(id).await.unwrap();
            let spawned = events.iter().filter(|event| matches!(event.event, Event::AgentSpawned { role: arany::AgentRole::Child, .. })).count();
            assert_eq!(spawned, if admitted { 2 } else { 0 }, "two children at {prefix}: AgentSpawned");
            assert_eq!(events.len(), prefix as usize + if admitted { 21 } else { 7 }, "two children at {prefix}: committed Events");
            store.close().await.unwrap();
        }
        for (index, (prefix, prior_forks, admitted)) in [
            (Some(9999), 0, true),
            (Some(10_000), 0, false),
            (None, 7, true),
            (None, 8, false),
        ].into_iter().enumerate() {
            let path = temp.path().join(format!("fork-admission-{index}"));
            let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "answer".into() })]));
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let root = run_to_end(&engine, &provider, request(&workspace, "Fork source")).await.unwrap().session_id;
            engine.close().await.unwrap();
            let mut connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            let first_padding_sequence: i64 = transaction
                .query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM events", [], |row| {
                    row.get(0)
                })
                .unwrap();
            if let Some(prefix) = prefix {
                for sequence in first_padding_sequence..=prefix {
                    transaction.execute("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',?3,'{\"title\":\"x\"}',0)", rusqlite::params![sequence, root.to_string(), arany::test_support::payload_version("SessionRenamed")]).unwrap();
                }
            }
            let event_count_before_forks = prefix.unwrap_or(first_padding_sequence - 1);
            transaction.commit().unwrap();
            drop(connection);
            let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
            let mut source = root;
            for _ in 0..prior_forks {
                source = engine.fork_session(source, workspace.clone(), None).await.unwrap();
            }
            let result = engine.fork_session(source, workspace.clone(), None).await;
            let forked = if admitted {
                Some(result.expect("a boundary-admitted fork must commit"))
            } else {
                assert!(matches!(result, Err(EngineError::Store(arany::StoreError::ReplayLimit))), "fork case {index}: resulting lineage must be rejected before commitment");
                None
            };
            engine.close().await.unwrap();
            assert_eq!(provider.requests.lock().unwrap().len(), 1, "fork admission makes no Provider call");
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            for id in [Some(root), Some(source), forked].into_iter().flatten() {
                let view = store.load_view(id).await.unwrap().unwrap();
                assert_eq!(view.runs.len(), 1);
                assert_eq!(view.runs[0].status, RunStatus::Finished);
                assert_eq!(view.runs[0].assistant_message.as_deref(), Some("answer"));
            }
            store.close().await.unwrap();
            let connection = rusqlite::Connection::open_with_flags(path.join("events.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let count: i64 = connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0)).unwrap();
            assert_eq!(
                count,
                event_count_before_forks + i64::from(prior_forks) + i64::from(admitted),
                "fork case {index}: rejected insert must roll back"
            );
        }
        let path = temp.path().join("compactions");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "answer".into() }), ProviderOutcome::Answer(Answer { text: "later answer".into() })])
            .with_compactions((0..66).map(|_| Err(ProviderError::RemoteHttp(403))).collect());
        let calls = provider.compact_requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default()).unwrap();
        let id = run_to_end(&engine, &provider, request(&workspace, "Source")).await.unwrap().session_id;
        for _ in 0..65 {
            assert!(matches!(engine.compact_session(&AdmittedProviders::single(provider.clone()), id, workspace.clone()).await.unwrap().status, CompactionStatus::Failed { .. }));
        }
        assert_eq!(calls.lock().unwrap().len(), 65);
        let mut later = request(&workspace, "Later");
        later.session_id = Some(id);
        later.title = None;
        run_to_end(&engine, &provider, later).await.unwrap();
        let branch = engine.fork_session(id, workspace.clone(), None).await.unwrap();
        assert!(matches!(engine.compact_session(&AdmittedProviders::single(provider.clone()), branch, workspace.clone()).await.unwrap().status, CompactionStatus::Failed { .. }), "inherited compactions do not block the fork's own compaction attempt");
        assert_eq!(calls.lock().unwrap().len(), 66);
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        assert_eq!(store.load_view(id).await.unwrap().unwrap().compactions.len(), 65);
        assert_eq!(store.load_view(branch).await.unwrap().unwrap().compactions.len(), 66);
        store.close().await.unwrap();
    });
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "file-backed 256 MiB database-capacity admission and recovery"
)]
fn database_capacity_is_admitted_before_provider_usage() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "answer".into() })]));
        let engine = engine_support::open(StateRoot::admit(&state_path).unwrap(), EngineConfig::default())
        .unwrap();
        let id = run_to_end(&engine, &provider, request(&workspace, "Source")).await.unwrap().session_id;
        engine.close().await.unwrap();
        let database = state_path.join("events.sqlite3");
        let mut connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE capacity_padding(value BLOB NOT NULL) STRICT;").unwrap();
        let target_pages = (256 * 1024 * 1024 - 4 * 1024 * 1024) / 4096 - 4;
        let transaction = connection.transaction().unwrap();
        transaction.execute(
            "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n < ?1) INSERT INTO capacity_padding SELECT zeroblob(3000) FROM rows",
            [target_pages - 300],
        ).unwrap();
        for _ in 0..512 {
            let pages: i64 = transaction.query_row("PRAGMA page_count", [], |row| row.get(0)).unwrap();
            if pages >= target_pages {
                break;
            }
            transaction.execute("INSERT INTO capacity_padding VALUES (zeroblob(3000))", []).unwrap();
        }
        let pages: i64 = transaction.query_row("PRAGMA page_count", [], |row| row.get(0)).unwrap();
        assert!((target_pages..=target_pages + 1).contains(&pages), "fixture must reach the admission floor");
        transaction.commit().unwrap();
        drop(connection);
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "r".repeat(32 * 1024) })]).with_compactions(vec![Ok(CompactionResponse {
            summary: "source and recovered answer".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let calls = provider.requests.clone();
        let compactions = provider.compact_requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).unwrap(), EngineConfig { guard_executable: Some(std::env::current_exe().expect("test executable")), ..Default::default() }).unwrap();
        let next = || {
            let mut value = request(&workspace, "Recovered");
            value.session_id = Some(id);
            value.title = None;
            value
        };
        for policy in [CollaborationPolicy::Single, CollaborationPolicy::Team { max_active_children: 8 }] {
            let mut attempt = next();
            attempt.objective = "large paste ".repeat(800);
            attempt.policy = policy;
            assert!(matches!(run_to_end(&engine, &provider, attempt).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
            assert!(calls.lock().unwrap().is_empty(), "database capacity rejection must precede inference");
        }
        {
            use std::os::unix::fs::PermissionsExt;
            let config = state_path.join("tools.json");
            std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(matches!(run_with(&engine, &provider, next(), RunControls { tools: ToolGrant::Configured, ..Default::default() }).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
        assert!(matches!(engine.compact_session(&AdmittedProviders::single(provider.clone()), id, workspace.clone()).await, Err(EngineError::Store(arany::StoreError::StorageFull))));
        assert!(calls.lock().unwrap().is_empty());
        assert!(compactions.lock().unwrap().is_empty());
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(id).await.unwrap();
        assert_eq!(events.len(), 12);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalFinished { .. }))
                .count(),
            1
        );
        let view = store.load_view(id).await.unwrap().unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert!(view.compactions.is_empty());
        store.close().await.unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("DROP TABLE capacity_padding; VACUUM;").unwrap();
        drop(connection);
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "recovered answer".into() })]).with_compactions(vec![Ok(CompactionResponse {
            summary: "source and recovered answer".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let calls = provider.requests.clone();
        let compactions = provider.compact_requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(StateRoot::admit(&state_path).unwrap(), EngineConfig::default()).unwrap();
        let recovered = run_to_end(&engine, &provider, next()).await.unwrap();
        assert_eq!(recovered.run.status, RunStatus::Finished);
        engine.compact_session(&AdmittedProviders::single(provider.clone()), id, workspace.clone()).await.unwrap();
        engine.close().await.unwrap();
        {
            let requests = calls.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.run_id, recovered.run.id);
            assert_eq!(request.agent_run_id, recovered.run.agents[0].id);
            assert_eq!(request.phase, AgentPhase::RootPlan);
            assert_eq!(request.collaboration, CollaborationPolicy::Single);
            assert_eq!(request.model, "test-model");
            assert_eq!(request.input.objective, "Recovered");
            assert!(request.input.images.is_empty());
            assert!(request.input.workspace_guidance.is_empty());
            assert_eq!(request.input.includes, vec![include_data("notes/facts.txt", "Fact")]);
            assert_eq!(request.input.history, vec![arany::HistoryTurn { user: "Source".into(), assistant: "answer".into() }]);
            assert!(request.input.context_summary.is_none());
            assert!(request.input.child_results.is_empty());
            assert_eq!(request.max_output_tokens, 4096);
            assert!(request.input.tools.is_none());
        }
        {
            let requests = compactions.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].session_id, id);
            assert_eq!(requests[0].covered_run_id, recovered.run.id);
            assert_eq!(requests[0].model, "test-model");
            assert!(requests[0].previous_summary.is_none());
            assert_eq!(requests[0].items, vec![
                CompactionItem::Completed(arany::HistoryTurn { user: "Source".into(), assistant: "answer".into() }),
                CompactionItem::Completed(arany::HistoryTurn { user: "Recovered".into(), assistant: "recovered answer".into() }),
            ]);
            assert_eq!(requests[0].max_output_tokens, 4096);
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(id).await.unwrap();
        let goal_event_counts = [
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalRunLinked { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalFinished { .. }))
                .count(),
        ];
        assert_eq!(goal_event_counts, [2; 4]);
        let non_goal_events = events
            .iter()
            .filter(|event| {
                !matches!(
                    event.event,
                    Event::GoalStarted { .. }
                        | Event::GoalRunLinked { .. }
                        | Event::GoalProviderCallReserved { .. }
                        | Event::GoalFinished { .. }
                )
            })
            .count();
        assert_eq!(non_goal_events, 16);
        assert_eq!(events.len(), 24);
        let view = store.load_view(id).await.unwrap().unwrap();
        assert_eq!(view.runs.len(), 2);
        assert!(view.runs.iter().all(|run| run.status == RunStatus::Finished));
        assert_eq!(view.runs[1].assistant_message.as_deref(), Some("recovered answer"));
        assert_eq!(view.compactions.len(), 1);
        assert!(matches!(&view.compactions[0].record.status, CompactionStatus::Succeeded { summary, .. } if summary == "source and recovered answer"));
        store.close().await.unwrap();
    });
}

#[test]
fn bounded_answer_output_must_fit_canonical_json() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "Fact");
        for (index, (bytes, child, accepted)) in [
            (8_000, false, true),
            (10_000, false, false),
            (32 * 1024, false, false),
            (16 * 1024, true, false),
        ]
        .into_iter()
        .enumerate()
        {
            let path = temp.path().join(format!("state-{index}"));
            let result = "\0".repeat(bytes);
            let mut responses = Vec::new();
            if child {
                responses.push(delegate(&[("Child", None)]));
            }
            let attempts = if accepted || child { 1 } else { 3 };
            for _ in 0..attempts {
                responses.push(ProviderOutcome::Answer(Answer {
                    text: result.clone(),
                }));
            }
            let provider = ScriptedProvider::new(responses);
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine =
                engine_support::open(StateRoot::admit(&path).unwrap(), EngineConfig::default())
                    .unwrap();
            let mut next = request(&workspace, "Question");
            if child {
                next.policy = CollaborationPolicy::Team {
                    max_active_children: 1,
                };
            }
            let outcome = run_to_end(&engine, &provider, next)
                .await
                .expect("durable output rejection, not StorageFull");
            assert_eq!(
                outcome.run.status,
                if accepted {
                    RunStatus::Finished
                } else {
                    RunStatus::Failed
                }
            );
            assert_eq!(
                outcome.run.assistant_message.as_deref(),
                accepted.then_some(result.as_str())
            );
            {
                let calls = requests.lock().unwrap();
                assert_eq!(calls.len(), if child { 2 } else { attempts });
                assert_eq!(calls[0].phase, AgentPhase::RootPlan);
                if child {
                    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
                }
            }
            engine.close().await.unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
            let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
            assert_eq!(view.runs[0], outcome.run);
            if !accepted {
                assert!(view.runs[0].agents.iter().any(|agent| {
                    agent.provider_calls.last().is_some_and(|call| {
                        call.disposition
                            == if child {
                                ProviderCallDisposition::InvalidResponse
                            } else {
                                ProviderCallDisposition::Rejected
                            }
                    })
                }));
            }
            store.close().await.unwrap();
        }
    });
}

#[test]
fn maximal_escaped_objective_with_the_largest_image_fits_its_events() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider =
            ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer { text: "ok".into() })]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut next = request(&workspace, &"\0".repeat(64 * 1024));
        let objective = next.objective.clone();
        next.images = vec![image_fixture(arany::MAX_IMAGE_BYTES)];
        let images = next.images.clone();
        let outcome = run_to_end(&engine, &provider, next)
            .await
            .expect("maximal objective and image Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.objective, objective);
        assert_eq!(outcome.run.images, images);
        {
            let calls = requests.lock().expect("requests");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].input.objective, objective);
            assert_eq!(
                calls[0]
                    .input
                    .images
                    .iter()
                    .map(|value| value.image.clone())
                    .collect::<Vec<_>>(),
                images
            );
        }
        let session_id = outcome.session_id;
        engine.close().await.expect("engine shutdown");

        std::fs::remove_file(workspace.join("notes/facts.txt")).expect("remove original include");
        let reopened = engine_support::open(
            StateRoot::open_existing(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("reopened Engine");
        let fork = reopened
            .fork_session(session_id, workspace.clone(), None)
            .await
            .expect("fork canonical history without inputs");
        reopened.close().await.expect("close reopened Engine");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(session_id)
            .await
            .expect("durable Events");
        let accepted = events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::MessageAccepted { .. }))
            .count();
        let started = events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::GoalStarted { .. }))
            .count();
        assert_eq!(
            (accepted, started),
            (1, 1),
            "the maximal Run is durably admitted and replayable"
        );
        for id in [session_id, fork] {
            let view = store
                .load_view(id)
                .await
                .expect("strict replay")
                .expect("Session");
            assert_eq!(view.runs[0].objective, objective);
            assert_eq!(view.runs[0].images, images);
        }
        assert_eq!(
            requests.lock().expect("requests").len(),
            1,
            "reopen and fork never call Provider"
        );
        store.close().await.expect("store shutdown");
    });
}

async fn assert_setup_blocked_consumers(
    engine: &Engine,
    store: &Store,
    state: &Path,
    workspace: &Path,
    session_id: SessionId,
) {
    for uncertain in [false, true] {
        let operation_id =
            arany::test_support::record_setup_edit_state(store, session_id, uncertain).await;
        let events = store.load_session(session_id).await.unwrap();
        let view = SessionView::replay(session_id, &events).unwrap().unwrap();
        assert_eq!(view.runs.len(), 2);
        assert_eq!(
            view.pending_provider_setup_edit,
            (!uncertain).then_some(operation_id)
        );
        assert_eq!(view.provider_setup_uncertain, uncertain);
        let sessions = list_sessions(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_owned(),
        )
        .await
        .unwrap();
        assert_setup_blocked_rewinds(engine, workspace, &view).await;
        assert_setup_blocked_lifecycle(engine, state, workspace, &view).await;
        assert_setup_blocked_submissions(engine, workspace, &view).await;
        assert_eq!(store.load_session(session_id).await.unwrap(), events);
        assert_eq!(
            list_sessions(
                StateRoot::open_existing(state).unwrap(),
                workspace.to_owned()
            )
            .await
            .unwrap(),
            sessions,
            "blocked operations create no Session or Event"
        );
    }
}

async fn assert_setup_blocked_rewinds(engine: &Engine, workspace: &Path, view: &SessionView) {
    for run in &view.runs {
        for result in [
            engine
                .rewind_conversation(view.id, workspace.to_owned(), run.id)
                .await,
            engine
                .rewind_conversation_at_head(
                    view.id,
                    workspace.to_owned(),
                    run.id,
                    (view.last_sequence, view.rewinds.prefix_digest()),
                )
                .await,
        ] {
            assert!(matches!(result, Err(EngineError::ProviderSetupBlocked)));
        }
    }
    let result = engine
        .rewind_preview(arany::RewindRequest {
            session_id: view.id,
            workspace: workspace.to_owned(),
            target: view.rewinds.checkpoints.last().unwrap().clone(),
            tools: ToolGrant::None,
            kept: Vec::new(),
            overwrite: Vec::new(),
        })
        .await;
    assert!(matches!(result, Err(EngineError::ProviderSetupBlocked)));
}

async fn assert_setup_blocked_lifecycle(
    engine: &Engine,
    state: &Path,
    workspace: &Path,
    view: &SessionView,
) {
    let root = || StateRoot::open_existing(state).unwrap();
    for result in [
        engine
            .fork_session(view.id, workspace.to_owned(), None)
            .await,
        engine
            .fork_session_at(
                view.id,
                workspace.to_owned(),
                view.runs[0].finished_sequence.unwrap(),
                None,
            )
            .await,
        fork_session(root(), workspace.to_owned(), view.id, None).await,
    ] {
        assert!(matches!(result, Err(EngineError::ProviderSetupBlocked)));
    }
    assert!(matches!(
        resume_session(root(), workspace.to_owned(), view.id).await,
        Err(EngineError::ProviderSetupBlocked)
    ));
    for result in [
        arany::set_session_defaults(root(), workspace.to_owned(), view.id, view.defaults.clone())
            .await,
        rename_session(
            root(),
            workspace.to_owned(),
            view.id,
            "Blocked change".into(),
        )
        .await,
    ] {
        assert!(matches!(result, Err(EngineError::ProviderSetupBlocked)));
    }
}

async fn assert_setup_blocked_submissions(engine: &Engine, workspace: &Path, view: &SessionView) {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let goal_id = view.goals.last().unwrap().id;
    for goal in [
        arany::GoalRequest::default(),
        arany::GoalRequest::autonomous(
            arany::GoalBudget::default(),
            arany::GoalPhase::Implementation,
        ),
        arany::GoalRequest::resume(goal_id, true),
        arany::GoalRequest::continue_from(
            goal_id,
            arany::GoalBudget::default(),
            Default::default(),
        ),
    ] {
        let mut input = request(workspace, "Must not disclose after setup");
        input.session_id = Some(view.id);
        input.title = None;
        if goal.continues.is_some() {
            input.objective.clear();
        }
        input.goal = goal;
        assert!(matches!(
            run_to_end(engine, &provider, input).await,
            Err(EngineError::ProviderSetupBlocked)
        ));
    }
    let providers = AdmittedProviders::single(provider.clone());
    assert!(matches!(
        engine
            .compact_session(&providers, view.id, workspace.to_owned())
            .await,
        Err(EngineError::ProviderSetupBlocked)
    ));
    for run in &view.runs {
        assert!(matches!(
            engine
                .auto_compact_session(&providers, view.id, workspace.to_owned(), run.id)
                .await,
            Err(EngineError::ProviderSetupBlocked)
        ));
    }
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(provider.compact_requests.lock().unwrap().is_empty());
}

#[test]
fn fork_pins_committed_parent_prefix_and_rejects_drift() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let other_workspace = temp.path().join("other");
        std::fs::create_dir_all(&other_workspace).expect("other workspace");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "first answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "second answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "fork answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "deep answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "bounded".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "bounded".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "bounded".into(),
            }),
        ]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut first_request = request(&workspace, "First question");
        first_request.goal.finish_after_run = false;
        first_request.images = vec![arany::test_support::test_image()];
        let first = run_to_end(&engine, &provider, first_request)
            .await
            .expect("source Run");
        let resumed = resume_session(
            StateRoot::admit(&state_path).expect("state readmission"),
            workspace.clone(),
            first.session_id,
        )
        .await
        .expect("exact Session resume");
        assert_eq!(resumed.id, first.session_id);
        assert_eq!(resumed.runs.len(), 1);
        assert!(matches!(
            resume_session(
                StateRoot::admit(&state_path).expect("state readmission"),
                other_workspace.clone(),
                first.session_id,
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            fork_session(
                StateRoot::admit(&state_path).expect("state readmission"),
                other_workspace.clone(),
                first.session_id,
                None,
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            rename_session(
                StateRoot::admit(&state_path).expect("state readmission"),
                other_workspace.clone(),
                first.session_id,
                "Other".into(),
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    other_workspace
                )
                .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        let fork_id = fork_session(
            StateRoot::admit(&state_path).expect("state readmission"),
            workspace.clone(),
            first.session_id,
            Some("Branch".into()),
        )
        .await
        .expect("forked Session");
        assert_ne!(fork_id, first.session_id);
        let continued = continue_session(
            StateRoot::open_existing(&state_path).expect("state readmission"),
            workspace.clone(),
        )
        .await
        .expect("latest fork in Workspace");
        assert_eq!(continued.id, fork_id);
        assert!(!continued.usage.groups.is_empty());
        assert!(
            continued
                .usage
                .groups
                .iter()
                .all(|group| group.session_id == first.session_id)
        );
        let listed = list_sessions(
            StateRoot::open_existing(&state_path).expect("state readmission"),
            workspace.clone(),
        )
        .await
        .expect("fork in Session picker");
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            [fork_id, first.session_id]
        );
        assert_eq!(listed[0].title, "Branch");
        assert_eq!(listed[0].defaults.model.as_deref(), Some("test-model"));
        let nested_id = engine
            .fork_session(fork_id, workspace.clone(), None)
            .await
            .expect("fork of fork before a new Run");

        let mut parent_request = request(&workspace, "Later source question");
        parent_request.session_id = Some(first.session_id);
        parent_request.title = None;
        parent_request.images = vec![arany::test_support::test_image()];
        run_to_end(&engine, &provider, parent_request)
            .await
            .expect("later source Run");

        let mut child_request = request(&workspace, "Branch question");
        child_request.session_id = Some(fork_id);
        child_request.title = None;
        let child = run_to_end(&engine, &provider, child_request)
            .await
            .expect("fork Run");
        assert_eq!(child.run.status, RunStatus::Finished);
        assert_eq!(
            child
                .run
                .config
                .as_ref()
                .expect("Run config")
                .history_run_ids,
            vec![first.run.id]
        );
        {
            let observed = requests.lock().expect("request lock");
            assert_eq!(observed.len(), 3);
            assert_eq!(observed[2].input.history.len(), 1);
            assert_eq!(observed[2].input.history[0].user, "First question");
            assert_eq!(observed[2].input.history[0].assistant, "first answer");
        }
        let boundary = first.run.finished_sequence.unwrap();
        assert!(matches!(
            engine
                .fork_session_at(
                    first.session_id,
                    workspace.clone(),
                    first.run.accepted_sequence,
                    None
                )
                .await,
            Err(EngineError::ForkNotAtRunBoundary)
        ));
        let earlier_id = engine
            .fork_session_at(first.session_id, workspace.clone(), boundary, None)
            .await
            .unwrap();
        let owner_id = engine
            .fork_session_at(nested_id, workspace.clone(), boundary, None)
            .await
            .unwrap();
        let store = Store::open(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let defaults = arany::SessionDefaults {
            provider: Some("openai".into()),
            model: Some("test-model".into()),
            policy: CollaborationPolicy::Single,
            ..Default::default()
        };
        arany::set_session_defaults(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
            first.session_id,
            defaults.clone(),
        )
        .await
        .unwrap();
        let source_before = store.load_session(first.session_id).await.unwrap();
        let unsaved = engine
            .rewind_conversation(first.session_id, workspace.clone(), first.run.id)
            .await
            .unwrap();
        assert!(unsaved.session_id.is_none());
        assert_eq!(unsaved.defaults, defaults);
        assert_eq!(unsaved.prompt.as_deref(), Some("First question"));
        assert_eq!(
            source_before,
            store.load_session(first.session_id).await.unwrap()
        );
        let source = store.load_view(first.session_id).await.unwrap().unwrap();
        assert_eq!(unsaved.images, source.runs[0].images);
        for head in [
            (source.last_sequence - 1, source.rewinds.prefix_digest()),
            (source.last_sequence, [0; 32]),
        ] {
            assert!(matches!(
                engine
                    .rewind_conversation_at_head(
                        first.session_id,
                        workspace.clone(),
                        source.runs[1].id,
                        head,
                    )
                    .await,
                Err(EngineError::RewindPreviewChanged)
            ));
            assert_eq!(
                source_before,
                store.load_session(first.session_id).await.unwrap()
            );
        }
        let restored = engine
            .rewind_conversation_at_head(
                first.session_id,
                workspace.clone(),
                source.runs[1].id,
                (source.last_sequence, source.rewinds.prefix_digest()),
            )
            .await
            .unwrap();
        assert_eq!(restored.images, source.runs[1].images);
        let restored = store
            .load_view(restored.session_id.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.runs.len(), 1);
        assert!(
            restored.goals.is_empty(),
            "an inherited paused Goal cannot be resumed or duplicate its budget"
        );
        let mut inherited_resume = request(&workspace, "Must not revive inherited Goal");
        inherited_resume.session_id = Some(restored.id);
        inherited_resume.title = None;
        inherited_resume.goal = arany::GoalRequest::resume(source.goals[0].id, true);
        assert!(matches!(
            run_to_end(&engine, &provider, inherited_resume).await,
            Err(EngineError::GoalUnavailable)
        ));
        assert_eq!(
            store
                .load_view(earlier_id)
                .await
                .unwrap()
                .unwrap()
                .runs
                .len(),
            1
        );
        assert_eq!(
            store
                .load_view(owner_id)
                .await
                .unwrap()
                .unwrap()
                .lineage
                .unwrap()
                .source_session_id,
            first.session_id
        );
        let mut deepest = first.session_id;
        for _ in 0..8 {
            deepest = engine
                .fork_session(deepest, workspace.clone(), None)
                .await
                .unwrap();
        }
        let heads_before = list_sessions(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            engine.fork_session(deepest, workspace.clone(), None).await,
            Err(EngineError::Store(arany::StoreError::ReplayLimit))
        ));
        assert_eq!(
            heads_before.len(),
            list_sessions(
                StateRoot::open_existing(&state_path).unwrap(),
                workspace.clone()
            )
            .await
            .unwrap()
            .len()
        );
        let mut deep_request = request(&workspace, "Deep question");
        deep_request.session_id = Some(deepest);
        deep_request.title = None;
        let deep = run_to_end(&engine, &provider, deep_request).await.unwrap();
        assert!(matches!(
            engine
                .fork_session_at(
                    deepest,
                    workspace.clone(),
                    deep.run.finished_sequence.unwrap(),
                    None
                )
                .await,
            Err(EngineError::ConversationRewindLimit)
        ));
        let avoided = engine
            .rewind_conversation(deepest, workspace.clone(), deep.run.id)
            .await
            .unwrap();
        assert_eq!(
            store
                .load_view(avoided.session_id.unwrap())
                .await
                .unwrap()
                .unwrap()
                .lineage
                .unwrap()
                .source_session_id,
            first.session_id
        );
        assert_setup_blocked_consumers(&engine, &store, &state_path, &workspace, first.session_id)
            .await;
        store.close().await.unwrap();
        for bytes in [8 * 1024, 8 * 1024 + 1, 64 * 1024] {
            let prompt = "x".repeat(bytes);
            let bounded = run_to_end(&engine, &provider, request(&workspace, &prompt))
                .await
                .unwrap();
            let unsaved = engine
                .rewind_conversation(bounded.session_id, workspace.clone(), bounded.run.id)
                .await
                .unwrap();
            assert!(unsaved.session_id.is_none());
            assert_eq!(unsaved.prompt.as_deref(), Some(prompt.as_str()));
        }
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store.load_session(fork_id).await.expect("fork Events");
        assert!(matches!(events[0].event, Event::SessionForked { .. }));
        let view = store
            .load_view(fork_id)
            .await
            .expect("resolved fork")
            .expect("fork exists");
        assert_eq!(view.title, "Branch");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[0].id, first.run.id);
        assert_eq!(view.runs[1].id, child.run.id);
        let nested = store
            .load_view(nested_id)
            .await
            .expect("resolved nested fork")
            .expect("nested fork exists");
        assert_eq!(nested.runs.len(), 1);
        assert_eq!(nested.runs[0].id, first.run.id);
        store.close().await.expect("store shutdown");

        let listed = list_sessions(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
        )
        .await
        .unwrap();
        let nested_item = listed.iter().find(|item| item.id == nested_id).unwrap();
        assert_eq!(
            nested_item.title, "First question",
            "later source objectives and answers must not enter the fork preview"
        );
        assert_eq!(
            nested.inherited_title.as_deref(),
            Some(nested_item.title.as_str())
        );
        assert_eq!(nested_item.defaults.model.as_deref(), Some("test-model"));
        rename_session(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
            nested_id,
            "New Session".into(),
        )
        .await
        .unwrap();
        let renamed = resume_session(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
            nested_id,
        )
        .await
        .unwrap();
        assert!(renamed.title_is_explicit);
        assert_eq!(
            renamed.conversation_title(),
            "New Session",
            "a manual placeholder-looking name remains explicit after reopen"
        );
        let relisted = list_sessions(
            StateRoot::open_existing(&state_path).unwrap(),
            workspace.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            relisted
                .iter()
                .find(|item| item.id == nested_id)
                .unwrap()
                .title,
            renamed.conversation_title()
        );

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("fault injection connection");
        connection
            .execute(
                "UPDATE events SET payload=?2 WHERE session_id=?1 AND kind='SessionStarted'",
                rusqlite::params![
                    first.session_id.to_string(),
                    Event::SessionStarted {
                        title: "Altered".into(),
                        workspace_identity: resumed.workspace_identity.unwrap(),
                    }
                    .payload()
                    .unwrap()
                ],
            )
            .expect("altered source prefix");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after drift");
        assert!(matches!(
            store.load_session(fork_id).await,
            Err(arany::StoreError::InvalidHistory)
        ));
        for id in [earlier_id, owner_id] {
            assert!(matches!(
                store.load_view(id).await,
                Err(arany::StoreError::InvalidHistory)
            ));
        }
        store.close().await.expect("store shutdown after drift");

        let provider = ScriptedProvider::new(Vec::new());
        let attempted_calls = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine after drift");
        let mut rejected = request(&workspace, "Must not disclose");
        rejected.session_id = Some(fork_id);
        rejected.title = None;
        assert!(matches!(
            run_to_end(&engine, &provider, rejected).await,
            Err(EngineError::Store(arany::StoreError::InvalidHistory))
        ));
        assert!(attempted_calls.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown after drift");
    });
}

#[tokio::test]
async fn list_sessions_page_rejects_zero_limit_before_workspace_or_store_access() {
    let temp = tempfile::tempdir().expect("temporary root");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).expect("private state");
    let store_before = std::fs::metadata(state_path.join("events.sqlite3"))
        .ok()
        .map(|m| m.len());
    assert!(matches!(
        arany::list_sessions_page(state, temp.path().join("missing-workspace"), None, 0).await,
        Err(EngineError::InvalidRequest)
    ));
    assert_eq!(
        std::fs::metadata(state_path.join("events.sqlite3"))
            .ok()
            .map(|m| m.len()),
        store_before
    );
}

#[tokio::test]
async fn create_session_on_open_engine_commits_session_started_and_submit_appends_to_it() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let engine = Engine::open(StateRoot::admit(&state).unwrap(), EngineConfig::default()).unwrap();
    for title in [String::new(), "x".repeat(129)] {
        assert!(matches!(
            engine.create_session(workspace.clone(), Some(title)).await,
            Err(EngineError::InvalidRequest)
        ));
    }
    let mut tap = engine.subscribe();
    let session = engine
        .create_session(workspace.clone(), Some("Explicit".into()))
        .await
        .unwrap();
    assert!(
        matches!(tokio::time::timeout(Duration::from_secs(10), tap.next()).await.unwrap(), Some(arany::Tapped::Committed(e)) if e.session_id == session && matches!(&e.event, Event::SessionStarted { title,.. } if title == "Explicit"))
    );
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: "Hello".into(),
        },
    )]));
    let mut input = request(&workspace, "Hi");
    input.include_paths.clear();
    input.session_id = Some(session);
    input.title = None;
    let outcome = run_to_end(&engine, &provider, input).await.unwrap();
    assert_eq!(outcome.session_id, session);
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::SessionStarted { .. }))
            .count(),
        1
    );
    assert_eq!(view.title, "Explicit");
    assert_eq!(view.runs[0].id, outcome.run.id);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}
