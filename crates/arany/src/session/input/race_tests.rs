use super::{InputError, WorkspaceInputs};
use crate::store::StateRoot;
use rustix::fs::{CWD, RenameFlags, renameat_with};
use std::{
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{
        Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

const SAFE: &[u8] = b"SAFE_WORKSPACE_FACTS";
const OUTSIDE: &[u8] = b"OUTSIDE_WORKSPACE_CANARY";
const LOAD_ATTEMPTS: usize = 2_048;
const MAX_SWAPS: usize = 100_000;

#[derive(Clone, Copy, Debug)]
enum Position {
    FirstDirectory,
    NestedDirectory,
    FinalFile,
}

fn exchange(active: &Path, spare: &Path) {
    renameat_with(CWD, active, CWD, spare, RenameFlags::EXCHANGE)
        .expect("atomic real-object/symlink exchange");
}

fn run_case(position: Position) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    let first = workspace.join("first");
    let nested = first.join("second");
    let selected = nested.join("facts.txt");
    std::fs::create_dir_all(&nested).expect("safe include path");
    std::fs::write(&selected, SAFE).expect("safe include bytes");

    let outside = temp.path().join("outside");
    std::fs::create_dir_all(outside.join("second")).expect("outside directory");
    std::fs::write(outside.join("facts.txt"), OUTSIDE).expect("outside file");
    std::fs::write(outside.join("second/facts.txt"), OUTSIDE).expect("outside nested file");
    let (active, spare, target) = match position {
        Position::FirstDirectory => (first, workspace.join("spare"), outside.clone()),
        Position::NestedDirectory => (nested, first.join("spare"), outside.clone()),
        Position::FinalFile => (selected, nested.join("spare"), outside.join("facts.txt")),
    };
    symlink(target, &spare).expect("outside-pointing symlink");

    let state = StateRoot::admit(&temp.path().join("state")).expect("private State");
    let include = [PathBuf::from("first/second/facts.txt")];
    let safe = WorkspaceInputs::load(&workspace, &state, &include).expect("stable safe input");
    assert!(safe.instructions.is_none());
    assert_eq!(safe.includes.len(), 1);
    assert!(safe.includes[0].content.as_bytes() == SAFE);
    exchange(&active, &spare);
    assert!(
        matches!(
            WorkspaceInputs::load(&workspace, &state, &include),
            Err(InputError::Io(_))
        ),
        "{position:?}: stable symlink must be rejected"
    );
    exchange(&active, &spare);

    let start = Barrier::new(2);
    let stop = AtomicBool::new(false);
    let swaps = AtomicUsize::new(0);
    thread::scope(|scope| {
        let attacker = scope.spawn(|| {
            start.wait();
            for _ in 0..MAX_SWAPS {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                exchange(&active, &spare);
                swaps.fetch_add(1, Ordering::Release);
                thread::yield_now();
            }
        });
        start.wait();
        let mut safe_count = 0;
        let mut rejected_count = 0;
        let mut overlapped_count = 0;
        for _ in 0..LOAD_ATTEMPTS {
            let before = swaps.load(Ordering::Acquire);
            match WorkspaceInputs::load(&workspace, &state, &include) {
                Ok(input) => {
                    assert!(input.instructions.is_none(), "{position:?}: extra guidance");
                    assert_eq!(input.includes.len(), 1, "{position:?}: include count");
                    assert!(
                        input.includes[0].content.as_bytes() == SAFE,
                        "{position:?}: outside bytes escaped the pinned root"
                    );
                    safe_count += 1;
                }
                Err(InputError::Io(_)) => rejected_count += 1,
                Err(error) => panic!("{position:?}: unexpected input error {error:?}"),
            }
            if swaps.load(Ordering::Acquire) > before {
                overlapped_count += 1;
            }
            thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        attacker.join().expect("attacker thread");
        assert!(swaps.load(Ordering::Acquire) > 0, "{position:?}: no swaps");
        assert!(overlapped_count > 0, "{position:?}: no overlapping loads");
        assert!(safe_count > 0, "{position:?}: no safe snapshots");
        assert!(rejected_count > 0, "{position:?}: no rejected symlinks");
    });
}

#[test]
#[ignore = "native Linux Workspace component-replacement release gate"]
fn concurrent_component_exchange_never_reads_outside_workspace() {
    for position in [
        Position::FirstDirectory,
        Position::NestedDirectory,
        Position::FinalFile,
    ] {
        run_case(position);
    }
}
