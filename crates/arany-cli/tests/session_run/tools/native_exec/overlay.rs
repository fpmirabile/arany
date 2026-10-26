use super::*;

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux whole-root overlay and closed replay"
)]
fn whole_root_reads_dependencies_and_large_files_and_refuses_nonempty_uppers() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline("whole-root overlay", SPILL_JOURNEY_WAIT);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for writes in [false, true] {
            let fixture = SpillFixture::new();
            fixture.set_workspace_root(".");
            std::fs::create_dir_all(fixture.workspace.join("node_modules/package/deep")).unwrap();
            private_file(&fixture.workspace.join("node_modules/package/input"), b"dependency");
            private_file(&fixture.workspace.join("node_modules/package/deep/.env.local"), b"PRIVATE_OVERLAY_CANARY");
            private_file(&fixture.workspace.join(".gitignore"), b"node_modules/\nlarge\n");
            let large = std::fs::File::create(fixture.workspace.join("large")).unwrap();
            large.set_len(2 * 1024 * 1024).unwrap();
            std::os::unix::fs::symlink("package/input", fixture.workspace.join("node_modules/link")).unwrap();
            let script = if writes {
                "printf 'private edit' > src/current.txt; printf 'private create' > created"
            } else {
                "set -eu; test \"$(cat node_modules/link)\" = dependency; test \"$(wc -c < large)\" -eq 2097152; test ! -s node_modules/package/deep/.env.local; test ! -e /seed/namespace; if printf leak > node_modules/package/deep/.env.local 2>/dev/null; then exit 1; fi; printf 'overlay reads passed\\n'"
            };
            let command = ToolCall::Command { program: "bash".into(), args: vec!["-c".into(), script.into()], cwd: "".into() };
            let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![crate::tool_outcome(command), arany::test_support::goals::finish_with("Done")]));
            let outcome = run_with(&fixture.engine, &provider, proposal_request(&fixture.workspace), RunControls { tools: ToolGrant::Configured, ..Default::default() }).await.unwrap();
            let observation = outcome.run.tools[0].observation.as_ref().unwrap();
            assert_eq!(observation.disposition, if writes { ToolDisposition::Denied } else { ToolDisposition::Succeeded });
            if !writes {
                let result: Value = serde_json::from_str(&observation.output).unwrap();
                assert_eq!(result["exit_code"], 0);
                assert_eq!(result["stdout"], "overlay reads passed\n");
                assert_eq!(result["changeset"]["files"], json!([]));
            }
            assert!(matches!(&observation.attestation, Some(arany::Attestation::Guard(receipt)) if receipt.intent_digest == Sha256::digest(serde_json::to_vec(&outcome.run.tools[0].intent).unwrap()).as_slice()));
            assert!(!observation.output.contains("PRIVATE_OVERLAY_CANARY"));
            assert_eq!(std::fs::read(fixture.workspace.join("src/current.txt")).unwrap(), b"current");
            assert!(!fixture.workspace.join("created").exists());
            fixture.engine.close().await.unwrap();
            let (events, view) = arany::test_support::goals::replayed(&fixture.state, outcome.session_id).await;
            assert_eq!(view.runs[0], outcome.run);
            assert!(!events.iter().any(|event| matches!(&event.event, arany::Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::ApplyChangeset { .. }))));
            assert_eq!(provider.requests.lock().unwrap().len(), 2);
        }
    });
}
