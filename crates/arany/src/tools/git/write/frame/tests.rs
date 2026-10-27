use super::*;

fn empty_plan() -> Plan {
    Plan {
        identity: (12, 34),
        refs: RefDigests {
            head: digest(b"ref: refs/heads/main\n"),
            packed: None,
            loose: Vec::new(),
        },
        index_before: Some(digest(b"original index")),
        publication: Publication::default(),
        objects: Vec::new(),
    }
}

fn long_path(prefix: &str, length: usize, index: usize) -> String {
    let suffix = format!("/{index:08}");
    let mut path = prefix.to_owned();
    while path.len() + suffix.len() < length {
        let count = (length - path.len() - suffix.len()).min(240);
        if count > 1 {
            path.push('/');
            path.push_str(&"a".repeat(count - 1));
        } else {
            path.push('a');
        }
    }
    path.push_str(&suffix);
    assert_eq!(path.len(), length);
    path
}

fn validate(plan: &Plan) -> Result<Plan, ToolError> {
    decode(&encode(plan)?)
}

fn round_trip(plan: &Plan) -> Plan {
    let original = digest(&encode(plan).unwrap());
    let mut framed = Vec::new();
    write(&mut framed, plan).unwrap();
    let mut remaining = framed.as_slice();
    let decoded = read(&mut remaining).unwrap();
    assert!(remaining.is_empty());
    assert_eq!(digest(&encode(&decoded).unwrap()), original);
    decoded
}

#[test]
fn largest_switch_and_long_paths_fit_the_derived_frame_bound() {
    let mut plan = empty_plan();
    plan.publication.index = Some(vec![b'i'; MAX_SNAPSHOT_BYTES]);
    let contents = vec![b'b'; MAX_FILE_BYTES];
    for index in 0..MAX_SNAPSHOT_FILES {
        let after = (index < MAX_SNAPSHOT_BYTES / MAX_FILE_BYTES)
            .then(|| FileVersion::from_bytes(&contents, index % 2 == 0));
        plan.publication.files.push(FileChange {
            path: long_path("source", MAX_PATH_BYTES, index),
            before: Some(FileVersion {
                sha256: hex_digest(b"original"),
                content: String::new(),
                executable: false,
            }),
            after,
        });
    }
    for index in 0..MAX_REF_ENTRIES {
        plan.refs.loose.push((
            long_path("refs/heads", MAX_PATH_BYTES, index),
            digest(b"tip"),
        ));
    }
    for index in 0..MAX_HOLDS {
        plan.publication
            .holds
            .push(long_path("refs/heads", MAX_PATH_BYTES, index));
    }
    plan.publication.head = Some(b"ref: refs/heads/target\n".to_vec());
    let decoded = round_trip(&plan);
    assert_eq!(
        decoded.publication.index.as_ref().unwrap().len(),
        MAX_SNAPSHOT_BYTES
    );
    assert_eq!(decoded.publication.files.len(), MAX_SNAPSHOT_FILES);
    drop(decoded);
    let payload_bytes = encode(&plan).unwrap().len();
    let fixed_bytes = 2 * size_of::<u64>()
        + HASH_BYTES
        + (LENGTH_BYTES + HASH_BYTES)
        + 8 * LENGTH_BYTES
        + plan.publication.head.as_ref().unwrap().len();
    let refs_bytes = MAX_REF_ENTRIES * (LENGTH_BYTES + MAX_PATH_BYTES + HASH_BYTES);
    let files_bytes = MAX_SNAPSHOT_FILES * (MAX_PATH_BYTES + HASH_BYTES + 4 * LENGTH_BYTES)
        + (MAX_SNAPSHOT_BYTES / MAX_FILE_BYTES) * (2 * LENGTH_BYTES);
    let holds_bytes = MAX_HOLDS * (MAX_PATH_BYTES + LENGTH_BYTES);
    assert_eq!(
        payload_bytes,
        2 * MAX_SNAPSHOT_BYTES + fixed_bytes + refs_bytes + files_bytes + holds_bytes
    );
    assert!(payload_bytes <= MAX_FRAME_BYTES);
    assert!(payload_bytes > crate::session::worker::WORKER_DATA_BYTES);

    plan.publication.files.last_mut().unwrap().after = Some(FileVersion::from_bytes(b"x", false));
    assert!(
        matches!(validate(&plan), Err(ToolError::Limit)),
        "one switch byte over"
    );
    plan.publication.files.last_mut().unwrap().after = None;
    plan.publication.files.push(FileChange {
        path: "extra".into(),
        before: None,
        after: None,
    });
    assert!(
        matches!(validate(&plan), Err(ToolError::Limit)),
        "one switch entry over"
    );
    plan.publication.files.pop();
    plan.refs
        .loose
        .push(("refs/heads/overflow".into(), digest(b"tip")));
    assert!(
        matches!(validate(&plan), Err(ToolError::Limit)),
        "one snapshot ref over"
    );
}

#[test]
fn largest_commit_objects_and_index_share_the_existing_bound() {
    let mut plan = empty_plan();
    plan.publication.index = Some(vec![b'i']);
    for index in 0..MAX_OBJECTS {
        let oid = format!("{index:040x}");
        plan.publication
            .objects
            .push(format!("{}/{}", &oid[..2], &oid[2..]));
        let count = if index + 1 == MAX_OBJECTS {
            MAX_SNAPSHOT_BYTES - MAX_OBJECTS
        } else {
            1
        };
        plan.objects.push(vec![b'o'; count]);
    }
    plan.publication.reference = Some(RefUpdate {
        name: "refs/heads/main".into(),
        before: Some("1".repeat(40)),
        after: "2".repeat(40),
    });
    let decoded = round_trip(&plan);
    assert_eq!(decoded.objects.len(), MAX_OBJECTS);
    assert_eq!(
        decoded.objects.iter().map(Vec::len).sum::<usize>() + 1,
        MAX_SNAPSHOT_BYTES
    );
    drop(decoded);
    plan.publication.index.as_mut().unwrap().push(b'i');
    assert!(
        matches!(validate(&plan), Err(ToolError::Limit)),
        "one object/index byte over"
    );
    plan.publication.index.as_mut().unwrap().pop();
    plan.publication
        .objects
        .push(format!("ff/{}", "f".repeat(38)));
    plan.objects.push(Vec::new());
    assert!(
        matches!(validate(&plan), Err(ToolError::Limit)),
        "one object over"
    );
}

#[test]
fn malformed_frames_never_reach_publication() {
    let plan = empty_plan();
    let mut frame = Vec::new();
    write(&mut frame, &plan).unwrap();
    for length in [0, 4, 35, frame.len() - 1] {
        assert!(
            read(&mut &frame[..length]).is_err(),
            "truncation at {length}"
        );
    }
    let mut corrupted = frame.clone();
    corrupted[4] ^= 1;
    assert!(matches!(
        read(&mut corrupted.as_slice()),
        Err(ToolError::ChangedInput)
    ));
    let over = u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_le_bytes();
    assert!(matches!(read(&mut over.as_slice()), Err(ToolError::Limit)));
    let worker_over = u32::try_from(crate::session::worker::WORKER_DATA_BYTES + 1)
        .unwrap()
        .to_le_bytes();
    assert!(matches!(
        transfer::read_frame(
            &mut worker_over.as_slice(),
            crate::session::worker::WORKER_DATA_BYTES
        ),
        Err(ToolError::Limit)
    ));
    let mut trailing = encode(&plan).unwrap();
    trailing.push(0);
    assert!(matches!(decode(&trailing), Err(ToolError::Configuration)));
    let mut plan = empty_plan();
    plan.publication
        .objects
        .push(format!("zz/{}", "a".repeat(38)));
    plan.objects.push(Vec::new());
    assert!(validate(&plan).is_err());
    plan.publication.objects.clear();
    plan.objects.clear();
    plan.publication.holds.push("refs/../config".into());
    assert!(validate(&plan).is_err());
    plan.publication.holds.clear();
    plan.publication.files.push(FileChange {
        path: "source".repeat(MAX_PATH_BYTES),
        before: None,
        after: None,
    });
    assert!(matches!(validate(&plan), Err(ToolError::Limit)));
    plan.publication.files.clear();
    for path in [
        "../outside",
        ".git/config",
        "source//file",
        "source/./file",
        "/absolute",
    ] {
        plan.publication.files.push(FileChange {
            path: path.into(),
            before: None,
            after: None,
        });
        assert!(validate(&plan).is_err(), "unsafe frame path: {path}");
        plan.publication.files.clear();
    }
    plan.publication.files.push(FileChange {
        path: "source".into(),
        before: None,
        after: Some(FileVersion::from_bytes(
            &vec![b'b'; MAX_FILE_BYTES + 1],
            false,
        )),
    });
    assert!(matches!(validate(&plan), Err(ToolError::Limit)));
}

#[test]
fn identical_replacement_git_directory_is_refused_before_any_lock() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let original = workspace.join(".git");
    std::fs::create_dir_all(original.join("refs/heads")).unwrap();
    std::fs::write(original.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    let dir = Dir::open_ambient_dir(&original, cap_std::ambient_authority()).unwrap();
    let mut plan = empty_plan();
    plan.identity = crate::tools::fs::identity(&dir).unwrap();
    plan.index_before = None;
    plan.publication.reference = Some(RefUpdate {
        name: "refs/heads/created".into(),
        before: None,
        after: "1".repeat(40),
    });
    let decoded = round_trip(&plan);
    std::fs::rename(&original, workspace.join("original-git")).unwrap();
    std::fs::create_dir_all(original.join("refs/heads")).unwrap();
    std::fs::write(original.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    // A planted ledger is untouched: refusal precedes even ledger creation.
    let intent = uuid::Uuid::now_v7();
    let ledger = original.join(super::super::ledger_name(intent));
    std::fs::write(&ledger, b"untouched").unwrap();
    std::fs::write(
        workspace
            .join("original-git")
            .join(super::super::ledger_name(intent)),
        b"untouched",
    )
    .unwrap();
    let result = super::super::publish(&workspace, &decoded, intent).unwrap();
    assert_eq!(result.unwrap_err(), DRIFT);
    assert_eq!(std::fs::read(&ledger).unwrap(), b"untouched");
    assert!(
        std::fs::read_dir(original.join("refs/heads"))
            .unwrap()
            .next()
            .is_none()
    );
    assert_eq!(
        std::fs::read(original.join("HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
}
