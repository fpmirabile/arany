use crate::tools::tests::in_process_runtime;
use crate::tools::types::*;
use serde_json::json;

#[test]
fn command_changeset_limits_cover_content_serialization_and_internal_context() {
    use crate::session::worker::ChangesetSource;

    let exact_content = CommandChangeset {
        files: vec![FileChange {
            path: "content".into(),
            before: Some(FileVersion::from_bytes(&vec![b'a'; 16 * 1024], false)),
            after: Some(FileVersion::from_bytes(&vec![b'b'; 16 * 1024], false)),
        }],
    };
    assert!(exact_content.valid(), "32 KiB combined content is admitted");
    let overflow_content = CommandChangeset {
        files: vec![FileChange {
            path: "content".into(),
            before: exact_content.files[0].before.clone(),
            after: Some(FileVersion::from_bytes(&vec![b'b'; 16 * 1024 + 1], false)),
        }],
    };
    assert!(
        !overflow_content.valid(),
        "combined content over 32 KiB rejects"
    );

    let long_escaped_paths = |components: usize| CommandChangeset {
        files: (0..MAX_CHANGESET_FILES)
            .map(|index| FileChange {
                path: format!(
                    "f{index:02}/{}",
                    std::iter::repeat_n("\"".repeat(100), components)
                        .collect::<Vec<_>>()
                        .join("/")
                ),
                before: None,
                after: Some(FileVersion::from_bytes(b"", false)),
            })
            .collect(),
    };
    let admitted = long_escaped_paths(3);
    let admitted_bytes = serde_json::to_vec(&admitted).unwrap();
    assert!(admitted_bytes.len() <= MAX_CHANGESET_SERIALIZED_BYTES);
    assert!(
        admitted.valid(),
        "long escaped paths fit the serialized ceiling"
    );
    assert!(
        !long_escaped_paths(4).valid(),
        "serialized overflow rejects"
    );

    let expected_paths: Vec<_> = admitted
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let integration = ToolCall::ApplyChangeset {
        source: ChangesetSource::CommandReceipt {
            source_intent_digest: [1; 32],
            changeset: admitted,
        },
    };
    assert_eq!(integration.display_facts().paths, expected_paths);
    assert_eq!(integration.display_facts().paths.len(), MAX_CHANGESET_FILES);
    assert_eq!(
        integration.argument_limit(),
        MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES
    );
    assert!(
        serde_json::to_vec(&integration).unwrap().len() <= integration.argument_limit(),
        "the internal envelope admits the largest serialized command changeset"
    );
    assert!(
        super::ToolRuntime::step_context_reserve(true, 0)
            >= MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES + 128 * 6 + 512 + 512,
        "context reserves escaped internal integration arguments"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn hook_task_fit_check_matches_the_encoded_task_ceiling() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(
        temp.path(),
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: String::new(),
        },
    );
    assert!(
        runtime
            .clone()
            .with_hook_stdin("{}")
            .hook_task_fits(&intent)
    );
    let padded = "x".repeat(super::guard::TASK_BYTES_CAP);
    assert!(
        !runtime
            .clone()
            .with_hook_stdin(&padded)
            .hook_task_fits(&intent)
    );
}
