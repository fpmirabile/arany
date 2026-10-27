use super::*;

const MERGE_OUTPUT_BYTES: usize = 32 * 1024;

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux overlay publication and closed replay"
)]
fn whole_root_overlay_publication_and_refusals_reopen_without_execution() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline("whole-root overlay", SPILL_JOURNEY_WAIT);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for case in [
                "reads",
                "narrow",
                "file-only",
                "narrow-replacement",
                "narrow-no-space",
                "writes",
                "spill-merge",
                "ignored-clean",
                "mode-zero",
                "no-space",
                "deletion",
                "delete-drift",
                "copy-ups",
                "recreation",
                "host-edit",
                "host-create",
            ] {
                overlay_case(case).await;
            }
        });
}

fn overlay_fixture(case: &str) -> SpillFixture {
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
    if case == "copy-ups" {
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
    if case == "writes" {
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
    fixture
}

fn configure_narrow_grants(fixture: &SpillFixture, config: &mut Value, case: &str) {
    if case.starts_with("narrow") || case == "file-only" {
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
        config["workspace_paths"] = if case == "file-only" {
            json!(["Cargo.toml"])
        } else {
            json!(["src", "lib/deep", "Cargo.toml", "large", "released"])
        };
    }
}

async fn overlay_case(case: &str) {
    let fixture = overlay_fixture(case);
    let spill_script = format!(
        "printf '%{}s\nPRIVATE_SPILL_CANARY\n' ''; printf '%{MERGE_OUTPUT_BYTES}s' '' > created",
        MERGE_OUTPUT_BYTES - b"\nPRIVATE_SPILL_CANARY\n".len()
    );
    let script = match case {
        "spill-merge" => spill_script.as_str(),
        "file-only" => {
            "set -eu; test \"$(cat Cargo.toml)\" = manifest; test ! -e src; test ! -e sibling; if printf write > Cargo.toml 2>/dev/null; then exit 1; fi; if touch created 2>/dev/null; then exit 1; fi"
        }
        "narrow" => {
            "set -eu; test \"$(cat Cargo.toml)\" = manifest; test \"$(wc -c < large)\" -eq 2097152; test ! -s src/.env; test ! -e sibling; test ! -e node_modules; test ! -e lib/sibling; test ! -e /seed/namespace; if printf write > Cargo.toml 2>/dev/null; then exit 1; fi; if touch lib/sibling 2>/dev/null; then exit 1; fi; printf 'private edit' > src/current.txt; printf 'private create' > lib/deep/created; printf ignored > src/node_modules/generated; printf selected > src/gen/output; printf 'narrow grants passed\\n'"
        }
        "narrow-replacement" => {
            "while read -r signal < released && test \"$signal\" = hold; do :; done; printf candidate > src/current.txt"
        }
        "narrow-no-space" => {
            "printf partial > src/current.txt; dd if=/dev/zero of=src/node_modules/fill bs=1M count=160 2>/dev/null; dd if=/dev/zero of=lib/deep/node_modules/fill bs=1M count=160 2>/dev/null"
        }

        "writes" => {
            "printf 'private edit' > src/current.txt; printf 'private create' > created; printf 'src/current.txt\\n' > .gitignore"
        }
        "ignored-clean" => {
            "set -eu; rm -rf node_modules/cache dist; mkdir -p node_modules/cache; printf replacement > node_modules/cache/new"
        }
        "mode-zero" => "umask 0777; printf readable > created",
        "no-space" => {
            "printf partial > src/current.txt; dd if=/dev/zero of=node_modules/fill bs=1M count=300 2>/dev/null"
        }
        "deletion" => "rm src/current.txt",
        "copy-ups" => {
            "set -eu; : >> large; for file in src/copy-*; do : >> \"$file\"; done; : >> src/current.txt"
        }
        "delete-drift" => {
            "while read -r signal < released && test \"$signal\" = hold; do :; done; rm src/current.txt"
        }
        "recreation" => "rm -rf src; mkdir src; printf recreated > src/current.txt",
        "host-edit" | "host-create" => {
            "while read -r signal < released && test \"$signal\" = hold; do :; done; printf candidate > src/current.txt; printf candidate > created"
        }
        _ => {
            "set -eu; test \"$(cat node_modules/link)\" = dependency; test \"$(wc -c < large)\" -eq 2097152; test ! -s node_modules/package/deep/.env.local; test ! -e /seed/namespace; if printf leak > node_modules/package/deep/.env.local 2>/dev/null; then exit 1; fi; printf 'overlay reads passed\\n'"
        }
    };
    let command = ToolCall::Command {
        program: "bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: "".into(),
    };
    let dispatched = matches!(
        case,
        "host-edit" | "host-create" | "delete-drift" | "narrow-replacement"
    )
    .then(|| arany::FaultPlan::observe_guard_dispatch(command.clone()));
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        crate::tool_outcome(command),
        arany::test_support::goals::finish_with("Done"),
    ]));
    let running = run_with(
        &fixture.engine,
        &provider,
        proposal_request(&fixture.workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    );
    let mutate = async {
        if let Some(dispatched) = dispatched {
            dispatched.await.unwrap();
            mutate_overlay_lower(&fixture, case);
            private_file(&fixture.workspace.join("released"), b"release\n");
        }
    };
    let (outcome, ()) = tokio::join!(running, mutate);
    check_overlay_case(fixture, &provider, outcome.unwrap(), case).await;
}

fn mutate_overlay_lower(fixture: &SpillFixture, case: &str) {
    if case == "narrow-replacement" {
        std::fs::rename(
            fixture.workspace.join("src"),
            fixture.workspace.join("old-source"),
        )
        .unwrap();
        std::fs::create_dir(fixture.workspace.join("src")).unwrap();
        private_file(&fixture.workspace.join("src/current.txt"), b"host");
    } else {
        private_file(
            &fixture
                .workspace
                .join(if matches!(case, "host-edit" | "delete-drift") {
                    "src/current.txt"
                } else {
                    "created"
                }),
            b"host",
        );
    }
}

async fn check_overlay_case(
    fixture: SpillFixture,
    provider: &Arc<arany::test_support::scenario::ScriptedProvider>,
    outcome: arany::RunOutcome,
    case: &str,
) {
    let publishes = matches!(
        case,
        "writes"
            | "mode-zero"
            | "no-space"
            | "spill-merge"
            | "narrow"
            | "narrow-no-space"
            | "deletion"
    );
    check_overlay_result(&fixture, &outcome, case);
    fixture.engine.close().await.unwrap();
    let (events, view) =
        arany::test_support::goals::replayed(&fixture.state, outcome.session_id).await;
    assert_eq!(view.runs[0], outcome.run);
    assert_eq!(events.iter().any(|event| matches!(&event.event, arany::Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::ApplyChangeset { .. }))), publishes, "{case}");
    if publishes {
        if case == "deletion" {
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
    if case == "spill-merge" {
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

fn check_overlay_result(fixture: &SpillFixture, outcome: &arany::RunOutcome, case: &str) {
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    let refused = matches!(
        case,
        "recreation" | "host-edit" | "host-create" | "delete-drift" | "narrow-replacement"
    );
    assert_eq!(
        observation.disposition,
        if matches!(
            case,
            "host-edit" | "host-create" | "delete-drift" | "narrow-replacement"
        ) {
            ToolDisposition::Conflict
        } else if refused {
            ToolDisposition::Denied
        } else {
            ToolDisposition::Succeeded
        },
        "{case}: {}",
        observation.output
    );
    let publishes = matches!(
        case,
        "writes"
            | "mode-zero"
            | "no-space"
            | "spill-merge"
            | "narrow"
            | "narrow-no-space"
            | "deletion"
    );
    if !refused {
        let result: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(
            result["exit_code"],
            if matches!(case, "no-space" | "narrow-no-space") {
                1
            } else {
                0
            },
            "{case}"
        );
        assert_eq!(result["selection"]["pruned_directories"], 0);
        assert_eq!(result["selection"]["oversized_inputs"], 0);
        if case == "reads" {
            assert!(observation.spill.is_none());
            assert_eq!(result["stdout"], "overlay reads passed\n");
        }
        if case == "narrow" {
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
        if case == "copy-ups" {
            assert_eq!(result["selection"]["discarded_outputs"], 0);
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

fn check_overlay_files(workspace: &std::path::Path, case: &str) {
    let expected: &[u8] = match case {
        "writes" | "narrow" => b"private edit",
        "no-space" | "narrow-no-space" => b"partial",
        "host-edit" | "delete-drift" | "narrow-replacement" => b"host",
        _ => b"current",
    };
    if case == "deletion" {
        assert!(!workspace.join("src/current.txt").exists());
    } else {
        assert_eq!(
            std::fs::read(workspace.join("src/current.txt")).unwrap(),
            expected,
            "{case}"
        );
    }
    if case == "mode-zero" {
        assert_eq!(
            std::fs::read(workspace.join("created")).unwrap(),
            b"readable"
        );
    }
    if case == "host-create" {
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
