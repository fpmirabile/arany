use super::*;

pub(crate) fn fixture() -> (CheckId, Vec<CheckEvent>) {
    let id = "01900000-0000-7000-8000-000000000001".parse().unwrap();
    let events = include_str!("identity.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (id, events)
}
pub(crate) fn start_fixture() -> CheckStart {
    let (_, events) = fixture();
    let CheckEvent::ProviderCheckStarted { mut start } = events[0].clone() else {
        unreachable!()
    };
    start.started_at_ms = now_ms().unwrap();
    start.deadline_ms = start.started_at_ms + LIFETIME_MS as i64;
    start
}

#[test]
fn format_registry_and_transition_corpus_reject_forged_progress() {
    let (id, events) = fixture();
    for ((kind, floor, current), representative) in CHECK_FORMATS.iter().map(|entry| {
        (
            entry,
            events
                .iter()
                .find(|event| event.kind() == entry.0)
                .expect("identity fixture for every registered kind"),
        )
    }) {
        assert_eq!((*floor, *current), (1, representative.version()), "{kind}");
    }
    for (event, line) in events.iter().zip(include_str!("identity.jsonl").lines()) {
        assert_eq!(serde_json::to_string(event).unwrap(), line);
    }
    for length in 1..events.len() {
        let view = CheckView::replay(id, &events[..length]).unwrap().unwrap();
        assert_eq!(view.replay_disposition(), CheckDisposition::Uncertain);
    }
    assert_eq!(
        CheckView::replay(id, &events).unwrap().unwrap().disposition,
        Some(CheckDisposition::Succeeded)
    );
    for (prefix, event) in [
        (0, events[1].clone()),
        (1, events[0].clone()),
        (1, events[2].clone()),
        (2, events[3].clone()),
        (3, events[2].clone()),
        (4, events[3].clone()),
        (4, events[6].clone()),
        (2, events[8].clone()),
        (8, events[9].clone()),
        (10, events[2].clone()),
        (
            8,
            CheckEvent::ProviderCheckEvidenceStarted {
                digest: [3; 32],
                snapshot: [9; 32],
            },
        ),
        (
            3,
            CheckEvent::ProviderCheckCallFinished {
                ordinal: 1,
                disposition: CheckDisposition::Succeeded,
                usage: Some(CheckUsage {
                    input_tokens: 12,
                    output_tokens: 8,
                }),
                catalog_complete: false,
            },
        ),
    ] {
        let mut forged = events[..prefix].to_vec();
        forged.push(event);
        assert!(CheckView::replay(id, &forged).is_err(), "prefix {prefix}");
    }
    for disposition in [
        CheckDisposition::Rejected,
        CheckDisposition::Failed,
        CheckDisposition::Cancelled,
        CheckDisposition::Uncertain,
    ] {
        let mut stopped = events[..3].to_vec();
        stopped.push(CheckEvent::ProviderCheckCallFinished {
            ordinal: 0,
            disposition,
            usage: None,
            catalog_complete: false,
        });
        let view = CheckView::replay(id, &stopped).unwrap().unwrap();
        assert!(CheckReducer::apply(id, Some(view.clone()), &events[4]).is_err());
        assert!(
            CheckReducer::apply(
                id,
                Some(view),
                &CheckEvent::ProviderCheckFinished {
                    disposition,
                    evidence: None
                }
            )
            .is_ok()
        );
    }
    for change in 0..6 {
        let mut start = start_fixture();
        match change {
            0 => start.cost_consent = false,
            1 => start.probes[0].output_tokens += 1,
            2 => start.probes[1].kind = CheckCallKind::Compaction,
            3 => start.deadline_ms += 1,
            4 => start.catalog_pages = 1,
            5 => start.probes.clear(),
            _ => unreachable!(),
        }
        assert!(CheckView::replay(id, &[CheckEvent::ProviderCheckStarted { start }]).is_err());
    }
}

#[test]
fn catalog_completion_closes_unused_allowance_and_admits_at_most_sixteen_pages() {
    for pages in [1, 16] {
        let (id, _) = fixture();
        let mut start = start_fixture();
        start.target = CheckTarget::Native {
            profile: "anthropic".into(),
            model: start.model.clone(),
            account: None,
            effort: Effort::High,
            workspace: None,
        };
        start.catalog_pages = 16;
        for probe in &mut start.probes {
            probe.effort = Some(Effort::High);
            probe.output_tokens = 1024;
        }
        let mut view = CheckView::replay(
            id,
            &[
                CheckEvent::ProviderCheckStarted { start },
                CheckEvent::ProviderCheckEvidenceCleared,
            ],
        )
        .unwrap()
        .unwrap();
        for page in 0..pages {
            let intent = CheckEvent::ProviderCheckCallStarted {
                ordinal: page,
                kind: CheckCallKind::Catalog,
                destination: [2; 32],
                calls_remaining: 3,
                output_remaining: 3072,
                time_remaining_ms: 1000,
            };
            view = CheckReducer::apply(id, Some(view), &intent).unwrap();
            view = CheckReducer::apply(
                id,
                Some(view),
                &CheckEvent::ProviderCheckCallFinished {
                    ordinal: page,
                    disposition: CheckDisposition::Succeeded,
                    usage: None,
                    catalog_complete: page + 1 == pages,
                },
            )
            .unwrap();
        }
        let extra = CheckEvent::ProviderCheckCallStarted {
            ordinal: pages,
            kind: CheckCallKind::Catalog,
            destination: [2; 32],
            calls_remaining: 3,
            output_remaining: 3072,
            time_remaining_ms: 1000,
        };
        assert!(CheckReducer::apply(id, Some(view.clone()), &extra).is_err());
        for (index, kind) in [
            CheckCallKind::Answer,
            CheckCallKind::Delegate,
            CheckCallKind::Compaction,
        ]
        .into_iter()
        .enumerate()
        {
            let ordinal = pages + u8::try_from(index).unwrap();
            let intent = CheckEvent::ProviderCheckCallStarted {
                ordinal,
                kind,
                destination: [2; 32],
                calls_remaining: view.calls_remaining(),
                output_remaining: view.output_remaining(),
                time_remaining_ms: 1000,
            };
            view = CheckReducer::apply(id, Some(view), &intent).unwrap();
            view = CheckReducer::apply(
                id,
                Some(view),
                &CheckEvent::ProviderCheckCallFinished {
                    ordinal,
                    disposition: CheckDisposition::Succeeded,
                    usage: Some(CheckUsage {
                        input_tokens: 12,
                        output_tokens: 8,
                    }),
                    catalog_complete: false,
                },
            )
            .unwrap();
        }
        view = CheckReducer::apply(
            id,
            Some(view),
            &CheckEvent::ProviderCheckEvidenceStarted {
                digest: [3; 32],
                snapshot: [1; 32],
            },
        )
        .unwrap();
        view = CheckReducer::apply(
            id,
            Some(view),
            &CheckEvent::ProviderCheckFinished {
                disposition: CheckDisposition::Succeeded,
                evidence: Some([3; 32]),
            },
        )
        .unwrap();
        assert_eq!(view.events, 10 + 2 * usize::from(pages));
    }
}

#[tokio::test]
async fn unfinished_checks_reopen_without_effects_and_recover_only_under_the_lease() {
    let (_, events) = fixture();
    for prefix in 2..events.len() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let mut journal = CheckJournal::start(StateRoot::admit(&path).unwrap(), start_fixture())
            .await
            .unwrap();
        let id = journal.view().id;
        for event in &events[2..prefix] {
            journal.append(event.clone()).await.unwrap();
        }
        assert!(matches!(
            CheckJournal::start(StateRoot::open_existing(&path).unwrap(), start_fixture()).await,
            Err(CheckOpenError {
                source: StoreError::ProviderCheckBusy,
                ..
            })
        ));
        let before = journal.view().clone();
        drop(journal);
        let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        assert_eq!(store.load_check(id).await.unwrap(), Some(before));
        assert!(
            store
                .load_provider_evidence("local".into())
                .await
                .unwrap()
                .is_none()
        );
        store.close().await.unwrap();
        let next = CheckJournal::start(StateRoot::open_existing(&path).unwrap(), start_fixture())
            .await
            .unwrap();
        assert_ne!(next.view().id, id);
        let recovered = next.store.load_check(id).await.unwrap().unwrap();
        assert_eq!(recovered.disposition, Some(CheckDisposition::Uncertain));
        assert!(recovered.pending.is_none());
        assert_eq!(next.view().completed_calls, 0);
        let connection = rusqlite::Connection::open(path.join("events.sqlite3")).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
