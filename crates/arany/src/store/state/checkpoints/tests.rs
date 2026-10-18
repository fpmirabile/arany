use super::*;

fn fixture() -> (tempfile::TempDir, StateRoot) {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).unwrap();
    (temp, root)
}
struct Batch {
    capture: Capture,
    sequence: u64,
}
impl std::ops::Deref for Batch {
    type Target = Capture;
    fn deref(&self) -> &Capture {
        &self.capture
    }
}
impl std::ops::DerefMut for Batch {
    fn deref_mut(&mut self) -> &mut Capture {
        &mut self.capture
    }
}
fn batch(session: SessionId, goal: GoalId, run: RunId, sequence: u64, content: &[u8]) -> Batch {
    Batch {
        capture: Capture {
            session,
            goal,
            run,
            intent_id: uuid::Uuid::now_v7(),
            files: vec![("file".into(), content.to_vec())],
        },
        sequence,
    }
}
fn capture(root: &StateRoot, batch: impl std::borrow::Borrow<Batch>, now: i64) -> Checkpoint {
    let batch = batch.borrow();
    super::capture(
        root,
        clone_capture(&batch.capture),
        batch.sequence,
        now,
        &mut None,
    )
}
fn clone_capture(capture: &Capture) -> Capture {
    Capture {
        session: capture.session,
        goal: capture.goal,
        run: capture.run,
        intent_id: capture.intent_id,
        files: capture.files.clone(),
    }
}
fn capture_inner(
    root: &StateRoot,
    batch: &Batch,
    now: i64,
    limits: Limits,
) -> Result<Option<RunId>, StoreError> {
    super::capture_inner(root, &batch.capture, batch.sequence, now, limits, &mut None).map(
        |pruned| {
            pruned
                .get(&batch.session.to_string())
                .and_then(|run| run.parse().ok())
        },
    )
}
fn load(root: &StateRoot, digest: [u8; 32]) -> Result<Option<Vec<u8>>, StoreError> {
    super::load(root, digest, &mut None)
}
fn sweep(root: &StateRoot, now: i64) -> Result<Pruned, StoreError> {
    super::sweep(root, now, &mut None)
}
fn count(root: &StateRoot, sql: &str) -> i64 {
    scalar(&open(root, false, LIMITS).unwrap(), sql, []).unwrap()
}

#[test]
fn checkpoints_deduplicate_verify_digests_and_commit_before_reopen() {
    let (_temp, root) = fixture();
    let session = SessionId::new();
    let goal = GoalId::new();
    for sequence in 1..=2 {
        assert_eq!(
            capture(
                &root,
                batch(session, goal, RunId::new(), sequence, b"original"),
                100
            )
            .status,
            CheckpointStatus::Captured
        );
    }
    assert_eq!(count(&root, "SELECT COUNT(*) FROM blobs"), 1);
    assert_eq!(count(&root, "SELECT COUNT(*) FROM refs"), 2);
    let digest: [u8; 32] = Sha256::digest(b"original").into();
    assert_eq!(
        load(&root, digest).unwrap().as_deref(),
        Some(b"original".as_slice())
    );
    let connection = open(&root, true, LIMITS).unwrap();
    connection
        .execute("UPDATE blobs SET bytes=?1", [b"tampered".as_slice()])
        .unwrap();
    drop(connection);
    assert_eq!(load(&root, digest).unwrap(), None);
    assert_eq!(
        capture(
            &root,
            batch(session, goal, RunId::new(), 3, b"original"),
            100
        )
        .reason,
        Some(CheckpointReason::StoreUnavailable)
    );
}

#[test]
fn checkpoints_every_logical_quota_accepts_boundary_and_evicts_first_overflow() {
    for dimension in [
        "path_versions",
        "path_bytes",
        "goal_bytes",
        "session_bytes",
        "session_checkpoints",
        "refs",
    ] {
        let (_temp, root) = fixture();
        let mut limits = LIMITS;
        match dimension {
            "path_versions" => limits.path_versions = 2,
            "path_bytes" => limits.path_bytes = 2,
            "goal_bytes" => limits.goal_bytes = 2,
            "session_bytes" => limits.session_bytes = 2,
            "session_checkpoints" => limits.session_checkpoints = 2,
            "refs" => limits.refs = 2,
            _ => unreachable!(),
        }
        let session = SessionId::new();
        let goal = GoalId::new();
        let mut oldest = None;
        for sequence in 1..=3 {
            let captured = batch(session, goal, RunId::new(), sequence, &[sequence as u8]);
            if sequence == 1 {
                oldest = Some(captured.run);
            }
            assert_eq!(
                capture_inner(&root, &captured, sequence as i64, limits).unwrap(),
                if sequence == 3 { oldest } else { None },
                "{dimension}: notice names the pruned checkpoint"
            );
            assert_eq!(
                count(&root, "SELECT COUNT(*) FROM refs"),
                sequence.min(2) as i64,
                "{dimension}"
            );
        }
        assert_eq!(
            count(&root, "SELECT MIN(tool_sequence) FROM refs"),
            2,
            "{dimension}: oldest first"
        );
    }
}

#[test]
fn checkpoints_current_run_and_whole_publication_are_protected() {
    let (_temp, root) = fixture();
    let session = SessionId::new();
    let goal = GoalId::new();
    let run = RunId::new();
    let limits = Limits {
        goal_bytes: 1,
        ..LIMITS
    };
    capture_inner(&root, &batch(session, goal, run, 1, b"a"), 1, limits).unwrap();
    let mut next = batch(session, goal, run, 2, b"b");
    next.files[0].0 = "other".into();
    assert!(matches!(
        capture_inner(&root, &next, 2, limits),
        Err(StoreError::StorageFull)
    ));
    assert_eq!(count(&root, "SELECT COUNT(*) FROM refs"), 1);
    assert_eq!(count(&root, "SELECT MIN(tool_sequence) FROM refs"), 1);
    let empty = SessionId::new();
    next.session = empty;
    next.run = RunId::new();
    next.goal = GoalId::new();
    next.files = vec![("a".into(), vec![1]), ("b".into(), vec![2])];
    let limits = Limits { refs: 1, ..LIMITS };
    assert!(matches!(
        capture_inner(&root, &next, 2, limits),
        Err(StoreError::StorageFull)
    ));
    assert_eq!(
        count(&root, "SELECT COUNT(*) FROM refs"),
        1,
        "failed batch rolls back eviction and insertion"
    );
}

#[test]
fn checkpoints_exact_file_and_path_boundaries_and_physical_overflow() {
    let (_temp, root) = fixture();
    let session = SessionId::new();
    let goal = GoalId::new();
    let bytes = vec![0; MAX_VERSION];
    for sequence in 1..=33 {
        let result = capture(
            &root,
            batch(session, goal, RunId::new(), sequence, &bytes),
            sequence as i64,
        );
        assert_eq!(result.status, CheckpointStatus::Captured);
        assert!(
            result.pruned_before_run.is_none(),
            "deduplicated bytes remain available after ref eviction"
        );
    }
    assert_eq!(count(&root, "SELECT COUNT(*) FROM refs"), 32);
    assert_eq!(
        capture(
            &root,
            batch(session, goal, RunId::new(), 34, &vec![0; MAX_VERSION + 1]),
            34
        )
        .reason,
        Some(CheckpointReason::TooLarge)
    );
    let limits = Limits {
        bytes: 64 * 1024,
        ..LIMITS
    };
    let (_other, small) = fixture();
    assert!(matches!(
        capture_inner(
            &small,
            &batch(session, goal, RunId::new(), 1, &bytes),
            1,
            limits
        ),
        Err(StoreError::StorageFull)
    ));
    assert!(count(&root, "PRAGMA page_count") * 4096 <= LIMITS.bytes);
}

#[test]
fn checkpoints_retention_is_session_relative_including_future_dates() {
    let (_temp, root) = fixture();
    let session = SessionId::new();
    let goal = GoalId::new();
    capture(&root, batch(session, goal, RunId::new(), 1, b"one"), 0);
    capture(
        &root,
        batch(session, goal, RunId::new(), 2, b"two"),
        RETENTION_MS,
    );
    sweep(&root, RETENTION_MS * 2).unwrap();
    assert_eq!(
        count(&root, "SELECT COUNT(*) FROM refs"),
        2,
        "exact retention boundary"
    );
    sweep(&root, RETENTION_MS * 2 + 1).unwrap();
    assert_eq!(count(&root, "SELECT COUNT(*) FROM refs"), 0);
    assert_eq!(count(&root, "SELECT COUNT(*) FROM blobs"), 0);
    capture(&root, batch(session, goal, RunId::new(), 3, b"future"), 100);
    sweep(&root, 99).unwrap();
    assert_eq!(count(&root, "SELECT COUNT(*) FROM refs"), 0);
}

#[test]
fn checkpoints_total_eviction_uses_least_recent_session_before_oldest_ref() {
    let (_temp, root) = fixture();
    let a = SessionId::new();
    let b = SessionId::new();
    let goal = GoalId::new();
    capture_inner(&root, &batch(a, goal, RunId::new(), 1, b"a"), 1, LIMITS).unwrap();
    capture_inner(&root, &batch(b, goal, RunId::new(), 2, b"b"), 2, LIMITS).unwrap();
    capture_inner(&root, &batch(a, goal, RunId::new(), 3, b"c"), 3, LIMITS).unwrap();
    capture_inner(
        &root,
        &batch(a, goal, RunId::new(), 4, b"d"),
        4,
        Limits { refs: 3, ..LIMITS },
    )
    .unwrap();
    assert_eq!(
        count(&root, "SELECT COUNT(*) FROM refs WHERE tool_sequence=2"),
        0
    );
    assert_eq!(
        count(&root, "SELECT COUNT(*) FROM refs WHERE tool_sequence=1"),
        1
    );
}

#[test]
fn checkpoints_malformed_unsupported_busy_and_changed_reset_preserve_data() {
    for malformed in [false, true] {
        let (_temp, root) = fixture();
        let session = SessionId::new();
        let goal = GoalId::new();
        capture(&root, batch(session, goal, RunId::new(), 1, b"saved"), 1);
        if malformed {
            std::fs::write(root.path().join(FILE), b"malformed").unwrap();
        } else {
            open(&root, true, LIMITS)
                .unwrap()
                .execute_batch("PRAGMA user_version=2")
                .unwrap();
        }
        let before = std::fs::read(root.path().join(FILE)).unwrap();
        assert!(sweep(&root, RETENTION_MS + 2).is_err());
        assert_eq!(
            capture(&root, batch(session, goal, RunId::new(), 2, b"next"), 2).reason,
            Some(CheckpointReason::StoreUnavailable)
        );
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), before);
        let reset = prepare_reset(&root).unwrap();
        std::fs::write(root.path().join(FILE), b"changed").unwrap();
        assert!(matches!(reset.apply(), Err(StoreError::CheckpointChanged)));
        prepare_reset(&root).unwrap().apply().unwrap();
        assert_eq!(
            capture(&root, batch(session, goal, RunId::new(), 3, b"new"), 3).status,
            CheckpointStatus::Captured
        );
        let held = lock(&root).unwrap();
        assert_eq!(
            capture(&root, batch(session, goal, RunId::new(), 4, b"busy"), 4).reason,
            Some(CheckpointReason::StoreUnavailable)
        );
        drop(held);
        let mut blocker = open(&root, true, LIMITS).unwrap();
        let transaction = blocker
            .transaction_with_behavior(TransactionBehavior::Exclusive)
            .unwrap();
        assert_eq!(
            capture(&root, batch(session, goal, RunId::new(), 4, b"busy"), 4).reason,
            Some(CheckpointReason::StoreUnavailable)
        );
        drop(transaction);
    }
}

#[test]
fn checkpoints_pin_the_owner_accepted_bounds() {
    assert_eq!(MAX_VERSION, 1_048_576);
    assert_eq!((LIMITS.path_versions, LIMITS.path_bytes), (32, 33_554_432));
    assert_eq!(LIMITS.goal_bytes, 268_435_456);
    assert_eq!(
        (LIMITS.session_checkpoints, LIMITS.session_bytes),
        (100, 536_870_912)
    );
    assert_eq!((LIMITS.bytes, LIMITS.refs), (2_147_483_648, 262_144));
    assert_eq!((MAX_FILES, MAX_CAPTURE), (2048, 33_554_432));
    assert_eq!(RETENTION_MS, 2_592_000_000);
    assert_eq!(BUSY, Duration::from_millis(2000));
}

#[test]
fn checkpoints_evict_a_large_store_in_one_bounded_pass_and_name_only_this_session() {
    let (_temp, root) = fixture();
    let (foreign, session, goal) = (SessionId::new(), SessionId::new(), GoalId::new());
    let foreign_run = RunId::new();
    let runs: Vec<RunId> = (0..20_000).map(|_| RunId::new()).collect();
    {
        let connection = open(&root, true, LIMITS).unwrap();
        connection.execute_batch("BEGIN").unwrap();
        let mut insert = connection
            .prepare("INSERT INTO refs(session_id,goal_id,run_id,rewind_id,tool_sequence,path,digest,len,captured_at_ms) VALUES(?1,?2,?3,NULL,?4,?5,?6,1,?7)")
            .unwrap();
        for index in 0..20_000i64 {
            let digest = Sha256::digest(index.to_le_bytes());
            connection
                .execute(
                    "INSERT INTO blobs(digest,bytes,len) VALUES(?1,?2,1)",
                    params![digest.as_slice(), [0u8]],
                )
                .unwrap();
            insert
                .execute(params![
                    session.to_string(),
                    goal.to_string(),
                    runs[index as usize].to_string(),
                    index + 1,
                    format!("p{index}"),
                    digest.as_slice(),
                    RETENTION_MS + index
                ])
                .unwrap();
        }
        let digest = Sha256::digest(b"foreign");
        connection
            .execute(
                "INSERT INTO blobs(digest,bytes,len) VALUES(?1,?2,7)",
                params![digest.as_slice(), b"foreign".as_slice()],
            )
            .unwrap();
        insert
            .execute(params![
                foreign.to_string(),
                GoalId::new().to_string(),
                foreign_run.to_string(),
                1,
                "f",
                digest.as_slice(),
                0
            ])
            .unwrap();
        drop(insert);
        connection.execute_batch("COMMIT").unwrap();
    }
    let mut limits = LIMITS;
    limits.refs = 30_000;
    limits.session_checkpoints = 100;
    let started = Instant::now();
    let pruned = super::capture_inner(
        &root,
        &batch(session, goal, RunId::new(), 30_000, b"new").capture,
        30_000,
        RETENTION_MS * 2,
        limits,
        &mut None,
    )
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(count(&root, "SELECT COUNT(DISTINCT run_id) FROM refs"), 100);
    assert_eq!(
        pruned.get(&session.to_string()),
        Some(&runs[19_900].to_string())
    );
    assert_eq!(
        pruned.get(&foreign.to_string()),
        Some(&foreign_run.to_string()),
        "foreign expiry is kept per Session, never attributed to this one"
    );
}

#[test]
fn checkpoints_expiry_persists_even_when_the_capture_itself_fails() {
    let (_temp, root) = fixture();
    let (old, session, goal) = (SessionId::new(), SessionId::new(), GoalId::new());
    assert_eq!(
        capture(&root, batch(old, goal, RunId::new(), 1, b"old"), 1).status,
        CheckpointStatus::Captured
    );
    let mut failing = batch(session, goal, RunId::new(), 2, b"new");
    failing.files[0].0 = "../escape".into();
    assert!(capture_inner(&root, &failing, RETENTION_MS * 2, LIMITS).is_err());
    assert_eq!(count(&root, "SELECT COUNT(*) FROM refs"), 0);
}

#[tokio::test]
async fn checkpoints_prune_notices_survive_discarded_captures_until_journal_commit() {
    use crate::session::worker::{WorkerIntegrationOutcome, WorkerSettlementReason};
    use crate::session::{Event, SessionView};
    use crate::store::Store;
    use crate::tools::{Attestation, ToolCall, ToolDisposition};

    for startup in [false, true] {
        let (_temp, root) = fixture();
        let f = crate::session::worker_protocol_fixture();
        let now = i64::try_from(crate::tools::now_ms()).unwrap();
        for sequence in 1..=if startup { 1 } else { 32 } {
            assert_eq!(
                capture(
                    &root,
                    batch(f.session, f.goal, f.run, sequence, &[sequence as u8]),
                    if startup { 0 } else { now },
                )
                .status,
                CheckpointStatus::Captured
            );
        }
        let store = Store::open(root.try_clone().unwrap()).unwrap();
        let finished = f
            .events
            .iter()
            .position(|event| {
                matches!(event,
                    Event::ToolFinished { observation, .. }
                        if crate::session::checkpoint::publication(&observation.intent)
                )
            })
            .unwrap();
        for event in &f.events[..finished] {
            store.append(f.session, event.clone()).await.unwrap();
        }
        let Event::ToolFinished { observation, .. } = &f.events[finished] else {
            unreachable!()
        };
        let request = |intent_id, files| Capture {
            session: f.session,
            goal: f.goal,
            run: f.run,
            intent_id,
            files,
        };
        let discarded = store
            .capture_checkpoint(request(
                observation.intent.id,
                vec![("file".into(), b"incoming".to_vec())],
            ))
            .await
            .unwrap();
        assert_eq!(discarded.pruned_before_run, Some(f.run));
        let mut denied = observation.clone();
        denied.disposition = ToolDisposition::Denied;
        denied.attestation = None;
        store
            .append(
                f.session,
                Event::ToolFinished {
                    run_id: f.run,
                    agent_run_id: f.primary,
                    observation: denied,
                    checkpoint: None,
                },
            )
            .await
            .unwrap();
        let mut integration = f.events[finished + 1].clone();
        let Event::WorkerIntegrationRecorded { record, .. } = &mut integration else {
            unreachable!()
        };
        record.outcome = WorkerIntegrationOutcome::Denied;
        let integrated = store.append(f.session, integration).await.unwrap();
        let Event::WorkerLeaseReleased { mut record, .. } = f.events[finished + 2].clone() else {
            unreachable!()
        };
        record.reason = WorkerSettlementReason::Denied;
        record.cause_sequence = integrated.sequence;
        let discarded = store
            .append(
                f.session,
                Event::WorkerHandoverDiscarded {
                    run_id: f.run,
                    agent_run_id: f.primary,
                    record: record.clone(),
                },
            )
            .await
            .unwrap();
        record.cause_sequence = discarded.sequence;
        store
            .append(
                f.session,
                Event::WorkerLeaseReleased {
                    run_id: f.run,
                    agent_run_id: f.primary,
                    record,
                },
            )
            .await
            .unwrap();

        for index in 0..2 {
            for event in &f.events[finished + 3..finished + 5] {
                store.append(f.session, event.clone()).await.unwrap();
            }
            let mut intent = observation.intent.clone();
            intent.id = uuid::Uuid::now_v7();
            let path = format!("src/changed{index}");
            intent.call = ToolCall::Write {
                path: path.clone(),
                expected_digest: Some(crate::tools::types::hex_digest(b"before")),
                content: "new".into(),
            };
            store
                .append(
                    f.session,
                    Event::ToolStarted {
                        run_id: f.run,
                        agent_run_id: f.primary,
                        intent: intent.clone(),
                        active_ceiling_ms: 0,
                    },
                )
                .await
                .unwrap();
            let checkpoint = store
                .capture_checkpoint(request(intent.id, vec![(path, b"before".to_vec())]))
                .await
                .unwrap();
            assert_eq!(checkpoint.pruned_before_run, (index == 0).then_some(f.run));
            assert_eq!(checkpoint.status, CheckpointStatus::Captured);
            let mut observation = observation.clone();
            observation.intent = intent.clone();
            observation.attestation = Some(Attestation::for_site(&intent));
            let finished = Event::ToolFinished {
                run_id: f.run,
                agent_run_id: f.primary,
                observation,
                checkpoint: Some(checkpoint),
            };
            if index == 0 {
                let mut invalid = finished.clone();
                let Event::ToolFinished {
                    checkpoint: Some(checkpoint),
                    ..
                } = &mut invalid
                else {
                    unreachable!()
                };
                checkpoint.pruned_before_run = Some(RunId::new());
                assert!(store.append(f.session, invalid).await.is_err());
                assert_eq!(
                    store
                        .capture_checkpoint(request(intent.id, Vec::new()))
                        .await
                        .unwrap()
                        .pruned_before_run,
                    Some(f.run),
                    "failed append must not consume the notice"
                );
            }
            store.append(f.session, finished).await.unwrap();
        }
        store.close().await.unwrap();
        let reopened = Store::open_read_only(root).unwrap();
        let events = reopened.load_session(f.session).await.unwrap();
        let view = SessionView::replay(f.session, &events).unwrap().unwrap();
        let notices = view.runs[0]
            .tools
            .iter()
            .filter(|tool| {
                tool.checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.pruned_before_run.is_some())
            })
            .count();
        assert_eq!(notices, 1, "one committed notice after a pre-GO denial");
        reopened.close().await.unwrap();
    }
}
