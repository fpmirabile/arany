use crate::tools::tests::{in_process_runtime, inspect_read};
use crate::tools::types::*;
use serde_json::{Value, json};

#[tokio::test]
async fn exact_search_path_refusals_are_attested_without_stopping_the_run() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(fixture.path().join("outside.txt"), b"outside payload").unwrap();
    std::os::unix::fs::symlink(fixture.path().join("outside.txt"), workspace.join("link")).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (path, disposition) in [
        ("link", ToolDisposition::Denied),
        ("missing", ToolDisposition::Failed),
    ] {
        let intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            search(path, "payload"),
        );
        let inspected = inspect_read(&runtime, intent.clone()).await;
        let observation = runtime.execute(intent, super::Cancellation::new()).await;
        assert_eq!(inspected, observation);
        assert_eq!(observation.disposition, disposition);
        assert!(observation.valid() && !observation.stops_run());
        assert!(matches!(
            observation.attestation,
            Some(Attestation::InProcess { .. })
        ));
        assert!(!observation.output.contains("outside payload"));
    }
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        search("missing", "payload"),
    );
    std::fs::rename(&workspace, fixture.path().join("moved")).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let observation = runtime.execute(intent, super::Cancellation::new()).await;
    assert!(observation.stops_run() && observation.attestation.is_none());
    assert!(observation.output.contains("pinned input"));
}

pub(super) fn search(path: &str, query: &str) -> ToolCall {
    ToolCall::Search {
        path: path.into(),
        query: query.into(),
        regex: false,
        include: None,
        case_sensitive: true,
        offset: 0,
        limit: MAX_SEARCH_ROWS,
    }
}

pub(super) fn with_search(
    mut call: ToolCall,
    edit: impl FnOnce(&mut bool, &mut Option<String>, &mut bool, &mut u32, &mut u32),
) -> ToolCall {
    if let ToolCall::Search {
        regex,
        include,
        case_sensitive,
        offset,
        limit,
        ..
    } = &mut call
    {
        edit(regex, include, case_sensitive, offset, limit);
    }
    call
}

pub(super) async fn search_page(
    runtime: &super::ToolRuntime,
    call: ToolCall,
    result_bytes: u32,
) -> Value {
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        call.clone(),
    );
    intent.limits.result_bytes = result_bytes;
    let observation = inspect_read(runtime, intent).await;
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "{call:?}: {}",
        observation.output
    );
    assert!(observation.output.len() <= result_bytes as usize);
    serde_json::from_str(&observation.output).unwrap()
}

fn rows(page: &Value) -> Vec<(String, u64)> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["path"].as_str().unwrap().to_owned(),
                row["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn search_matches_regex_include_and_case_in_byte_order_without_protected_files() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for (path, text) in [
        ("src/nested/c.rs", "fn helper() {}\nfoo()\n"),
        ("src/a/z.rs", "foo\n"),
        ("src/b.txt", "foo\nFOO\n"),
        ("src/a.rs", "fn main() {}\nlet Foo = 1;\n// foo bar\n"),
        ("src/.env", "foo secret-canary\n"),
        ("src/.env.local", "foo secret-canary\n"),
        ("src/.ssh/key", "foo secret-canary\n"),
        ("target/build.rs", "foo ignored-canary\n"),
    ] {
        std::fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let row = |path: &str, line: u64| (path.to_owned(), line);
    let regex = |call| with_search(call, |regex, _, _, _, _| *regex = true);
    let include =
        |call, glob: &str| with_search(call, |_, include, _, _, _| *include = Some(glob.into()));
    let folded = |call| with_search(call, |_, _, case, _, _| *case = false);
    for (call, expected) in [
        (
            search("src", "foo"),
            vec![
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            folded(search("src", "foo")),
            vec![
                row("src/a.rs", 2),
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/b.txt", 2),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            search("src", "("),
            vec![
                row("src/a.rs", 1),
                row("src/nested/c.rs", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            regex(search("src", r"^fn \w+\(")),
            vec![row("src/a.rs", 1), row("src/nested/c.rs", 1)],
        ),
        (
            folded(regex(search(".", "^FOO$"))),
            vec![
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/b.txt", 2),
            ],
        ),
        (
            include(search("src", "foo"), "*.rs"),
            vec![row("src/a.rs", 3)],
        ),
        (
            include(search("src", "foo"), "**/*.rs"),
            vec![
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            include(search(".", "foo"), "src/{b.txt,a.?s}"),
            vec![row("src/a.rs", 3), row("src/b.txt", 1)],
        ),
        (
            include(search("src/b.txt", "foo"), "b.txt"),
            vec![row("src/b.txt", 1)],
        ),
        (include(search("src/b.txt", "foo"), "*.rs"), vec![]),
        (include(search(".", "foo"), "**/.env"), vec![]),
        (
            search(".", "foo"),
            vec![
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
    ] {
        let page = search_page(&runtime, call.clone(), MAX_TOOL_RESULT_BYTES as u32).await;
        assert_eq!(rows(&page), expected, "{call:?}");
        assert_eq!(page["next_offset"], Value::Null, "{call:?}");
        assert_eq!(page["may_be_truncated"], false, "{call:?}");
        assert!(!page.to_string().contains("canary"), "{call:?}");
    }

    let large = r"\w{1000}";
    for (call, cause) in [
        (regex(search("src", "(")), "invalid or unsupported regex"),
        (
            regex(search("src", "(?=foo)")),
            "invalid or unsupported regex",
        ),
        (
            regex(search("src", r"(f)\1")),
            "invalid or unsupported regex",
        ),
        (regex(search("src", large)), "regex exceeds the 128 KiB"),
        (
            include(search("src", "foo"), "[ab].rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "a**/b"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "{a,{b}}"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "{a,}"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "/src/*.rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "../*.rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "!*.rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "{a,b,c,d,e,f,g,h}{i,j,k,l,m,n,o,p,q}"),
            "include glob exceeds",
        ),
    ] {
        assert!(call.valid(), "{call:?}");
        // Refusals fit the smallest admitted result budget.
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call.clone(),
        );
        intent.limits.result_bytes = 128;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(observation.disposition, ToolDisposition::Failed, "{call:?}");
        assert!(
            observation.output.starts_with("Search pattern refused: ")
                && observation.output.contains(cause)
                && observation.output.ends_with("; nothing was read"),
            "{call:?}: {}",
            observation.output
        );
        assert!(matches!(
            observation.attestation,
            Some(Attestation::InProcess { .. })
        ));
        assert!(!observation.stops_run());
        assert!(observation.valid());
    }
}

#[tokio::test]
async fn search_pages_rows_with_a_hard_cap_and_resumes_after_fitted_rows() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    // Shuffled creation order; pages follow path byte order, not directory order.
    let mut expected = Vec::new();
    for index in [3, 0, 4, 1, 2] {
        let text: String = (1..=61)
            .map(|line| format!("hit {index}.{line}\nmiss\n"))
            .collect();
        std::fs::write(workspace.join(format!("src/f{index}.txt")), text).unwrap();
    }
    for index in 0..5 {
        expected.extend((1..=61).map(|line| (format!("src/f{index}.txt"), 2 * line - 1)));
    }
    std::fs::write(
        workspace.join("src/exact.md"),
        "exact\n".repeat(MAX_SEARCH_ROWS as usize),
    )
    .unwrap();
    std::fs::write(
        workspace.join("src/over.md"),
        "over\n".repeat(MAX_SEARCH_ROWS as usize + 1),
    )
    .unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let paged = |offset: u32, limit: u32| {
        with_search(search("src", "hit"), move |_, _, _, o, l| {
            (*o, *l) = (offset, limit);
        })
    };

    for (budget, limit) in [
        (MAX_TOOL_RESULT_BYTES as u32, MAX_SEARCH_ROWS),
        (1024, MAX_SEARCH_ROWS),
        (MAX_TOOL_RESULT_BYTES as u32, 7),
    ] {
        let (mut seen, mut offset, mut pages) = (Vec::new(), Some(0), 0);
        while let Some(start) = offset {
            let page = search_page(&runtime, paged(start, limit), budget).await;
            let page_rows = rows(&page);
            assert!(!page_rows.is_empty() && page_rows.len() <= limit as usize);
            assert_eq!(page["limit"], limit);
            offset = page["next_offset"].as_u64().map(|next| next as u32);
            assert_eq!(page["may_be_truncated"], offset.is_some());
            if let Some(next) = offset {
                assert_eq!(next as usize, start as usize + page_rows.len());
            }
            seen.extend(page_rows);
            pages += 1;
        }
        assert_eq!(seen, expected, "budget {budget}, limit {limit}");
        assert!(pages >= expected.len().div_ceil(limit as usize));
    }
    let first = search_page(
        &runtime,
        paged(0, MAX_SEARCH_ROWS),
        MAX_TOOL_RESULT_BYTES as u32,
    )
    .await;
    assert_eq!(rows(&first).len(), MAX_SEARCH_ROWS as usize);
    assert_eq!(first["next_offset"], MAX_SEARCH_ROWS);

    for (query, count, next) in [
        ("exact", MAX_SEARCH_ROWS, Value::Null),
        ("over", MAX_SEARCH_ROWS, json!(MAX_SEARCH_ROWS)),
        ("absent", 0, Value::Null),
    ] {
        let page = search_page(&runtime, search("src", query), MAX_TOOL_RESULT_BYTES as u32).await;
        assert_eq!(rows(&page).len(), count as usize, "{query}");
        assert_eq!(page["next_offset"], next, "{query}");
    }
    let past = search_page(&runtime, paged(10_000, MAX_SEARCH_ROWS), 1024).await;
    assert_eq!(past["entries"], json!([]));
    assert_eq!(past["next_offset"], Value::Null);

    for call in [
        paged(0, 0),
        paged(0, MAX_SEARCH_ROWS + 1),
        paged(MAX_SNAPSHOT_BYTES as u32 + 1, 1),
        with_search(search("src", "hit"), |_, include, _, _, _| {
            *include = Some(String::new());
        }),
    ] {
        assert!(!call.valid(), "{call:?}");
    }
    assert!(paged(MAX_SNAPSHOT_BYTES as u32, MAX_SEARCH_ROWS).valid());
}
