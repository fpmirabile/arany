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
