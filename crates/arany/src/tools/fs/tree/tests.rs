use super::*;

fn tree(files: &[&str]) -> (tempfile::TempDir, Dir) {
    let temp = tempfile::tempdir().unwrap();
    for file in files {
        let path = temp.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, file).unwrap();
    }
    let dir = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
    (temp, dir)
}

fn later() -> Instant {
    Instant::now() + std::time::Duration::from_secs(30)
}

#[test]
fn walks_merge_in_component_order_and_skip_subtrees() {
    let (_left_temp, left) = tree(&["a/x", "a-c", "b/old", "same", "gone/deep/file"]);
    let (_right_temp, right) = tree(&["a/x", "a-c", "b/new", "same", "ignored/file"]);
    let mut seen = Vec::new();
    diff(
        &mut Walk::new(&left, later()).unwrap(),
        &mut Walk::new(&right, later()).unwrap(),
        |pair| {
            let (side, path) = match pair {
                Pair::Left(node) => ("left", node.path),
                Pair::Right(node) => ("right", node.path),
                Pair::Both(node, _) => ("both", node.path),
            };
            let descend = path != "ignored";
            seen.push(format!("{side} {path}"));
            Ok(descend)
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        [
            "both a",
            "both a/x",
            "both a-c",
            "both b",
            "right b/new",
            "left b/old",
            "left gone",
            "left gone/deep",
            "left gone/deep/file",
            "right ignored",
            "both same",
        ]
    );
}

#[test]
fn walks_stop_at_the_deadline_and_held_name_bound() {
    let (_temp, root) = tree(&["a"]);
    assert!(matches!(
        Walk::new(&root, Instant::now()),
        Err(ToolError::Timeout(_))
    ));
    let mut walk = Walk::new(&root, later()).unwrap();
    walk.held = HELD_NAME_BYTES;
    walk.levels.clear();
    assert!(matches!(walk.push(String::new()), Err(ToolError::Limit)));
}
