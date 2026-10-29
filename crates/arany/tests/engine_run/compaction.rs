use super::*;

struct WaitingCompactionProvider {
    started: mpsc::UnboundedSender<()>,
    dropped: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    compact_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

#[async_trait::async_trait]
impl Provider for WaitingCompactionProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "test-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: ProviderOutcome::Answer(Answer {
                text: "Seed answer".into(),
            }),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact_requests
            .lock()
            .expect("compaction requests")
            .push(request);
        let _drop_counter = DropCounter(Arc::clone(&self.dropped));
        self.started
            .send(())
            .map_err(|_| ProviderError::Unavailable)?;
        std::future::pending().await
    }
}

fn subscription_replay_provider(account_id: Uuid, fingerprint: [u8; 32]) -> Arc<ScriptedProvider> {
    let cap = arany::ChatGptProvider::from_checked_account(
        "model-a".into(),
        Effort::High,
        "synthetic-subscription-token".into(),
        ChatGptProvenance {
            account_id,
            evidence_fingerprint: fingerprint,
            admission: ChatGptAdmission::AccountConsent,
        },
    )
    .expect("synthetic account")
    .identity()
    .limits
    .max_output_tokens;
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "Synthetic answer".into(),
    })])
    .with_chatgpt_account(account_id, fingerprint)
    .with_max_output_tokens(cap)
    .with_reported_usage(arany::Usage {
        input_tokens: Some(20),
        output_tokens: Some(cap + 9_000),
        ..Default::default()
    })
    .with_compactions(vec![Ok(CompactionResponse {
        summary: "Compacted synthetic history".into(),
        response_id: Some("resp_compact".into()),
        usage: arany::Usage {
            input_tokens: Some(20),
            output_tokens: Some(3_000),
            ..Default::default()
        },
        wire_provenance: Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested),
    })]);
    Arc::new(provider)
}

async fn assert_subscription_replay(state_path: &Path, outcome: &arany::RunOutcome) {
    let config = outcome.run.config.as_ref().expect("Run provenance");
    let store = Store::open_read_only(StateRoot::open_existing(state_path).unwrap())
        .expect("read-only reopen");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("canonical Events");
    let view = SessionView::replay(outcome.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.runs[0].config.as_ref(), Some(config));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::ContextCompacted { record, .. }
            if record.chatgpt_provenance == config.chatgpt_provenance
                && record.output_token_bound == OutputTokenBound::LocalAcceptanceOnly
                && record.output_tokens == Some(3_000)
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::ProviderCallRecorded { record, .. } if record.output_tokens == Some(config.output_token_cap + 9_000)
    )));
    store.close().await.expect("store shutdown");
}

fn assert_subscription_account(config: &arany::RunConfig, account_id: Uuid, fingerprint: [u8; 32]) {
    assert_eq!(config.saved_api_account_id, None);
    assert_eq!(
        config.output_token_bound,
        OutputTokenBound::LocalAcceptanceOnly
    );
    assert_eq!(
        config.chatgpt_provenance,
        Some(ChatGptProvenance {
            account_id,
            evidence_fingerprint: fingerprint,
            admission: ChatGptAdmission::AccountConsent,
        })
    );
}

#[test]
fn subscription_account_and_local_bound_survive_run_and_compaction_replay() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Synthetic fact");
        let state_path = temp.path().join("state");
        let account_id = Uuid::now_v7();
        let fingerprint = [7; 32];
        let provider = subscription_replay_provider(account_id, fingerprint);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let outcome = run_to_end(
            &engine,
            &provider,
            request(&workspace, "Synthetic objective"),
        )
        .await
        .expect("committed Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let config = outcome.run.config.as_ref().expect("Run provenance");
        assert_eq!(config.provider.profile(), "chatgpt");
        assert_eq!(
            config.output_token_cap,
            provider.identity().limits.max_output_tokens
        );
        assert_eq!(
            provider.requests.lock().unwrap()[0].max_output_tokens,
            config.output_token_cap
        );
        assert_subscription_account(config, account_id, fingerprint);
        let compaction = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                outcome.session_id,
                workspace,
            )
            .await
            .expect("committed compaction");
        assert!(matches!(
            compaction.status,
            CompactionStatus::Succeeded { .. }
        ));
        assert_eq!(compaction.chatgpt_provenance, config.chatgpt_provenance);
        assert_eq!(
            compaction.output_token_bound,
            OutputTokenBound::LocalAcceptanceOnly
        );
        engine.close().await.expect("engine shutdown");

        assert_subscription_replay(&state_path, &outcome).await;
    });
}

#[test]
fn a_tampered_compaction_reference_fails_load() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "First answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Second answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Derived summary".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let first = run_to_end(&engine, &provider, request(&workspace, "First question"))
            .await
            .expect("first Run");
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("compaction");
        let mut next = request(&workspace, "Second question");
        next.session_id = Some(first.session_id);
        next.title = None;
        run_to_end(&engine, &provider, next)
            .await
            .expect("Run after compaction");
        engine.close().await.expect("engine shutdown");
        let store =
            Store::open(StateRoot::admit(&state_path).expect("state readmission")).expect("store");
        let view = store
            .load_view(first.session_id)
            .await
            .expect("honest replay")
            .expect("Session");
        let reference = view
            .runs
            .last()
            .and_then(|run| run.config.as_ref())
            .and_then(|config| config.compaction_event_sequence)
            .expect("later Run names its compaction");
        store.close().await.expect("store shutdown");
        let connection =
            rusqlite::Connection::open(state_path.join("events.sqlite3")).expect("database");
        let changed = connection
            .execute(
                "UPDATE events SET payload = replace(payload, ?1, ?2)
                 WHERE kind = 'RunStarted' AND instr(payload, ?1) > 0",
                rusqlite::params![
                    format!("\"compaction_event_sequence\":{reference}"),
                    format!("\"compaction_event_sequence\":{}", reference + 1000),
                ],
            )
            .expect("tamper");
        assert_eq!(changed, 1);
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only store");
        assert!(store.load_view(first.session_id).await.is_err());
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn a_fork_before_the_snapshot_event_recompiles_from_canonical_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Source answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Child answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Source-only derived summary".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let observed = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let first = run_to_end(&engine, &provider, request(&workspace, "Source question"))
            .await
            .expect("source Run");
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("source compaction");
        let fork_id = engine
            .fork_session(first.session_id, workspace.clone(), None)
            .await
            .expect("fork at committed Run boundary");
        let mut branch_request = request(&workspace, "Branch question");
        branch_request.session_id = Some(fork_id);
        branch_request.title = None;
        let branch = run_to_end(&engine, &provider, branch_request)
            .await
            .expect("branch Run");
        assert!(
            branch
                .run
                .config
                .as_ref()
                .expect("config")
                .compaction_event_sequence
                .is_none()
        );
        {
            let requests = observed.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            assert!(requests[1].input.context_summary.is_none());
            assert_eq!(requests[1].input.history.len(), 1);
            assert_eq!(requests[1].input.history[0].assistant, "Source answer");
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let branch_view = store
            .load_view(fork_id)
            .await
            .expect("branch replay")
            .expect("branch exists");
        assert!(branch_view.compactions.is_empty());
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn invalid_compaction_snapshot_corpus_fails_before_provider_disclosure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for fault in [
            CompactionFault::SourceDigest,
            CompactionFault::ContentDigest,
            CompactionFault::CompilerVersion,
        ] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = facts_workspace(temp.path(), "Fact");
            let state_path = temp.path().join("state");
            let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
                text: "Source answer".into(),
            })])
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "Derived summary".into(),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            })]);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&state_path).expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let first = run_to_end(&engine, &provider, request(&workspace, "Source question"))
                .await
                .expect("source Run");
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    first.session_id,
                    workspace.clone(),
                )
                .await
                .expect("source compaction");
            engine.close().await.expect("engine shutdown");

            let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
                .expect("fault injection connection");
            let payload: String = connection
                .query_row(
                    "SELECT payload FROM events WHERE session_id=?1 AND kind='ContextCompacted'",
                    [first.session_id.to_string()],
                    |row| row.get(0),
                )
                .expect("snapshot payload");
            let mut payload: serde_json::Value =
                serde_json::from_str(&payload).expect("valid snapshot JSON");
            let document = &mut payload["record"];
            corrupt_compaction_document(document, fault);
            connection
                .execute(
                    "UPDATE events SET payload=?1 WHERE session_id=?2 AND kind='ContextCompacted'",
                    [payload.to_string(), first.session_id.to_string()],
                )
                .expect("injected invalid snapshot");
            drop(connection);

            let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
                .expect("reopened store");
            assert!(
                matches!(
                    store.load_session(first.session_id).await,
                    Err(arany::StoreError::InvalidSnapshot)
                ),
                "fault: {fault:?}"
            );
            store.close().await.expect("store shutdown");

            let provider = ScriptedProvider::new(Vec::new());
            let calls = Arc::clone(&provider.requests);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&state_path).expect("state readmission"),
                EngineConfig::default(),
            )
            .expect("engine after fault");
            let mut rejected = request(&workspace, "Must not disclose");
            rejected.session_id = Some(first.session_id);
            rejected.title = None;
            assert!(
                matches!(
                    run_to_end(&engine, &provider, rejected).await,
                    Err(EngineError::Store(arany::StoreError::InvalidSnapshot))
                ),
                "fault: {fault:?}"
            );
            assert!(calls.lock().expect("request lock").is_empty());
            engine.close().await.expect("engine shutdown after fault");
        }
    });
}

#[derive(Clone, Copy, Debug)]
enum CompactionFault {
    SourceDigest,
    ContentDigest,
    CompilerVersion,
}

fn corrupt_compaction_document(document: &mut serde_json::Value, fault: CompactionFault) {
    match fault {
        CompactionFault::SourceDigest => {
            let value = document["source_digest"][0].as_u64().expect("digest byte");
            document["source_digest"][0] = (value ^ 1).into();
        }
        CompactionFault::ContentDigest => {
            let value = document["status"]["content_digest"][0]
                .as_u64()
                .expect("digest byte");
            document["status"]["content_digest"][0] = (value ^ 1).into();
        }
        CompactionFault::CompilerVersion => {
            let version = document["compiler_version"]
                .as_u64()
                .expect("compiler version");
            document["compiler_version"] = (version + 1).into();
        }
    }
}

#[test]
fn compaction_input_limit_rejects_before_provider_egress() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "x".repeat(32 * 1024)
            });
            10
        ]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut session_id = None;
        for _ in 0..10 {
            let mut next = request(&workspace, &"Q".repeat(8 * 1024));
            next.session_id = session_id;
            if session_id.is_some() {
                next.title = None;
            }
            session_id = Some(
                run_to_end(&engine, &provider, next)
                    .await
                    .expect("bounded Run")
                    .session_id,
            );
        }
        let session_id = session_id.expect("Session exists");
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    session_id,
                    workspace.clone()
                )
                .await,
            Err(EngineError::CompactionInputTooLarge)
        ));
        assert!(compact_requests.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(session_id)
            .await
            .expect("durable Events");
        assert_eq!(events.len(), 111);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalStarted { .. }))
                .count(),
            10
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
                .count(),
            10
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ContextCompacted { .. }))
        );
        store.close().await.expect("store shutdown");

        let maintained_path = temp.path().join("maintained-state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "x".repeat(32 * 1024)
            });
            16
        ])
        .with_compactions(
            (0..3)
                .map(|_| {
                    Ok(CompactionResponse {
                        summary: "s".repeat(8 * 1024),
                        response_id: None,
                        usage: arany::Usage {
                            input_tokens: Some(100),
                            output_tokens: Some(1024),
                            ..Default::default()
                        },
                        wire_provenance: None,
                    })
                })
                .collect(),
        );
        let requests = Arc::clone(&provider.requests);
        let compactions = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&maintained_path).unwrap(),
            EngineConfig::default(),
        )
        .expect("maintained Engine");
        let mut session_id = None;
        let mut maintenance_sequences = Vec::new();
        // The automatic threshold is 158_080, so 8 KiB-draft turns trigger maintenance every five turns and each cycle fits the input bound.
        for index in 0..16 {
            let objective = format!("{index:02}{}", "Q".repeat(8 * 1024 - 2));
            let mut next = request(&workspace, &objective);
            next.session_id = session_id;
            if session_id.is_some() {
                next.title = None;
            }
            let outcome = run_to_end(&engine, &provider, next)
                .await
                .expect("maintained bounded Run");
            session_id = Some(outcome.session_id);
            let config = outcome.run.config.as_ref().expect("context footprint");
            assert_eq!(
                config.auto_compaction_due(),
                matches!(index, 4 | 9 | 14),
                "maintenance must precede an oversized prefix at turn {}",
                index + 1
            );
            if matches!(index, 5 | 10 | 15) {
                assert_eq!(
                    config.compaction_event_sequence,
                    maintenance_sequences.last().copied()
                );
                let requests = requests.lock().expect("observed requests");
                let request = requests.last().expect("post-maintenance request");
                assert_eq!(
                    request.input.context_summary.as_deref(),
                    Some("s".repeat(8 * 1024).as_str())
                );
                assert!(
                    request.input.history.is_empty(),
                    "summary replaces covered context, not canonical history"
                );
            }
            if config.auto_compaction_due() {
                let compacted = engine
                    .auto_compact_session(
                        &AdmittedProviders::single(provider.clone()),
                        outcome.session_id,
                        workspace.clone(),
                        outcome.run.id,
                    )
                    .await
                    .expect("automatic input remains admissible")
                    .expect("new boundary");
                assert_eq!(compacted.covered_run_id, outcome.run.id);
                assert!(compacted.source_bytes <= 256 * 1024);
                assert!(matches!(
                    compacted.status,
                    CompactionStatus::Succeeded { .. }
                ));
                let observer =
                    Store::open_read_only(StateRoot::open_existing(&maintained_path).unwrap())
                        .expect("committed maintenance observer");
                let events = observer
                    .load_session(outcome.session_id)
                    .await
                    .expect("committed maintenance");
                maintenance_sequences.push(events.last().expect("summary Event").sequence);
                observer.close().await.expect("close maintenance observer");
            }
        }
        let session_id = session_id.expect("maintained Session");
        assert_eq!(maintenance_sequences.len(), 3);
        {
            let requests = requests.lock().expect("observed Run requests");
            assert_eq!(requests.len(), 16);
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(request.phase, AgentPhase::RootPlan);
                assert_eq!(request.model, "test-model");
                assert_eq!(request.max_output_tokens, 4096);
                assert_eq!(
                    request.input.objective,
                    format!("{index:02}{}", "Q".repeat(8 * 1024 - 2))
                );
                assert_eq!(
                    request.input.includes,
                    [include_data("notes/facts.txt", "Fact")]
                );
                assert!(request.input.workspace_guidance.is_empty());
                assert!(request.input.child_results.is_empty());
            }
            let compactions = compactions.lock().expect("observed compactions");
            assert_eq!(compactions.len(), 3);
            for (cycle, request) in compactions.iter().enumerate() {
                assert_eq!(request.model, "test-model");
                assert_eq!(request.max_output_tokens, 4096);
                assert_eq!(request.previous_summary.is_some(), cycle > 0);
                assert_eq!(request.items.len(), 5);
                for (offset, item) in request.items.iter().enumerate() {
                    let CompactionItem::Completed(turn) = item else {
                        panic!("completed source turn");
                    };
                    assert_eq!(
                        turn.user,
                        format!("{:02}{}", cycle * 5 + offset, "Q".repeat(8 * 1024 - 2))
                    );
                    assert_eq!(turn.assistant, "x".repeat(32 * 1024));
                }
            }
        }
        engine.close().await.expect("maintained Engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&maintained_path).unwrap())
            .expect("closed maintained Store");
        let events = store
            .load_session(session_id)
            .await
            .expect("maintained Events");
        let view = SessionView::replay(session_id, &events)
            .expect("strict maintained replay")
            .expect("Session");
        assert_eq!(view.runs.len(), 16);
        assert_eq!(view.compactions.len(), 3);
        for (index, run) in view.runs.iter().enumerate() {
            assert_eq!(run.status, RunStatus::Finished);
            assert_eq!(
                run.objective,
                format!("{index:02}{}", "Q".repeat(8 * 1024 - 2))
            );
            assert_eq!(
                run.assistant_message.as_deref(),
                Some("x".repeat(32 * 1024).as_str())
            );
        }
        store.close().await.expect("close maintained replay");
    });
}

#[test]
fn oversized_compaction_output_is_recorded_as_failure_without_a_snapshot() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Source answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Later answer".into(),
            }),
        ])
        .with_compactions(vec![
            Ok(CompactionResponse {
                summary: "z".repeat(64 * 1024 + 1),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(1024),
                    ..Default::default()
                },
                wire_provenance: None,
            }),
            Ok(CompactionResponse {
                summary: "safe summary".into(),
                response_id: Some("bad\nidentifier".into()),
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            }),
        ]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let first = run_to_end(&engine, &provider, request(&workspace, "Source question"))
            .await
            .expect("source Run");
        let failed = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("durable failed compaction");
        assert!(matches!(
            failed.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::InvalidOutcome
            }
        ));
        let mut later_request = request(&workspace, "Later question");
        later_request.session_id = Some(first.session_id);
        later_request.title = None;
        run_to_end(&engine, &provider, later_request)
            .await
            .expect("later Run");
        let hostile_id = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                first.session_id,
                workspace.clone(),
            )
            .await
            .expect("durable hostile-ID failure");
        assert!(matches!(
            hostile_id.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::InvalidOutcome
            }
        ));
        assert_eq!(hostile_id.response_id, None);
        {
            let observed = requests.lock().expect("request lock");
            assert_eq!(observed.len(), 2);
            assert!(observed[1].input.context_summary.is_none());
            assert_eq!(observed[1].input.history.len(), 1);
        }
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let view = store
            .load_view(first.session_id)
            .await
            .expect("replay")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.compactions.len(), 2);
        assert!(
            view.compactions.iter().all(|compaction| matches!(
                compaction.record.status,
                CompactionStatus::Failed { .. }
            ))
        );
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn compaction_preserves_unanswered_accepted_objectives_as_untrusted_data() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let provider =
            ScriptedProvider::new((0..3).map(|_| delegate(&[("not allowed", None)])).collect())
                .with_compactions(vec![Ok(CompactionResponse {
                    summary: "Question remains unresolved".into(),
                    response_id: None,
                    usage: arany::Usage {
                        input_tokens: None,
                        output_tokens: Some(8),
                        ..Default::default()
                    },
                    wire_provenance: None,
                })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let failed = run_to_end(
            &engine,
            &provider,
            request(&workspace, "Unanswered question"),
        )
        .await
        .expect("durable failed Run");
        assert_eq!(failed.run.status, RunStatus::Failed);
        {
            use std::os::unix::fs::PermissionsExt;
            let prompts = state_path.join("prompts");
            std::fs::create_dir(&prompts).expect("prompt directory");
            std::fs::set_permissions(&prompts, std::fs::Permissions::from_mode(0o700))
                .expect("prompt directory mode");
            let file = prompts.join("compaction.md");
            std::fs::write(&file, "Keep it short.").expect("compaction override");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .expect("compaction override mode");
        }
        let record = engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                failed.session_id,
                workspace,
            )
            .await
            .expect("compaction over failed Run");
        assert_eq!(record.covered_run_id, failed.run.id);
        assert_eq!(record.prompt.source, arany::PromptSource::Override);
        assert_eq!(
            record.prompt.digest,
            <[u8; 32]>::from(Sha256::digest(b"Keep it short."))
        );
        {
            let requests = compact_requests.lock().expect("compaction request lock");
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].prompts.text(arany::PromptFile::Compaction),
                "Keep it short."
            );
            assert!(matches!(
                &requests[0].items[..],
                [CompactionItem::Unanswered { user, status: UnansweredStatus::Failed }]
                    if user == "Unanswered question"
            ));
        }
        engine.close().await.expect("engine shutdown");
    });
}

#[test]
fn an_earlier_prestart_interruption_does_not_block_later_compaction_or_fork() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let session_id = arany::create_session(
            StateRoot::admit(&state_path).expect("private state"),
            workspace.clone(),
            Some("Interrupted Session".into()),
        )
        .await
        .expect("pinned Session start");
        use std::os::unix::fs::MetadataExt;
        let workspace_metadata = std::fs::metadata(&workspace).expect("Workspace metadata");
        let workspace_identity = arany::WorkspaceIdentity {
            device: workspace_metadata.dev(),
            inode: workspace_metadata.ino(),
        };
        let goal_id = arany::GoalId::new();
        let interrupted_run_id = arany::RunId::new();
        let store =
            Store::open(StateRoot::admit(&state_path).expect("private state")).expect("store");
        store
            .append(
                session_id,
                Event::GoalStarted {
                    include_paths: Some(Vec::new()),
                    goal_id,
                    objective: "Unfinished question".into(),
                    workspace_identity,
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
            )
            .await
            .expect("Goal start");
        store
            .append(
                session_id,
                Event::GoalRunLinked {
                    goal_id,
                    run_id: interrupted_run_id,
                    phase: arany::GoalPhase::Implementation,
                },
            )
            .await
            .expect("interrupted Run link");
        store
            .append(
                session_id,
                Event::MessageAccepted {
                    skill: None,
                    run_id: interrupted_run_id,
                    text: "Unfinished question".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("accepted objective");
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "Finished answer".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Follow-up answer".into(),
            }),
        ])
        .with_compactions(vec![Ok(CompactionResponse {
            summary: "Unfinished then finished".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })]);
        let compact_requests = Arc::clone(&provider.compact_requests);
        let observed = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut resumed = request(&workspace, "Finished question");
        resumed.session_id = Some(session_id);
        resumed.title = None;
        resumed.goal = arany::GoalRequest::resume(goal_id, true);
        run_to_end(&engine, &provider, resumed)
            .await
            .expect("completed later Run");
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                session_id,
                workspace.clone(),
            )
            .await
            .expect("compaction after interruption");
        {
            let requests = compact_requests.lock().expect("compaction request lock");
            assert!(matches!(
                &requests[0].items[0],
                CompactionItem::Unanswered {
                    user,
                    status: UnansweredStatus::Interrupted
                } if user == "Unfinished question"
            ));
        }
        let fork_id = engine
            .fork_session(session_id, workspace.clone(), None)
            .await
            .expect("fork after interruption");
        let mut follow_up = request(&workspace, "Question after compaction");
        follow_up.session_id = Some(session_id);
        follow_up.title = None;
        run_to_end(&engine, &provider, follow_up)
            .await
            .expect("Run after compaction");
        {
            let requests = observed.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1].input.context_summary.as_deref(),
                Some("Unfinished then finished")
            );
            assert!(requests[1].input.history.is_empty());
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let fork = store
            .load_view(fork_id)
            .await
            .expect("fork replay")
            .expect("fork exists");
        assert_eq!(fork.runs.len(), 1);
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn compaction_timeout_drops_the_call_and_records_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let compact_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = WaitingCompactionProvider {
            started,
            dropped: Arc::clone(&dropped),
            requests: Arc::clone(&requests),
            compact_requests: Arc::clone(&compact_requests),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let session_id = run_to_end(&engine, &provider, request(&workspace, "Seed question"))
            .await
            .expect("seed Run")
            .session_id;
        let mut compact_task = tokio::spawn(async move {
            let outcome = engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    session_id,
                    workspace,
                )
                .await;
            let close = engine.close().await;
            (outcome, close)
        });
        let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
        assert_eq!(started.expect("Provider start deadline"), Some(()));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        tokio::time::resume();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut compact_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("compaction task"),
            Err(_) => {
                compact_task.abort();
                let _ = compact_task.await;
                panic!("timed-out compaction call did not terminate");
            }
        };
        let record = outcome.expect("durable failed compaction");
        close.expect("engine shutdown");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        {
            let calls = requests.lock().expect("requests");
            assert_eq!(calls.len(), 1, "exactly one seed call");
            assert_test_call(
                &calls[0],
                AgentPhase::RootPlan,
                "Seed question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            let compacts = compact_requests.lock().expect("compaction requests");
            assert_eq!(compacts.len(), 1, "exactly one timed-out compaction");
            let compact = &compacts[0];
            assert_eq!(compact.session_id, session_id);
            assert_eq!(compact.covered_run_id, calls[0].run_id);
            assert!(compact.model == "test-model", "compaction model");
            assert!(compact.previous_summary.is_none(), "no previous summary");
            assert!(
                compact.items
                    == [CompactionItem::Completed(arany::HistoryTurn {
                        user: "Seed question".into(),
                        assistant: "Seed answer".into(),
                    })],
                "exact compaction source"
            );
            assert_eq!(compact.max_output_tokens, 4096);
        }
        assert!(matches!(
            record.status,
            CompactionStatus::Failed {
                reason: CompactionFailure::TimedOut
            }
        ));
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let view = store
            .load_view(session_id)
            .await
            .expect("durable replay")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.compactions.len(), 1);
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn escape_heavy_session_over_the_compaction_bound_fails_closed_and_keeps_its_journal() {
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
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        // Each control character serializes to six bytes, so this objective exceeds the compaction bound.
        let session_id = run_to_end(
            &engine,
            &provider,
            request(&workspace, &"\u{1}".repeat(64 * 1024)),
        )
        .await
        .expect("bounded Run")
        .session_id;
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let before = store
            .load_session(session_id)
            .await
            .expect("durable Events")
            .len();
        store.close().await.expect("store shutdown");

        let provider = ScriptedProvider::new(Vec::new());
        let compact_requests = Arc::clone(&provider.compact_requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state readmission"),
            EngineConfig::default(),
        )
        .expect("engine");
        assert!(matches!(
            engine
                .compact_session(
                    &AdmittedProviders::single(provider.clone()),
                    session_id,
                    workspace.clone()
                )
                .await,
            Err(EngineError::CompactionInputTooLarge)
        ));
        assert!(compact_requests.lock().expect("request lock").is_empty());
        engine.close().await.expect("engine shutdown");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let after = store
            .load_session(session_id)
            .await
            .expect("durable Events")
            .len();
        assert_eq!(after, before, "a refused compaction appends no Event");
        store.close().await.expect("store shutdown");
    });
}
