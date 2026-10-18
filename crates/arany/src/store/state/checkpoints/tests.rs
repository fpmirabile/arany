use super::*;

fn fixture() -> (tempfile::TempDir, StateRoot) {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).unwrap();
    (temp, root)
}
fn batch(session: SessionId, goal: GoalId, run: RunId, sequence: u64, content: &[u8]) -> Capture {
    Capture {
        session,
        goal,
        run,
        sequence,
        intent_id: uuid::Uuid::now_v7(),
        files: vec![("file".into(), content.to_vec())],
    }
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
