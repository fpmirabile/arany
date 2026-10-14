use std::process::Command;

#[cfg(target_os = "linux")]
pub fn isolated_script(root: &std::path::Path) -> Command {
    script(root, true)
}

#[cfg(target_os = "linux")]
pub fn account_isolated_script(root: &std::path::Path) -> Command {
    script(root, false)
}

#[cfg(target_os = "linux")]
fn script(root: &std::path::Path, isolate_network: bool) -> Command {
    let product = std::path::Path::new(env!("CARGO_BIN_EXE_arany"));
    let mut command = account_command(root, isolate_network, product);
    command.arg("/usr/bin/script");
    command
}

#[cfg(target_os = "linux")]
pub fn account_isolated_command(
    root: &std::path::Path,
    executable: &str,
    product: &std::path::Path,
) -> Command {
    let mut command = account_command(root, false, product);
    command.arg(executable);
    command
}

#[cfg(target_os = "linux")]
pub fn native_account_isolated_test_command(
    root: &std::path::Path,
    test_executable: &std::path::Path,
    guard_executable: &std::path::Path,
) -> Command {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    assert!(
        test_executable.is_absolute() && test_executable.is_file(),
        "isolated test executable must be an absolute file"
    );
    assert!(
        guard_executable.is_absolute() && guard_executable.is_file(),
        "isolated Guard executable must be an absolute file"
    );
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    assert_ne!(
        user.uid.as_raw(),
        0,
        "native fixture requires an ordinary user"
    );
    assert!(root.is_absolute() && root.join("account-home").is_dir());
    let mut passwd = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("passwd"))
        .expect("exclusive native fixture passwd file");
    writeln!(
        passwd,
        "arany-fixture:x:{}:{}:Arany native fixture:{}:/usr/bin/sh",
        user.uid.as_raw(),
        user.gid.as_raw(),
        root.join("account-home").display()
    )
    .expect("private native fixture passwd entry");
    passwd.sync_all().expect("native fixture passwd sync");
    let mut command = Command::new("/usr/bin/sudo");
    command
        .args([
            "--non-interactive", "--", "/usr/bin/unshare", "--mount", "--net",
            "--propagation", "private", "--", "/usr/bin/sh", "-eu", "-c",
            "root=$1; uid=$2; gid=$3; shift 3; /usr/bin/mount --bind \"$root/passwd\" /etc/passwd; /usr/bin/mount --options remount,bind,ro /etc/passwd; exec /usr/bin/timeout --kill-after=2s 80s /usr/bin/setpriv --reuid \"$uid\" --regid \"$gid\" --clear-groups --no-new-privs /usr/bin/env -i HOME=\"$root/home\" XDG_STATE_HOME=\"$root/environment-state\" XDG_DATA_HOME=\"$root/environment-data\" PATH=/usr/bin:/bin ARANY_TEST_CACHE_CANARY_ROOT=\"$root\" \"$@\"",
            "arany-native-account-fixture",
        ])
        .arg(root)
        .arg(user.uid.as_raw().to_string())
        .arg(user.gid.as_raw().to_string())
        .arg(test_executable);
    command
}

#[cfg(target_os = "linux")]
fn account_command(
    root: &std::path::Path,
    isolate_network: bool,
    product: &std::path::Path,
) -> Command {
    use std::os::unix::fs::PermissionsExt;
    let home = root.join("account-home");
    std::fs::create_dir_all(&home).expect("isolated account home");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .args([
            "--unshare-user",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
        ])
        .arg("--ro-bind")
        .arg(product)
        .arg("/arany")
        .args(["--ro-bind", "/etc/passwd", "/etc/passwd"])
        .arg("--bind")
        .arg(root)
        .arg(root)
        .arg("--bind")
        .arg(&home)
        .arg(user.dir)
        .arg("--ro-bind")
        .arg(product)
        .arg(product)
        .args(["--proc", "/proc", "--dev-bind", "/dev", "/dev"]);
    if isolate_network {
        command.arg("--unshare-net");
    }
    if std::path::Path::new("/etc/ssl").is_dir() {
        command.args(["--ro-bind", "/etc/ssl", "/etc/ssl"]);
    }
    command.args([
        "--", "/usr/bin/sh", "-c",
        "HOME=${HOME:-\"$1\"}; XDG_STATE_HOME=${XDG_STATE_HOME:-\"$1/fixture-state\"}; XDG_DATA_HOME=${XDG_DATA_HOME:-\"$1/fixture-data\"}; export HOME XDG_STATE_HOME XDG_DATA_HOME; shift; exec \"$@\"",
        "arany-account-fixture",
    ]).arg(home);
    command
}

#[cfg(target_os = "linux")]
pub fn remember_workspace_trust(
    account: &std::path::Path,
    workspace: &std::path::Path,
    admitted_path: &std::path::Path,
) {
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let identity = std::fs::metadata(workspace).expect("fixture directory identity");
    let root = arany::StateRoot::admit(account).expect("private fixture account root");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 2, "workspaces": [{ "path": admitted_path,
            "device": identity.dev(), "inode": identity.ino(), "mode": "auto_edits",
            "runtime_grants": [] }]
    }))
    .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.path().join("workspace-permissions.json"))
        .expect("exclusive fixture trust record");
    file.write_all(&bytes).expect("remember fixture consent");
}
