use super::*;

#[path = "overlay/subtree.rs"]
mod subtree;

const MERGE_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
enum OverlayCase {
    Reads,
    Narrow,
    FileOnly,
    NarrowReplacement,
    NarrowNoSpace,
    Writes,
    SpillMerge,
    IgnoredClean,
    ModeZero,
    NoSpace,
    Deletion,
    DeleteDrift,
    CopyUps,
    Recreation,
    RecreationNonzero,
    SubtreeDeletion,
    SubtreeDrift,
    HostEdit,
    HostCreate,
}

const OVERLAY_CASES: [OverlayCase; 8] = [
    OverlayCase::Reads,
    OverlayCase::Writes,
    OverlayCase::SpillMerge,
    OverlayCase::IgnoredClean,
    OverlayCase::ModeZero,
    OverlayCase::NoSpace,
    OverlayCase::HostEdit,
    OverlayCase::HostCreate,
];
const PUBLICATION_OVERLAY_CASES: [OverlayCase; 3] = [
    OverlayCase::Deletion,
    OverlayCase::DeleteDrift,
    OverlayCase::CopyUps,
];
const SUBTREE_OVERLAY_CASES: [OverlayCase; 4] = [
    OverlayCase::Recreation,
    OverlayCase::RecreationNonzero,
    OverlayCase::SubtreeDeletion,
    OverlayCase::SubtreeDrift,
];
const NARROW_OVERLAY_CASES: [OverlayCase; 4] = [
    OverlayCase::Narrow,
    OverlayCase::FileOnly,
    OverlayCase::NarrowReplacement,
    OverlayCase::NarrowNoSpace,
];

impl OverlayCase {
    fn publishes(self) -> bool {
        match self {
            Self::Writes
            | Self::SpillMerge
            | Self::ModeZero
            | Self::NoSpace
            | Self::Narrow
            | Self::NarrowNoSpace
            | Self::Deletion
            | Self::Recreation
            | Self::RecreationNonzero
            | Self::SubtreeDeletion => true,
            Self::Reads
            | Self::FileOnly
            | Self::NarrowReplacement
            | Self::IgnoredClean
            | Self::DeleteDrift
            | Self::CopyUps
            | Self::SubtreeDrift
            | Self::HostEdit
            | Self::HostCreate => false,
        }
    }

    fn refused(self) -> bool {
        match self {
            Self::DeleteDrift
            | Self::SubtreeDrift
            | Self::HostEdit
            | Self::HostCreate
            | Self::NarrowReplacement => true,
            Self::Reads
            | Self::Narrow
            | Self::FileOnly
            | Self::NarrowNoSpace
            | Self::Writes
            | Self::SpillMerge
            | Self::IgnoredClean
            | Self::ModeZero
            | Self::NoSpace
            | Self::Deletion
            | Self::CopyUps
            | Self::Recreation
            | Self::RecreationNonzero
            | Self::SubtreeDeletion => false,
        }
    }

    fn host_path(self) -> Option<&'static str> {
        match self {
            Self::HostEdit | Self::DeleteDrift | Self::NarrowReplacement => Some("src/current.txt"),
            Self::HostCreate => Some("created"),
            Self::SubtreeDrift => Some("src/nested/host"),
            Self::Reads
            | Self::Narrow
            | Self::FileOnly
            | Self::NarrowNoSpace
            | Self::Writes
            | Self::SpillMerge
            | Self::IgnoredClean
            | Self::ModeZero
            | Self::NoSpace
            | Self::Deletion
            | Self::CopyUps
            | Self::Recreation
            | Self::RecreationNonzero
            | Self::SubtreeDeletion => None,
        }
    }

    fn disposition(self) -> ToolDisposition {
        match self {
            Self::Reads
            | Self::Writes
            | Self::SpillMerge
            | Self::IgnoredClean
            | Self::ModeZero
            | Self::Narrow
            | Self::FileOnly
            | Self::Deletion
            | Self::CopyUps
            | Self::Recreation
            | Self::SubtreeDeletion => ToolDisposition::Succeeded,
            Self::NoSpace | Self::NarrowNoSpace | Self::RecreationNonzero => {
                ToolDisposition::Failed
            }
            Self::SubtreeDrift => ToolDisposition::Conflict,
            Self::HostEdit | Self::HostCreate | Self::DeleteDrift | Self::NarrowReplacement => {
                ToolDisposition::Conflict
            }
        }
    }
}

impl std::fmt::Display for OverlayCase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reads => "reads",
            Self::Narrow => "narrow",
            Self::FileOnly => "file-only",
            Self::NarrowReplacement => "narrow-replacement",
            Self::NarrowNoSpace => "narrow-no-space",
            Self::Writes => "writes",
            Self::SpillMerge => "spill-merge",
            Self::IgnoredClean => "ignored-clean",
            Self::ModeZero => "mode-zero",
            Self::NoSpace => "no-space",
            Self::Deletion => "deletion",
            Self::DeleteDrift => "delete-drift",
            Self::CopyUps => "copy-ups",
            Self::Recreation => "recreation",
            Self::RecreationNonzero => "recreation-nonzero",
            Self::SubtreeDeletion => "subtree-deletion",
            Self::SubtreeDrift => "subtree-drift",
            Self::HostEdit => "host-edit",
            Self::HostCreate => "host-create",
        })
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux overlay publication and closed replay"
)]
fn whole_root_overlay_publication_and_refusals_reopen_without_execution() {
    overlay_journey(&OVERLAY_CASES);
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux overlay deletion and copy-up publication and closed replay"
)]
fn overlay_deletion_and_copyups_reopen_without_execution() {
    overlay_journey(&PUBLICATION_OVERLAY_CASES);
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux narrow overlay publication and closed replay"
)]
fn narrow_overlay_publication_and_refusals_reopen_without_execution() {
    overlay_journey(&NARROW_OVERLAY_CASES);
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux overlay subtree publication and closed replay"
)]
fn subtree_overlay_publication_and_refusals_reopen_without_execution() {
    overlay_journey(&SUBTREE_OVERLAY_CASES);
}

fn overlay_journey(cases: &[OverlayCase]) {
    let journey_wait = SPILL_JOURNEY_WAIT
        .saturating_mul(u32::try_from(cases.len()).expect("bounded overlay case count"));
    if !crate::isolation::enter_native_account_isolated_test_with_timeout(
        journey_wait
            .as_secs()
            .try_into()
            .expect("bounded overlay journey timeout"),
    ) {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline("Workspace overlay", journey_wait);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for &case in cases {
                let name = format!("Workspace overlay {case}");
                let _case_deadline =
                    arany::test_support::engine::deadline(&name, SPILL_JOURNEY_WAIT);
                overlay_case(case).await;
            }
        });
}

fn overlay_fixture(case: OverlayCase) -> SpillFixture {
    let fixture = SpillFixture::new();
    fixture.set_workspace_root(".");
    let path = fixture.state.join("tools.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["write"] = true.into();
    configure_narrow_grants(&fixture, &mut config, case);
    private_file(&path, &serde_json::to_vec(&config).unwrap());
    std::fs::create_dir_all(fixture.workspace.join("node_modules/package/deep")).unwrap();
    std::fs::create_dir_all(fixture.workspace.join("node_modules/cache")).unwrap();
    private_file(
        &fixture.workspace.join("node_modules/cache/old"),
        b"original dependency",
    );
    std::fs::create_dir(fixture.workspace.join("dist")).unwrap();
    private_file(&fixture.workspace.join("dist/output"), b"original output");
    private_file(
        &fixture.workspace.join("node_modules/package/input"),
        b"dependency",
    );
    private_file(
        &fixture
            .workspace
            .join("node_modules/package/deep/.env.local"),
        b"PRIVATE_OVERLAY_CANARY",
    );
    private_file(
        &fixture.workspace.join(".gitignore"),
        b"node_modules/\nlarge\ndist/\nsrc/gen/\n",
    );
    let large = std::fs::File::create(fixture.workspace.join("large")).unwrap();
    large.set_len(2 * 1024 * 1024).unwrap();
    std::os::unix::fs::symlink("package/input", fixture.workspace.join("node_modules/link"))
        .unwrap();
    private_file(&fixture.workspace.join("released"), b"hold\n");
    if case == OverlayCase::CopyUps {
        private_file(
            &fixture.workspace.join(".gitignore"),
            b"node_modules/\ndist/\nsrc/gen/\n",
        );
        for index in 0..=arany::test_support::max_changeset_files() {
            private_file(
                &fixture.workspace.join(format!("src/copy-{index}")),
                b"unchanged",
            );
        }
    }
    if case == OverlayCase::Writes {
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions) in [(1u16, 6u16), (4, 4), (16, 4), (32, 0)] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        for (name, value) in [
            ("user.note", b"copy-up attribute".as_slice()),
            ("system.posix_acl_access", acl.as_slice()),
        ] {
            rustix::fs::setxattr(
                fixture.workspace.join("src/current.txt"),
                name,
                value,
                rustix::fs::XattrFlags::CREATE,
            )
            .unwrap();
        }
    }
    subtree::prepare(&fixture.workspace, case);
    fixture
}

fn configure_narrow_grants(fixture: &SpillFixture, config: &mut Value, case: OverlayCase) {
    let roots: &[&str] = match case {
        OverlayCase::FileOnly => &["Cargo.toml"],
        OverlayCase::Narrow | OverlayCase::NarrowReplacement | OverlayCase::NarrowNoSpace => {
            &["src", "lib/deep", "Cargo.toml", "large", "released"]
        }
        OverlayCase::Reads
        | OverlayCase::Writes
        | OverlayCase::SpillMerge
        | OverlayCase::IgnoredClean
        | OverlayCase::ModeZero
        | OverlayCase::NoSpace
        | OverlayCase::Deletion
        | OverlayCase::DeleteDrift
        | OverlayCase::CopyUps
        | OverlayCase::Recreation
        | OverlayCase::RecreationNonzero
        | OverlayCase::SubtreeDeletion
        | OverlayCase::SubtreeDrift
        | OverlayCase::HostEdit
        | OverlayCase::HostCreate => return,
    };
    std::fs::create_dir_all(fixture.workspace.join("lib/deep/node_modules")).unwrap();
    std::fs::create_dir_all(fixture.workspace.join("src/node_modules")).unwrap();
    std::fs::create_dir(fixture.workspace.join("src/gen")).unwrap();
    private_file(
        &fixture.workspace.join("lib/sibling"),
        b"PRIVATE_SIBLING_CANARY",
    );
    private_file(&fixture.workspace.join("Cargo.toml"), b"manifest");
    private_file(
        &fixture.workspace.join("src/.env"),
        b"PRIVATE_OVERLAY_CANARY",
    );
    private_file(
        &fixture.workspace.join("sibling"),
        b"PRIVATE_SIBLING_CANARY",
    );
    config["workspace_paths"] = json!(roots);
}

fn overlay_command(case: OverlayCase) -> ToolCall {
    let spill_script = format!(
        "printf '%{}s\nPRIVATE_SPILL_CANARY\n' ''; printf '%{MERGE_OUTPUT_BYTES}s' '' > created",
        MERGE_OUTPUT_BYTES - b"\nPRIVATE_SPILL_CANARY\n".len()
    );
    let script = match case {
        OverlayCase::SpillMerge => spill_script.as_str(),
        OverlayCase::FileOnly => {
            "set -eu; test \"$(cat Cargo.toml)\" = manifest; test ! -e src; test ! -e sibling; if printf write > Cargo.toml 2>/dev/null; then exit 1; fi; if touch created 2>/dev/null; then exit 1; fi"
        }
        OverlayCase::Narrow => {
            "set -eu; test \"$(cat Cargo.toml)\" = manifest; test \"$(wc -c < large)\" -eq 2097152; test ! -s src/.env; test ! -e sibling; test ! -e node_modules; test ! -e lib/sibling; test ! -e /seed/namespace; if printf write > Cargo.toml 2>/dev/null; then exit 1; fi; if touch lib/sibling 2>/dev/null; then exit 1; fi; printf 'private edit' > src/current.txt; printf 'private create' > lib/deep/created; printf ignored > src/node_modules/generated; printf selected > src/gen/output; printf 'narrow grants passed\\n'"
        }
        OverlayCase::NarrowReplacement => "printf candidate > src/current.txt",
        OverlayCase::NarrowNoSpace => {
            "printf partial > src/current.txt; LC_ALL=C dd if=/dev/zero of=src/node_modules/fill bs=1M count=160; LC_ALL=C dd if=/dev/zero of=lib/deep/node_modules/fill bs=1M count=160"
        }

        OverlayCase::Writes => {
            "printf 'private edit' > src/current.txt; printf 'private create' > created; printf 'src/current.txt\\n' > .gitignore"
        }
        OverlayCase::IgnoredClean => {
            "set -eu; rm -rf node_modules/cache dist; mkdir -p node_modules/cache; printf replacement > node_modules/cache/new"
        }
        OverlayCase::ModeZero => "umask 0777; printf readable > created",
        OverlayCase::NoSpace => {
            "printf partial > src/current.txt; LC_ALL=C dd if=/dev/zero of=node_modules/fill bs=1M count=300"
        }
        OverlayCase::Deletion => "rm src/current.txt",
        OverlayCase::CopyUps => {
            "set -eu; : >> large; for file in src/copy-*; do : >> \"$file\"; done; : >> src/current.txt"
        }
        OverlayCase::DeleteDrift => {
            "while read -r signal < released && test \"$signal\" = hold; do :; done; rm src/current.txt"
        }
        OverlayCase::Recreation | OverlayCase::RecreationNonzero => subtree::recreate(case),
        OverlayCase::SubtreeDeletion => "rm -rf src",
        OverlayCase::SubtreeDrift => {
            "while read -r signal < released && test \"$signal\" = hold; do :; done; rm -rf src"
        }
        OverlayCase::HostEdit | OverlayCase::HostCreate => {
            "while read -r signal < released && test \"$signal\" = hold; do :; done; printf candidate > src/current.txt; printf candidate > created"
        }
        OverlayCase::Reads => {
            "set -eu; test \"$(cat node_modules/link)\" = dependency; test \"$(wc -c < large)\" -eq 2097152; test ! -s node_modules/package/deep/.env.local; test ! -e /seed/namespace; if printf leak > node_modules/package/deep/.env.local 2>/dev/null; then exit 1; fi; printf 'overlay reads passed\\n'"
        }
    };
    ToolCall::Command {
        program: "bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: "".into(),
        resource_class: None,
        timeout_ms: None,
    }
}

async fn overlay_case(case: OverlayCase) {
    let fixture = overlay_fixture(case);
    let command = overlay_command(case);
    let replacement = case == OverlayCase::NarrowReplacement;
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let dispatched = case
        .host_path()
        .filter(|_| !replacement)
        .map(|_| arany::FaultPlan::observe_guard_dispatch(command.clone()));
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        crate::tool_outcome(command.clone()),
        arany::test_support::goals::finish_with("Done"),
    ]));
    let running = run_with(
        &fixture.engine,
        &provider,
        proposal_request(&fixture.workspace),
        RunControls {
            tools: ToolGrant::Configured,
            approvals: replacement.then_some(approvals),
            ..Default::default()
        },
    );
    let mutate = async {
        if let Some(path) = case.host_path() {
            let release = if replacement {
                let approval = inbox.next().await.expect("overlay command approval");
                assert_eq!(approval.intent.call, command);
                let (entered, release) =
                    arany::FaultPlan::gate_guard_cache_report(approval.intent.id);
                approval.decide(true);
                let report = entered
                    .await
                    .expect("overlay command completed before capture");
                assert_eq!(report.disposition, ToolDisposition::Succeeded, "{case}");
                std::fs::rename(
                    fixture.workspace.join("src"),
                    fixture.workspace.join("old-source"),
                )
                .unwrap();
                std::fs::create_dir(fixture.workspace.join("src")).unwrap();
                Some(release)
            } else {
                dispatched
                    .expect("host mutation dispatch gate")
                    .await
                    .unwrap();
                None
            };
            private_file(&fixture.workspace.join(path), b"host");
            if let Some(release) = release {
                release.send(()).expect("resume overlay capture");
            } else {
                private_file(&fixture.workspace.join("released"), b"release\n");
            }
        }
    };
    let running = async {
        running.await.unwrap_or_else(|error| {
            crate::process::annotate_native_fixture(&format!(
                "fixture=command_overlay; case={case}; stage=engine; error={error:?}"
            ));
            panic!("Workspace overlay {case} Engine failed: {error:?}");
        })
    };
    let outcome = {
        tokio::pin!(running);
        tokio::pin!(mutate);
        tokio::select! {
            outcome = &mut running => outcome,
            () = &mut mutate => (&mut running).await,
        }
    };
    check_overlay_case(fixture, &provider, outcome, case).await;
}

async fn check_overlay_case(
    fixture: SpillFixture,
    provider: &Arc<arany::test_support::scenario::ScriptedProvider>,
    outcome: arany::RunOutcome,
    case: OverlayCase,
) {
    let publishes = case.publishes();
    check_overlay_result(&fixture, &outcome, case);
    subtree::check(&fixture.workspace, &outcome.run, case);
    fixture.engine.close().await.unwrap();
    let (events, view) =
        arany::test_support::goals::replayed(&fixture.state, outcome.session_id).await;
    assert_eq!(view.runs[0], outcome.run);
    assert_eq!(events.iter().any(|event| matches!(&event.event, arany::Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::ApplyChangeset { .. }))), publishes, "{case}");
    if publishes {
        if case == OverlayCase::Deletion {
            assert_eq!(
                outcome.run.tools[1].checkpoint.as_ref().unwrap().status,
                arany::CheckpointStatus::Captured
            );
            let ToolCall::ApplyChangeset {
                source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
            } = &outcome.run.tools[1].intent.call
            else {
                panic!("expected deletion integration")
            };
            assert_eq!(changeset.files.len(), 1);
            assert_eq!(changeset.files[0].path, "src/current.txt");
            assert_eq!(
                changeset.files[0].before.as_ref().unwrap().bytes().unwrap(),
                b"current"
            );
            assert!(changeset.files[0].after.is_none());
        }
        assert!(
            outcome.run.tools[1]
                .observation
                .as_ref()
                .unwrap()
                .attestation
                .is_some()
        );
    }
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    if case == OverlayCase::SpillMerge {
        let spill = outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .spill
            .as_ref()
            .unwrap();
        let mut expected = vec![b' '; MERGE_OUTPUT_BYTES - b"\nPRIVATE_SPILL_CANARY\n".len()];
        expected.extend_from_slice(b"\nPRIVATE_SPILL_CANARY\n");
        let path = fixture
            .state
            .join("spill")
            .join(outcome.run.id.to_string())
            .join(format!("{}.txt", spill.r#ref));
        assert!(std::fs::read(path).unwrap() == expected);
        let engine = arany::test_support::engine::open(
            StateRoot::open_existing(&fixture.state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_spill_session_scope(
            &engine,
            &fixture.workspace,
            outcome.session_id,
            &spill.r#ref,
        )
        .await;
        engine.close().await.unwrap();
    }
}

fn check_overlay_result(fixture: &SpillFixture, outcome: &arany::RunOutcome, case: OverlayCase) {
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    let refused = case.refused();
    let expected = case.disposition();
    if observation.disposition != expected {
        let exit = serde_json::from_str::<Value>(&observation.output)
            .ok()
            .and_then(|result| result["exit_code"].as_i64());
        crate::process::annotate_native_fixture(&format!(
            "fixture=command_overlay; case={case}; expected={expected:?}; observed={:?}; exit={exit:?}; limit={:?}; enforcement={:?}",
            observation.disposition, observation.limit_cause, observation.enforcement_cause
        ));
    }
    assert_eq!(observation.disposition, expected, "{case}");
    let publishes = case.publishes();
    if !refused {
        let result: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(
            result["exit_code"],
            if case == OverlayCase::RecreationNonzero {
                7
            } else if matches!(case, OverlayCase::NoSpace | OverlayCase::NarrowNoSpace) {
                1
            } else {
                0
            },
            "{case}"
        );
        assert_eq!(result["selection"]["pruned_directories"], 0);
        assert_eq!(result["selection"]["oversized_inputs"], 0);
        if case == OverlayCase::Reads {
            assert!(observation.spill.is_none());
            assert_eq!(result["stdout"], "overlay reads passed\n");
        }
        if case == OverlayCase::Narrow {
            assert_eq!(result["stdout"], "narrow grants passed\n");
            assert_eq!(
                std::fs::read(fixture.workspace.join("src/gen/output")).unwrap(),
                b"selected"
            );
            assert_eq!(
                std::fs::read(fixture.workspace.join("lib/deep/created")).unwrap(),
                b"private create"
            );
            assert!(
                !fixture
                    .workspace
                    .join("src/node_modules/generated")
                    .exists()
            );
        }
        if case == OverlayCase::CopyUps {
            assert_eq!(result["selection"]["discarded_outputs"], 0);
        }
        if matches!(case, OverlayCase::NoSpace | OverlayCase::NarrowNoSpace) {
            assert!(
                result["stderr"]
                    .as_str()
                    .unwrap()
                    .contains("No space left on device")
            );
        }
        if !publishes {
            assert_eq!(result["changeset"]["files"], json!([]));
        }
    }
    assert!(
        matches!(&observation.attestation, Some(arany::Attestation::Guard(receipt)) if receipt.intent_digest == Sha256::digest(serde_json::to_vec(&outcome.run.tools[0].intent).unwrap()).as_slice())
    );
    assert!(!observation.output.contains("PRIVATE_OVERLAY_CANARY"));
    assert!(!observation.output.contains("PRIVATE_SIBLING_CANARY"));
    check_overlay_files(&fixture.workspace, case);
}

fn check_overlay_files(workspace: &std::path::Path, case: OverlayCase) {
    let expected: &[u8] = match case {
        OverlayCase::Writes | OverlayCase::Narrow => b"private edit",
        OverlayCase::Recreation | OverlayCase::RecreationNonzero => &[0, 255],
        OverlayCase::NoSpace | OverlayCase::NarrowNoSpace => b"partial",
        OverlayCase::HostEdit | OverlayCase::DeleteDrift | OverlayCase::NarrowReplacement => {
            b"host"
        }
        OverlayCase::Reads
        | OverlayCase::FileOnly
        | OverlayCase::SpillMerge
        | OverlayCase::IgnoredClean
        | OverlayCase::ModeZero
        | OverlayCase::Deletion
        | OverlayCase::CopyUps
        | OverlayCase::SubtreeDeletion
        | OverlayCase::SubtreeDrift
        | OverlayCase::HostCreate => b"current",
    };
    if matches!(case, OverlayCase::Deletion | OverlayCase::SubtreeDeletion) {
        assert!(!workspace.join("src/current.txt").exists());
    } else {
        assert_eq!(
            std::fs::read(workspace.join("src/current.txt")).unwrap(),
            expected,
            "{case}"
        );
    }
    if case == OverlayCase::NarrowReplacement {
        assert_eq!(
            std::fs::read(workspace.join("old-source/current.txt")).unwrap(),
            b"current"
        );
    }
    if case == OverlayCase::Writes {
        assert_eq!(
            std::fs::read(workspace.join("created")).unwrap(),
            b"private create"
        );
    }
    if case == OverlayCase::SpillMerge {
        assert_eq!(
            std::fs::read(workspace.join("created")).unwrap(),
            vec![b' '; MERGE_OUTPUT_BYTES]
        );
    }
    if case == OverlayCase::ModeZero {
        assert_eq!(
            std::fs::read(workspace.join("created")).unwrap(),
            b"readable"
        );
    }
    if case == OverlayCase::HostCreate {
        assert_eq!(std::fs::read(workspace.join("created")).unwrap(), b"host");
    }
    assert_eq!(
        std::fs::read(workspace.join("node_modules/cache/old")).unwrap(),
        b"original dependency"
    );
    assert_eq!(
        std::fs::read(workspace.join("dist/output")).unwrap(),
        b"original output"
    );
}
