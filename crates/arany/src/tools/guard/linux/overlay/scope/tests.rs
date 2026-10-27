use super::*;

#[test]
fn narrow_mounts_attest_every_grant_readonly_parents_and_aggregate_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    std::fs::write(workspace.join("Cargo.toml"), b"manifest").unwrap();
    let metadata = std::fs::metadata(workspace.join("Cargo.toml")).unwrap();
    let roots = vec![
        Root {
            relative: "src".into(),
            device: 0,
            inode: 0,
            directory: true,
        },
        Root {
            relative: "crates/core".into(),
            device: 0,
            inode: 0,
            directory: true,
        },
        Root {
            relative: "Cargo.toml".into(),
            device: metadata.dev(),
            inode: metadata.ino(),
            directory: false,
        },
    ];
    let paths = roots
        .iter()
        .map(|root| root.relative.clone())
        .collect::<Vec<_>>();
    let attestation = Attestation {
        digest: [0; 32],
        namespaces: profile::Namespaces::read().unwrap(),
        masks: vec!["src/.env".into()],
        roots,
    };
    let topology = format!(
        "1 0 0:1 / / ro - tmpfs tmpfs ro\n2 1 0:2 / {base} ro,nosuid,nodev - tmpfs tmpfs rw,size={size}k,nr_inodes={inodes}\n3 2 0:3 / {base}/src rw,nosuid,nodev - overlay overlay rw,userxattr\n4 3 0:2 /grant-0/empty-file {base}/src/.env ro,nosuid,nodev - tmpfs tmpfs rw\n5 2 0:4 / {base}/crates/core rw,nosuid,nodev - overlay overlay rw,userxattr\n6 2 0:5 /project/Cargo.toml {base}/Cargo.toml ro,nosuid,nodev - ext4 /dev/disk rw\n",
        base = workspace.display(),
        size = PARENT_BYTES / 1024,
        inodes = parent_inodes(&attestation.roots)
    );
    let verify = |text: &str| {
        verify_workspace(
            &parse_mountinfo(text.as_bytes()).unwrap(),
            &attestation,
            &paths,
            workspace,
        )
    };
    assert!(verify(&topology).is_ok());
    for invalid in [
        topology.replacen("ro,nosuid,nodev - tmpfs", "rw,nosuid,nodev - tmpfs", 1),
        topology.replace(&format!("size={}k", PARENT_BYTES / 1024), "size=1k"),
        topology.replace(
            &format!("nr_inodes={}", parent_inodes(&attestation.roots)),
            "nr_inodes=1",
        ),
        topology.replace("Cargo.toml ro,nosuid", "Cargo.toml rw,nosuid"),
        topology.replace(
            "core rw,nosuid,nodev - overlay overlay rw,userxattr",
            "core rw,nosuid,nodev - tmpfs tmpfs rw",
        ),
        topology
            .lines()
            .filter(|line| !line.contains("/crates/core"))
            .map(|line| format!("{line}\n"))
            .collect(),
        format!(
            "{topology}7 2 0:6 / {}/sibling ro - ext4 /dev/disk rw\n",
            workspace.display()
        ),
        format!(
            "{topology}7 2 0:6 / {}/src/unexpected ro - ext4 /dev/disk rw\n",
            workspace.display()
        ),
    ] {
        assert!(verify(&invalid).is_err());
    }
    std::fs::rename(workspace.join("Cargo.toml"), workspace.join("old")).unwrap();
    std::fs::write(workspace.join("Cargo.toml"), b"replacement").unwrap();
    assert!(verify(&topology).is_err());
}

#[test]
fn grants_reject_file_directory_and_ancestor_replacement_without_widening_mounts() {
    for replaced in ["file", "directory", "ancestor", "link"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("nested/src")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), b"manifest").unwrap();
        let runtime = crate::tools::tests::in_process_runtime(
            root.path(),
            serde_json::json!({
            "version":1,"workspace_paths":["nested/src","Cargo.toml"],"write":true,
            "commands":[],"skills":[],"mcp":[]}),
        );
        let intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            ToolCall::Command {
                program: "shell".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
        );
        let grant = Grant::new(&runtime, &intent).unwrap();
        assert!(grant.verify().is_ok());
        let mut command = Command::new("/usr/bin/bwrap");
        grant.arguments(&mut command, Path::new("/private"));
        let arguments = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--bind",
                "/private/namespace/grant-0/merged",
                "/workspace/nested/src",
                "--ro-bind",
                "/private/namespace/grant-1/file",
                "/workspace/Cargo.toml",
                "--remount-ro",
                "/workspace"
            ]
        );
        let path = root.path().join(match replaced {
            "file" => "Cargo.toml",
            "ancestor" => "nested",
            _ => "nested/src",
        });
        std::fs::rename(&path, root.path().join("old")).unwrap();
        match replaced {
            "file" => std::fs::write(&path, b"replacement").unwrap(),
            "ancestor" => std::fs::create_dir_all(path.join("src")).unwrap(),
            "link" => std::os::unix::fs::symlink(root.path().join("old"), &path).unwrap(),
            _ => std::fs::create_dir(&path).unwrap(),
        }
        assert!(grant.verify().is_err(), "{replaced}");
    }
}

#[test]
fn inventory_budget_and_protected_alias_refusals_span_grants() {
    let directory = tempfile::tempdir().unwrap();
    let mut inventories = Vec::new();
    let mut budget = crate::tools::fs::CommandScanBudget::default();
    for _ in 0..crate::tools::types::COMMAND_SCAN_ENTRIES - 1 {
        budget.entry("entry").unwrap();
    }
    let deadline = std::time::Instant::now() + crate::tools::selection::PHASE_TIME;
    let mut selector = crate::tools::selection::Selector::new(0, deadline);
    for name in ["first", "second"] {
        let path = directory.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("input"), b"input").unwrap();
        let lower = crate::tools::fs::open_directory(&path).unwrap();
        inventories.push(
            masks::Inventory::read_shared(
                &lower,
                (name, masks::MASK_ARGUMENT_BYTES),
                &mut budget,
                &mut selector,
                deadline,
            )
            .unwrap(),
        );
    }
    assert!(inventories[0].paths().is_empty());
    assert_eq!(inventories[1].paths(), [PathBuf::new()]);
    let protected = directory.path().join("second/.env");
    std::fs::write(&protected, b"protected").unwrap();
    std::fs::hard_link(&protected, directory.path().join("first/alias")).unwrap();
    let metadata = std::fs::metadata(&protected).unwrap();
    let roots = vec![Root {
        relative: "alias".into(),
        device: metadata.dev(),
        inode: metadata.ino(),
        directory: false,
    }];
    let views = ["first", "second"]
        .into_iter()
        .map(|name| {
            let root = directory.path().join(name);
            let lower = crate::tools::fs::open_directory(&root).unwrap();
            DirectoryView {
                root,
                grant: Root {
                    relative: name.into(),
                    device: 0,
                    inode: 0,
                    directory: true,
                },
                inventory: masks::Inventory::read(&lower, deadline).unwrap(),
                masks: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    assert!(!masks::Inventory::aliases_safe(&views, &[]));
    assert!(!masks::Inventory::aliases_safe(&views[1..], &roots));
}
