use super::*;
use crate::tools::selection::TEST_RUNTIME;

fn config() -> Config {
    serde_json::from_value(serde_json::json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]})).unwrap()
}

fn workspace() -> (tempfile::TempDir, Dir, Dir) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    for (path, bytes) in [
        (".gitignore", b"*.log\n".as_slice()),
        ("src/lib.rs", b"fn a() {}\n"),
        ("src/gone.rs", b"gone"),
        ("kept.txt", b"kept"),
        ("debug.log", b"ignored"),
        ("target/out", b"build"),
    ] {
        let path = source.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    std::fs::write(source.join("large.bin"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
    std::fs::create_dir(temp.path().join("editor")).unwrap();
    let open = |name: &str| super::open_directory(&temp.path().join(name)).unwrap();
    let (source, editor) = (open("source"), open("editor"));
    (temp, source, editor)
}

fn limits() -> ChangeLimits {
    ChangeLimits {
        files: 8,
        bytes: 1024,
    }
}

fn run(editor: &Dir, source: &Dir) -> Result<Vec<(String, bool, bool)>, &'static str> {
    let changes = changes(editor, source, Instant::now() + TEST_RUNTIME, &limits());
    changes.map_err(|error| error.variant()).map(|changes| {
        changes
            .into_iter()
            .map(|change| (change.path, change.before.is_some(), change.after.is_some()))
            .collect()
    })
}

#[test]
fn editor_workspaces_clone_project_inputs_and_integrate_only_project_changes() {
    let (temp, source, editor) = workspace();
    create(&editor, &source, &config(), Instant::now() + TEST_RUNTIME).unwrap();
    let tree = temp.path().join("editor").join(TREE);
    assert_eq!(
        std::fs::read(tree.join("src/lib.rs")).unwrap(),
        b"fn a() {}\n"
    );
    assert!(!tree.join("debug.log").exists() && !tree.join("target").exists());
    // Inputs above the file bound are cloned too; only changing them is refused.
    assert_eq!(
        std::fs::read(tree.join("large.bin")).unwrap().len(),
        MAX_FILE_BYTES + 1
    );
    assert_eq!(run(&editor, &source).unwrap(), []);

    std::fs::write(tree.join("src/lib.rs"), b"fn b() {}\n").unwrap();
    std::fs::remove_file(tree.join("src/gone.rs")).unwrap();
    std::fs::write(tree.join("new.rs"), b"new").unwrap();
    std::fs::write(tree.join("trace.log"), b"ignored output").unwrap();
    std::fs::create_dir_all(tree.join("target/debug")).unwrap();
    std::fs::write(tree.join("target/debug/bin"), b"build output").unwrap();
    std::fs::write(tree.join(".env"), b"SECRET=1").unwrap();
    std::fs::write(tree.join("kept.txt"), b"kept").unwrap();
    // An excluded output over an existing Workspace path refuses by name; new ones are discarded.
    let error = changes(&editor, &source, Instant::now() + TEST_RUNTIME, &limits()).unwrap_err();
    assert!(
        matches!(&error, ToolError::Conflicts(paths) if paths == "target"),
        "{error:?}"
    );
    std::fs::remove_dir_all(tree.join("target")).unwrap();
    assert_eq!(
        run(&editor, &source).unwrap(),
        [
            ("new.rs".to_owned(), false, true),
            ("src/gone.rs".to_owned(), true, false),
            ("src/lib.rs".to_owned(), true, true),
        ]
    );
    std::fs::remove_file(tree.join("new.rs")).unwrap();
    std::fs::write(tree.join("big"), vec![b'x'; 2048]).unwrap();
    assert_eq!(run(&editor, &source), Err("Limit"));
    std::fs::remove_file(tree.join("big")).unwrap();
    // An oversized input is present but refuses typed edits as in the Workspace, and changing it
    // refuses the integration by name.
    let control = MutationControl::default();
    let edit = ToolCall::Edit {
        path: "large.bin".into(),
        expected_digest: Some(hex_digest(&vec![b'x'; MAX_FILE_BYTES + 1])),
        old: "x".into(),
        new: "y".into(),
    };
    let tree_dir = editor.open_dir_nofollow(TREE).unwrap();
    assert!(matches!(
        mutate(&tree_dir, &edit, &config(), &control),
        Err(ToolError::Limit)
    ));
    std::fs::write(tree.join("large.bin"), vec![b'y'; MAX_FILE_BYTES + 2]).unwrap();
    let error = changes(&editor, &source, Instant::now() + TEST_RUNTIME, &limits()).unwrap_err();
    assert!(
        matches!(&error, ToolError::AtPath { path, source } if path == "large.bin" && matches!(**source, ToolError::Limit)),
        "{error:?}"
    );
    std::fs::write(tree.join("large.bin"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
    std::os::unix::fs::symlink("kept.txt", tree.join("link")).unwrap();
    assert_eq!(run(&editor, &source), Err("Path"));
}

#[test]
fn editor_changes_refuse_unready_workspaces_and_disordered_bases() {
    let (temp, source, editor) = workspace();
    std::fs::remove_file(temp.path().join("source/large.bin")).unwrap();
    assert_eq!(run(&editor, &source), Err("Path"));
    create(&editor, &source, &config(), Instant::now() + TEST_RUNTIME).unwrap();
    let base = read_regular(open_file(&editor, BASE).unwrap(), MAX_FILE_BYTES).unwrap();
    let mut lines: Vec<_> = base.split_inclusive(|byte| *byte == b'\n').collect();
    lines.reverse();
    editor.remove_file(BASE).unwrap();
    put(&editor, BASE, &lines.concat(), false).unwrap();
    assert_eq!(run(&editor, &source), Err("Path"));
}
