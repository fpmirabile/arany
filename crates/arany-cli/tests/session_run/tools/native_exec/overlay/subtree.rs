use super::*;

pub(super) fn prepare(workspace: &Path, case: OverlayCase) {
    if !matches!(
        case,
        OverlayCase::Recreation
            | OverlayCase::RecreationNonzero
            | OverlayCase::SubtreeDeletion
            | OverlayCase::SubtreeDrift
    ) {
        return;
    }
    std::fs::create_dir(workspace.join("src/nested")).unwrap();
    private_file(&workspace.join("src/nested/removed"), b"before");
    private_file(&workspace.join("src/kept"), b"unchanged");
}

pub(super) fn recreate(case: OverlayCase) -> &'static str {
    if case == OverlayCase::RecreationNonzero {
        "set -eu; rm -rf src; mkdir src; printf '\\000\\377' > src/current.txt; chmod 700 src/current.txt; printf unchanged > src/kept; exit 7"
    } else {
        "set -eu; rm -rf src; mkdir src; printf '\\000\\377' > src/current.txt; chmod 700 src/current.txt; printf unchanged > src/kept"
    }
}

pub(super) fn check(workspace: &Path, run: &arany::RunView, case: OverlayCase) {
    match case {
        OverlayCase::SubtreeDrift => {
            assert_eq!(
                std::fs::read(workspace.join("src/nested/host")).unwrap(),
                b"host"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/nested/removed")).unwrap(),
                b"before"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/kept")).unwrap(),
                b"unchanged"
            );
            return;
        }
        OverlayCase::Recreation | OverlayCase::RecreationNonzero | OverlayCase::SubtreeDeletion => {
        }
        _ => return,
    }
    assert_eq!(
        run.tools[1].checkpoint.as_ref().unwrap().status,
        arany::CheckpointStatus::Captured
    );
    let ToolCall::ApplyChangeset {
        source:
            arany::worker::ChangesetSource::CommandReceipt {
                source_intent_digest,
                changeset,
            },
    } = &run.tools[1].intent.call
    else {
        panic!("expected subtree integration")
    };
    let observation = run.tools[0].observation.as_ref().unwrap();
    let digest: [u8; 32] = Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
    assert_eq!(*source_intent_digest, digest);
    assert!(!workspace.join("src/nested/removed").exists());
    assert_eq!(
        changeset.files.len(),
        if case == OverlayCase::SubtreeDeletion {
            3
        } else {
            2
        }
    );
    let removed = changeset
        .files
        .iter()
        .find(|file| file.path == "src/nested/removed")
        .unwrap();
    assert_eq!(removed.before.as_ref().unwrap().bytes().unwrap(), b"before");
    assert!(removed.after.is_none());
    if case == OverlayCase::SubtreeDeletion {
        assert!(changeset.files.iter().all(|file| file.after.is_none()));
    } else {
        check_recreated(workspace, changeset);
    }
}

fn check_recreated(workspace: &Path, changeset: &arany::CommandChangeset) {
    assert_eq!(
        std::fs::read(workspace.join("src/kept")).unwrap(),
        b"unchanged"
    );
    let recreated = changeset
        .files
        .iter()
        .find(|file| file.path == "src/current.txt")
        .unwrap();
    assert_eq!(
        recreated.before.as_ref().unwrap().bytes().unwrap(),
        b"current"
    );
    assert_eq!(recreated.after.as_ref().unwrap().bytes().unwrap(), [0, 255]);
    assert!(recreated.after.as_ref().unwrap().executable);
    assert_ne!(
        std::fs::metadata(workspace.join("src/current.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
}
