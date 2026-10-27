use super::*;

#[test]
fn descendant_mounts_refuse_but_a_grant_root_mount_is_admitted() {
    let mounts = parse_mountinfo(b"1 0 0:1 / / rw - tmpfs tmpfs rw\n2 1 0:2 / /project rw - tmpfs tmpfs rw\n3 2 0:3 / /project/target rw - tmpfs tmpfs rw\n").unwrap();
    assert!(no_submounts(&mounts, Path::new("/project")).is_err());
    assert!(no_submounts(&mounts, Path::new("/project/target")).is_ok());
    assert!(no_submounts(&mounts, Path::new("/project-other")).is_ok());
}

#[test]
fn masks_keep_dependencies_and_large_inputs_but_refuse_known_protected_aliases() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    std::fs::create_dir_all(path.join("node_modules/package/deep")).unwrap();
    std::fs::write(
        path.join("node_modules/package/deep/.ENV.local"),
        b"protected",
    )
    .unwrap();
    std::fs::write(path.join("node_modules/package/input"), b"dependency").unwrap();
    let large = std::fs::File::create(path.join("large")).unwrap();
    large
        .set_len(crate::tools::types::MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    std::os::unix::fs::symlink("package/input", path.join("node_modules/link")).unwrap();
    let lower = crate::tools::fs::open_directory(path).unwrap();
    let deadline = std::time::Instant::now() + crate::tools::selection::PHASE_TIME;
    let inventory = masks::Inventory::read(&lower, deadline).unwrap();
    assert_eq!(
        inventory.paths(),
        vec![PathBuf::from("node_modules/package/deep/.ENV.local")]
    );
    std::fs::hard_link(
        path.join("node_modules/package/deep/.ENV.local"),
        path.join("alias"),
    )
    .unwrap();
    assert!(masks::Inventory::read(&lower, deadline).is_err());
}

#[test]
fn protected_links_and_unwalkable_trees_mask_an_enclosing_subtree() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    std::fs::create_dir(path.join("nested")).unwrap();
    std::fs::write(path.join("target"), b"protected target").unwrap();
    std::os::unix::fs::symlink("../target", path.join("nested/.env")).unwrap();
    let lower = crate::tools::fs::open_directory(path).unwrap();
    let deadline = std::time::Instant::now() + crate::tools::selection::PHASE_TIME;
    let inventory = masks::Inventory::read(&lower, deadline).unwrap();
    assert_eq!(
        inventory.paths(),
        vec![PathBuf::from("nested"), PathBuf::from("target")]
    );
    std::fs::remove_file(path.join("nested/.env")).unwrap();
    std::os::unix::fs::symlink("/outside", path.join("nested/.env")).unwrap();
    assert!(inventory.verify(path).is_err());
    let inventory = masks::Inventory::read(&lower, deadline).unwrap();
    assert_eq!(inventory.paths(), vec![PathBuf::from("nested")]);
    std::fs::create_dir_all(
        path.join(vec!["deep"; crate::tools::types::COMMAND_SNAPSHOT_DEPTH + 1].join("/")),
    )
    .unwrap();
    let inventory = masks::Inventory::read(&lower, deadline).unwrap();
    assert!(
        inventory
            .paths()
            .iter()
            .any(|path| path.components().count() == crate::tools::types::COMMAND_SNAPSHOT_DEPTH)
    );
    assert!(masks::Inventory::read(&lower, std::time::Instant::now()).is_err());
}

#[test]
fn overlay_topology_rejects_wrong_types_writable_masks_and_extra_mounts() {
    let topology = b"1 0 0:1 / / ro - tmpfs tmpfs ro\n2 1 0:2 / /workspace rw,nosuid,nodev - overlay overlay rw,userxattr\n3 2 0:3 /empty-file /workspace/.env ro,nosuid,nodev - tmpfs tmpfs rw\n";
    let masks = [PathBuf::from(".env")];
    assert!(
        verify_overlay(
            &parse_mountinfo(topology).unwrap(),
            Path::new("/workspace"),
            &masks,
            Path::new("/"),
        )
        .is_ok()
    );
    let text = std::str::from_utf8(topology).unwrap();
    for invalid in [
        text.replace("overlay overlay", "tmpfs tmpfs"),
        text.replace("ro,nosuid", "rw,nosuid"),
        text.replace("/empty-file", "/other-grant/empty-file"),
        text.replace(",userxattr", ""),
        format!("{text}4 2 0:4 / /workspace/extra rw - tmpfs tmpfs rw\n"),
    ] {
        assert!(
            verify_overlay(
                &parse_mountinfo(invalid.as_bytes()).unwrap(),
                Path::new("/workspace"),
                &masks,
                Path::new("/"),
            )
            .is_err()
        );
    }
}

#[test]
fn overflowing_mask_arguments_replace_the_entire_enclosing_view() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..crate::tools::config::MAX_WORKSPACE_PATHS * 4 {
        std::fs::write(
            root.path().join(format!(".env.protected-variable-{index}")),
            b"protected",
        )
        .unwrap();
    }
    let lower = crate::tools::fs::open_directory(root.path()).unwrap();
    let inventory = masks::Inventory::read(
        &lower,
        std::time::Instant::now() + crate::tools::selection::PHASE_TIME,
    )
    .unwrap();
    assert_eq!(inventory.paths(), vec![PathBuf::new()]);
    let deadline = std::time::Instant::now() + crate::tools::selection::PHASE_TIME;
    let allowance = serde_json::to_vec(&["narrow/"]).unwrap().len();
    let inventory = masks::Inventory::read_shared(
        &lower,
        ("narrow", allowance),
        &mut Default::default(),
        &mut crate::tools::selection::Selector::new(0, deadline),
        deadline,
    )
    .unwrap();
    assert_eq!(inventory.paths(), vec![PathBuf::new()]);
    assert!(inventory.argument_bytes().unwrap() <= allowance);
}

#[tokio::test]
async fn completion_is_correlated_and_refusal_preserves_confirmed_stops() {
    use crate::tools::{ToolDisposition, guard::CacheCandidate};
    let parent = profile::Namespaces::read().unwrap();
    let mut namespaces = serde_json::to_value(&parent).unwrap();
    for name in ["mnt", "user"] {
        namespaces[name] = (namespaces[name].as_u64().unwrap() + 1).into();
    }
    let namespaces: profile::Namespaces = serde_json::from_value(namespaces).unwrap();
    for disposition in [
        ToolDisposition::Succeeded,
        ToolDisposition::Limit,
        ToolDisposition::Cancelled,
        ToolDisposition::Denied,
        ToolDisposition::Failed,
        ToolDisposition::Conflict,
        ToolDisposition::Uncertain,
    ] {
        let mut observation = crate::tools::guard::tests::command_observation();
        let intent = observation.intent.clone();
        observation.disposition = disposition;
        let mut candidate = CacheCandidate::Received;
        let report = Completion {
            digest: intent.digest(),
            namespaces: namespaces.clone(),
            capture: Err(CaptureFailure::Rejected),
        };
        let mut bytes = serde_json::to_vec(&report).unwrap();
        bytes.push(b'\n');
        complete(
            &mut bytes.as_slice(),
            (&intent, &parent),
            &mut observation,
            &mut candidate,
            crate::tools::process::Truncation::Allow,
        )
        .await
        .unwrap();
        assert_eq!(
            observation.disposition,
            if disposition == ToolDisposition::Succeeded {
                ToolDisposition::Denied
            } else {
                disposition
            }
        );
        assert!(
            candidate
                == if disposition == ToolDisposition::Uncertain {
                    CacheCandidate::Uncertain
                } else {
                    CacheCandidate::Rejected
                }
        );
        for invalid in [
            serde_json::json!({"digest":[0],"namespaces":parent,"capture":{"Err":"Rejected"}}),
            serde_json::json!({"digest":intent.digest(),"namespaces":parent,"capture":{"Err":"Rejected"}}),
        ] {
            let mut bytes = serde_json::to_vec(&invalid).unwrap();
            bytes.push(b'\n');
            assert!(
                complete(
                    &mut bytes.as_slice(),
                    (&intent, &parent),
                    &mut observation,
                    &mut candidate,
                    crate::tools::process::Truncation::Allow,
                )
                .await
                .is_err()
            );
        }
    }
}

#[tokio::test]
async fn completion_merges_candidates_with_totals_and_keeps_hook_output_fail_closed() {
    use crate::tools::types::{
        CommandChangeset, CommandResult, CommandSelectionSummary, FileChange, FileVersion,
    };
    let parent = profile::Namespaces::read().unwrap();
    let mut namespaces = serde_json::to_value(&parent).unwrap();
    for name in ["mnt", "user"] {
        namespaces[name] = (namespaces[name].as_u64().unwrap() + 1).into();
    }
    let namespaces: profile::Namespaces = serde_json::from_value(namespaces).unwrap();
    for hook in [false, true] {
        let mut observation = crate::tools::guard::tests::command_observation();
        let intent = observation.intent.clone();
        let mut original: CommandResult = serde_json::from_str(&observation.output).unwrap();
        original.stdout = "x".repeat(intent.limits.result_bytes as usize / 2);
        original.stdout_total = crate::tools::types::OutputTotal {
            bytes: original.stdout.len() as u64,
            lines: 1,
        };
        observation.output = serde_json::to_string(&original).unwrap();
        assert!(!crate::tools::spill::has_excerpt(&observation));
        observation.spill = crate::tools::spill::OutputSpill::pending(
            &observation,
            Some(original.stdout.as_bytes().to_vec()),
        );
        observation = crate::tools::spill::decode_report(
            &crate::tools::spill::encode_report(&observation).unwrap(),
        )
        .unwrap();
        let changeset = CommandChangeset {
            files: vec![FileChange {
                path: "created".into(),
                before: None,
                after: Some(FileVersion::from_bytes(
                    &vec![b'a'; intent.limits.result_bytes as usize / 2],
                    false,
                )),
            }],
        };
        let selection = CommandSelectionSummary {
            ignored_entries: 3,
            discarded_outputs: 2,
            ..Default::default()
        };
        let report = Completion {
            digest: intent.digest(),
            namespaces: namespaces.clone(),
            capture: Ok((changeset.clone(), selection)),
        };
        let mut bytes = serde_json::to_vec(&report).unwrap();
        bytes.push(b'\n');
        let mut candidate = super::super::super::CacheCandidate::Received;
        complete(
            &mut bytes.as_slice(),
            (&intent, &parent),
            &mut observation,
            &mut candidate,
            if hook {
                crate::tools::process::Truncation::Refuse
            } else {
                crate::tools::process::Truncation::Allow
            },
        )
        .await
        .unwrap();
        if hook {
            assert_eq!(
                observation.disposition,
                crate::tools::ToolDisposition::Limit
            );
            assert_eq!(
                observation.limit_cause,
                Some(crate::tools::LimitCause::Resource)
            );
        } else {
            let result: CommandResult = serde_json::from_str(&observation.output).unwrap();
            assert_eq!(result.changeset, changeset);
            assert_eq!(result.selection, Some(selection));
            assert_eq!(result.stdout_total, original.stdout_total);
            assert!(crate::tools::spill::has_excerpt(&observation));
            assert_eq!(
                observation.spill.as_ref().unwrap().captured.as_deref(),
                Some(original.stdout.as_bytes())
            );
            assert!(observation.output.len() <= intent.limits.result_bytes as usize);
        }
    }
}
