use super::*;
use std::path::PathBuf;

const FIXTURE_RUST_VERSION: &str = "1.98.1";

pub(super) fn configure_proxy(command: &mut std::process::Command, root: &Path, proxy: &Path) {
    command.env("ARANY_TEST_RUSTUP_PROXY", proxy).env(
        "PATH",
        std::env::join_paths([
            root.join("home/brew/bin"),
            root.join("home/.cargo/bin"),
            Path::new("/usr/bin").to_owned(),
            Path::new("/bin").to_owned(),
        ])
        .unwrap(),
    );
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native macOS user programs, linked dylib and rustup proxy"
)]
fn guarded_user_programs_load_homebrew_libraries_and_select_rustup_toolchains() {
    let proxy = PathBuf::from(std::env::var_os("HOME").expect("Rust installation home"))
        .join(".cargo/bin/rustup");
    assert!(proxy.is_file(), "CI requires its installed rustup proxy");
    super::native_journey_with_rust_sysroot(
        "tools::user_programs::native_user_programs_helper",
        None,
        None,
        Some(&proxy),
    );
}

fn compile_fixture(root: &Path, args: &[&std::ffi::OsStr]) {
    let output = std::process::Command::new("/usr/bin/xcrun")
        .args(["--sdk", "macosx", "clang"])
        .args(args)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("TMPDIR", root)
        .current_dir(root)
        .bounded_output_for(Duration::from_secs(30), 4096)
        .unwrap();
    assert!(output.status.success(), "fixture compilation: {output:?}");
}

fn homebrew_fixture(root: &Path) -> PathBuf {
    let prefix = root.join("home/brew");
    let formula = prefix.join("Cellar/probe/1.0");
    for directory in [
        prefix.join("bin"),
        prefix.join("opt"),
        formula.join("bin"),
        formula.join("lib"),
    ] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let library_source = root.join("library.c");
    let program_source = root.join("program.c");
    std::fs::write(
        &library_source,
        "extern int puts(const char *); int fixture(void) { return puts(\"homebrew-dylib\"); }\n",
    )
    .unwrap();
    std::fs::write(
        &program_source,
        "extern int fixture(void); int main(void) { return fixture() < 0; }\n",
    )
    .unwrap();
    let library = formula.join("lib/libfixture.dylib");
    compile_fixture(
        root,
        &[
            "-dynamiclib".as_ref(),
            library_source.as_os_str(),
            "-o".as_ref(),
            library.as_os_str(),
            "-Wl,-install_name,@rpath/libfixture.dylib".as_ref(),
        ],
    );
    std::os::unix::fs::symlink("../Cellar/probe/1.0", prefix.join("opt/probe")).unwrap();
    let executable = formula.join("bin/arany-brew-probe");
    let rpath = format!("-Wl,-rpath,{}", prefix.join("opt/probe/lib").display());
    compile_fixture(
        root,
        &[
            program_source.as_os_str(),
            library.as_os_str(),
            rpath.as_ref(),
            "-o".as_ref(),
            executable.as_os_str(),
        ],
    );
    std::os::unix::fs::symlink(
        "../Cellar/probe/1.0/bin/arany-brew-probe",
        prefix.join("bin/arany-brew-probe"),
    )
    .unwrap();
    prefix
}

fn rustup_fixture(root: &Path, workspace: &Path) {
    let home = root.join("home");
    let bin = home.join(".cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let proxy = std::env::var_os("ARANY_TEST_RUSTUP_PROXY").expect("parent's rustup proxy");
    std::fs::copy(proxy, bin.join("rustup")).unwrap();
    std::fs::hard_link(bin.join("rustup"), bin.join("rustc")).unwrap();
    let host = format!("{}-apple-darwin", std::env::consts::ARCH);
    let version = format!("{FIXTURE_RUST_VERSION}-{host}");
    for (name, marker) in [
        (version.as_str(), "selected-toolchain"),
        ("arany-default", "wrong-toolchain"),
    ] {
        let bin = home.join(".rustup/toolchains").join(name).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let rustc = bin.join("rustc");
        private_file(
            &rustc,
            format!("#!/bin/sh\nprintf '{marker}\\n'\n").as_bytes(),
        );
        std::fs::set_permissions(rustc, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    private_file(
        &home.join(".rustup/settings.toml"),
        format!("version = \"12\"\ndefault_toolchain = \"arany-default\"\ndefault_host_triple = \"{host}\"\nprofile = \"minimal\"\n").as_bytes(),
    );
    private_file(
        &workspace.join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel = \"{FIXTURE_RUST_VERSION}\"\n").as_bytes(),
    );
}

fn selected_permissions(workspace: &Path, prefix: &Path) -> arany::WorkspacePermissions {
    let account = StateRoot::account_path().unwrap();
    let mut permissions = arany::WorkspacePermissions::open(workspace)
        .unwrap()
        .prepare_profile_selection(&account);
    assert!(permissions.discovery_error().is_none());
    let selected = permissions
        .runtime_profile_choices()
        .into_iter()
        .filter(|option| matches!(option.group, "User programs" | "Manager shims"))
        .map(|option| {
            assert!(option.detected && !option.exclusive && !option.selected);
            assert!(
                option
                    .label
                    .contains("runs whatever the directory currently holds")
            );
            option.token
        })
        .collect::<Vec<_>>();
    assert_eq!(selected.len(), 2);
    permissions
        .remember_profiles(true, arany::ApprovalMode::Request, &selected)
        .unwrap();
    // Model a selected Homebrew prefix without modifying the host installation.
    let record = account.join("workspace-permissions.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    let metadata = std::fs::metadata(prefix).unwrap();
    let grants = value["workspaces"][0]["runtime_grants"]
        .as_array_mut()
        .unwrap();
    let brew = grants
        .iter_mut()
        .find(|grant| grant["path"] == json!(prefix.join("bin")))
        .unwrap();
    brew["root"] = json!({"path":prefix,"device":metadata.dev(),"inode":metadata.ino()});
    private_file(&record, &serde_json::to_vec(&value).unwrap());
    arany::WorkspacePermissions::open(workspace)
        .unwrap()
        .prepare_profile_selection(&account)
}

fn user_program_script(root: &Path, prefix: &Path) -> String {
    format!(
        r#"set -eu
test "$(arany-brew-probe)" = homebrew-dylib
test "$(rustc --version)" = selected-toolchain
test "$RUSTUP_HOME" = '{home}/.rustup'
test "$PATH" = '{prefix}/bin:{home}/.cargo/bin:/usr/bin:/bin'
test "$HOME" = "$TMPDIR"
test "$HOME" != '{home}'
for path in '{prefix}/lib-new' '{prefix}/Cellar/probe/1.0/lib/libfixture.dylib' '{home}/.cargo/bin/new-file' '{home}/.rustup/new-file'; do
    if (printf changed > "$path") 2>/dev/null; then exit 91; fi
done
if cat '{home}/unselected-canary' >/dev/null 2>&1; then exit 92; fi
if /usr/sbin/sysctl -n hw.pagesize >/dev/null 2>&1; then exit 93; fi
printf user-programs-ok
"#,
        home = root.join("home").display(),
        prefix = prefix.display()
    )
}

fn disk_image(root: &Path, args: &[&std::ffi::OsStr]) {
    let output = std::process::Command::new("/usr/bin/hdiutil")
        .args(args)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("TMPDIR", root)
        .current_dir(root)
        .bounded_output_for(Duration::from_secs(30), 4096)
        .unwrap();
    assert!(output.status.success(), "fixture disk image: {output:?}");
}

fn mount_image(root: &Path, mountpoint: &Path) -> MountedRuntimeImage {
    std::fs::create_dir(mountpoint).unwrap();
    let image = root.join("user-runtime.sparseimage");
    disk_image(
        root,
        &[
            "create".as_ref(),
            "-quiet".as_ref(),
            "-size".as_ref(),
            "8m".as_ref(),
            "-fs".as_ref(),
            "HFS+".as_ref(),
            "-volname".as_ref(),
            "AranyUserRuntime".as_ref(),
            "-type".as_ref(),
            "SPARSE".as_ref(),
            "-ov".as_ref(),
            image.as_os_str(),
        ],
    );
    let mounted = MountedRuntimeImage(Some(image.clone()));
    disk_image(
        root,
        &[
            "attach".as_ref(),
            "-quiet".as_ref(),
            "-nobrowse".as_ref(),
            "-mountpoint".as_ref(),
            mountpoint.as_os_str(),
            image.as_os_str(),
        ],
    );
    mounted
}

async fn refuse_mounted_roots(
    root: &Path,
    prefix: &Path,
    permissions: arany::WorkspacePermissions,
) {
    let workspace = root.join("workspace");
    for (name, selected_root) in [
        ("brew", prefix.to_owned()),
        ("rustup", root.join("home/.rustup")),
    ] {
        let mut mounted = mount_image(root, &selected_root.join("mounted-image"));
        let mut request = arany::test_support::scenario::request(
            &workspace,
            "Refuse mounts under selected user roots",
        );
        request.include_paths.clear();
        request.goal = build_goal_request();
        let call = ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "printf must-not-run".into()],
            cwd: String::new(),
        };
        let state = root.join(format!("state-mounted-{name}"));
        let (outcome, _) =
            run_offline_build_case(&state, request, permissions.clone(), call, 60_000).await;
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert_eq!(outcome.run.tools.len(), 1);
        let observation = outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert!(observation.attestation.is_none());
        assert!(observation.output.contains("runtime mount topology"));
        assert_eq!(trace_snapshot(root)["stages"]["worker_go"], "unknown");
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        assert_eq!(
            SessionView::replay(outcome.session_id, &events)
                .unwrap()
                .unwrap()
                .runs,
            vec![outcome.run]
        );
        crate::process::detach_disk_image(mounted.0.as_ref().unwrap(), Duration::from_secs(30))
            .unwrap();
        let _ = mounted.0.take();
    }
}

#[tokio::test]
#[ignore = "isolated helper owned by native macOS user program journey"]
async fn native_user_programs_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    trace_case(root, "runtime-mounts");
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let prefix = homebrew_fixture(root);
    rustup_fixture(root, &workspace);
    private_file(
        &root.join("home/unselected-canary"),
        b"UNSELECTED_HOME_CANARY",
    );
    let permissions = selected_permissions(&workspace, &prefix);
    assert_eq!(permissions.selected_runtime_profiles().len(), 2);
    let call = ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), user_program_script(root, &prefix)],
        cwd: String::new(),
    };
    let mut request =
        arany::test_support::scenario::request(&workspace, "Run selected user programs offline");
    request.include_paths.clear();
    request.goal = build_goal_request();
    let (outcome, requests) = run_offline_build_case(
        &root.join("state-users"),
        request,
        permissions.clone(),
        call.clone(),
        60_000,
    )
    .await;
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{}",
        crate::safe_run_diagnostics(&outcome.run)
    );
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_guarded_build_observation(observation);
    let output: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["stdout"], "user-programs-ok");
    assert_eq!(output["stderr"], "");
    assert_provider_journal(&requests, &call);
    assert_closed_build_replay(root, "state-users", &outcome).await;
    refuse_mounted_roots(root, &prefix, permissions).await;
}
