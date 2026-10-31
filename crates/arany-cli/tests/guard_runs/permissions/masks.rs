use super::*;

const PROTECTED: [&str; 4] = ["a/b/.env", ".ssh/id", "a/.aws/x", "link"];

// A folder with protected names at several depths, a link to one of them and a readable sibling.
fn granted_folder(root: &std::path::Path) -> std::path::PathBuf {
    let folder = root.join("granted");
    for path in ["a/b/.env", ".ssh/id", "a/.aws/x", "sibling"] {
        let file = folder.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, path).unwrap();
    }
    std::os::unix::fs::symlink(folder.join(".ssh/id"), folder.join("link")).unwrap();
    std::fs::canonicalize(folder).unwrap()
}

fn read_command(folder: &std::path::Path) -> arany::ToolCall {
    let folder = folder.to_str().unwrap();
    let probes = PROTECTED
        .iter()
        .map(|path| format!("cat '{folder}/{path}' >/dev/null 2>&1 && echo 'exposed {path}';"))
        .collect::<String>();
    arany::test_support::with_additional_reads(
        arany::ToolCall::Command {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "cat '{folder}/sibling' || echo hidden-sibling; {probes} ls -A '{folder}/.ssh' 2>/dev/null | grep -q . && echo exposed-directory; true"
                ),
            ],
            cwd: "".into(),
            resource_class: None,
            timeout_ms: None,
            additional_reads: None,
        },
        &[folder],
    )
}

async fn run_reads(fixture: &Fixture, call: &arany::ToolCall) -> serde_json::Value {
    let (result, _, humans) = fixture
        .run(
            vec![
                arany::test_support::scenario::plan_outcome(plan(call)),
                tool_outcome(call.clone()),
                finish("answer"),
            ],
            Some(arany::ApprovalMode::Request),
            false,
        )
        .await;
    assert_eq!(humans, 1, "a new read folder asks once");
    let observation = result.run.tools[0].observation.as_ref().unwrap();
    serde_json::from_str(&observation.output).unwrap_or_else(|_| {
        panic!(
            "{:?}: {} {:?}",
            observation.disposition,
            observation.output,
            arany::test_support::recorded_tool_failure(observation.intent.id)
        )
    })
}

// macOS hides protected names at any depth; Linux hides only the folder's first level.
#[tokio::test]
async fn approved_read_folders_hide_protected_names() {
    let fixture = Fixture::new(None, true);
    let outside = tempfile::tempdir().unwrap();
    let folder = granted_folder(outside.path());
    let output = run_reads(&fixture, &read_command(&folder)).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    assert_eq!(
        output["stdout"],
        if cfg!(target_os = "linux") {
            "siblingexposed a/b/.env\nexposed a/.aws/x\n"
        } else {
            "sibling"
        },
        "{output}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_over_limit_folder_is_withheld_and_listed() {
    let fixture = Fixture::new(None, true);
    let outside = tempfile::tempdir().unwrap();
    let folder = granted_folder(outside.path());
    arany::FaultPlan::refuse_mask_scan(&folder);
    let output = run_reads(&fixture, &read_command(&folder)).await;
    assert_eq!(output["stdout"], "hidden-sibling\n", "{output}");
    assert_eq!(
        output["hidden_grants"],
        serde_json::json!([folder.to_str().unwrap()])
    );
}
