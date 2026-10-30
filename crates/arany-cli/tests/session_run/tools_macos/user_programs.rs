use super::*;
use std::path::PathBuf;

const FIXTURE_OUTPUT_CHARS: usize = 4096;
// Created only by a write the Guard failed to deny; removed whatever the outcome.
const WRITE_PROBE: &str = "arany-guard-write-probe";

// The helper sees the real user programs: the parent's HOME and PATH. Account and State stay fixtures.
pub(super) fn use_real_user_programs(command: &mut std::process::Command) {
    for name in ["HOME", "PATH"] {
        command.env(
            name,
            std::env::var_os(name).expect("real user programs environment"),
        );
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native macOS user programs; runs the real Homebrew rustup and ~/.rustup toolchains"
)]
fn guarded_user_programs_load_homebrew_libraries_and_select_rustup_toolchains() {
    super::native_journey_with_rust_sysroot(
        "tools::user_programs::native_user_programs_helper",
        None,
        None,
        true,
    );
}

// The repository's pin, which CI installs before the suite runs.
fn pinned_channel() -> &'static str {
    include_str!("../../../../../rust-toolchain.toml")
        .lines()
        .find_map(|line| line.strip_prefix("channel = \"")?.strip_suffix('"'))
        .expect("pinned Rust channel")
}

struct UserPrograms {
    home: PathBuf,
    prefix: PathBuf,
    shims: PathBuf,
}

// Selects the detected Homebrew rustup shims and the Homebrew prefix's bin, nothing else.
fn selected_permissions(workspace: &Path) -> (arany::WorkspacePermissions, UserPrograms) {
    let account = StateRoot::account_path().unwrap();
    let mut permissions = arany::WorkspacePermissions::open(workspace)
        .unwrap()
        .prepare_profile_selection(&account);
    assert!(
        permissions.discovery_error().is_none(),
        "{:?}",
        permissions.discovery_error()
    );
    let choices = permissions.runtime_profile_choices();
    let shims = choices
        .iter()
        .find(|option| {
            option.group == "Manager shims"
                && option
                    .path
                    .components()
                    .any(|part| part.as_os_str() == "Cellar")
        })
        .expect("CI installs rustup through Homebrew");
    let prefix = shims
        .path
        .ancestors()
        .find(|path| path.ends_with("Cellar"))
        .and_then(Path::parent)
        .unwrap()
        .to_owned();
    let programs = choices
        .iter()
        .find(|option| option.group == "User programs" && option.path == prefix.join("bin"))
        .expect("Homebrew bin on PATH");
    for option in [shims, programs] {
        assert!(option.detected && !option.exclusive && !option.selected);
        assert!(
            option
                .label
                .contains("runs whatever the directory currently holds")
        );
    }
    let selected = [shims.token.clone(), programs.token.clone()];
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    // Checked before the cleanup owner exists, so a real file is never removed.
    for probe in probes(&home, &prefix) {
        assert!(!probe.exists(), "{} predates the journey", probe.display());
    }
    let programs = UserPrograms {
        home,
        prefix,
        shims: shims.path.clone(),
    };
    permissions
        .remember_profiles(true, arany::ApprovalMode::Request, &selected)
        .unwrap();
    let permissions = arany::WorkspacePermissions::open(workspace)
        .unwrap()
        .prepare_profile_selection(&account);
    (permissions, programs)
}

fn probes(home: &Path, prefix: &Path) -> [PathBuf; 3] {
    [
        prefix.join(WRITE_PROBE),
        home.join(".cargo").join(WRITE_PROBE),
        home.join(".rustup").join(WRITE_PROBE),
    ]
}

impl UserPrograms {
    fn script(&self, canary: &Path) -> String {
        let [prefix_probe, cargo_probe, rustup_probe] =
            probes(&self.home, &self.prefix).map(|path| path.display().to_string());
        // The Guard exports Homebrew's fallback settings exactly when its wrappers would.
        let settings = self.prefix.join("etc/rustup/settings.toml");
        let settings = if settings.is_file() {
            format!(
                "test \"$RUSTUP_OVERRIDE_UNIX_FALLBACK_SETTINGS\" = '{0}' && cat '{0}' >/dev/null",
                settings.display()
            )
        } else {
            "test -z \"${RUSTUP_OVERRIDE_UNIX_FALLBACK_SETTINGS-}\"".to_owned()
        };
        format!(
            r#"set -eu
{settings} || exit 82
toolchain=$(rustc --version) || exit 83
case "$toolchain" in "rustc {channel} "*) ;; *) exit 84 ;; esac
test "$RUSTUP_HOME" = '{home}/.rustup' || exit 85
test "$PATH" = '{shims}:{prefix}/bin:/usr/bin:/bin' || exit 86
test "$HOME" = "$TMPDIR" || exit 87
test "$HOME" != '{home}' || exit 88
for path in '{prefix_probe}' '{cargo_probe}' '{rustup_probe}'; do
    if (printf changed > "$path") 2>/dev/null; then exit 91; fi
done
if cat '{canary}' >/dev/null 2>&1; then exit 92; fi
if /usr/sbin/sysctl -n hw.pagesize >/dev/null 2>&1; then exit 93; fi
printf user-programs-ok
"#,
            channel = pinned_channel(),
            home = self.home.display(),
            shims = self.shims.display(),
            prefix = self.prefix.display(),
            canary = canary.display(),
        )
    }
}

impl Drop for UserPrograms {
    fn drop(&mut self) {
        for probe in probes(&self.home, &self.prefix) {
            let _ = std::fs::remove_file(probe);
        }
    }
}

// The script's exit code names the failing step and its stderr carries the loader or proxy error.
fn fixture_output(run: &arany::RunView) -> String {
    run.tools
        .iter()
        .filter_map(|tool| tool.observation.as_ref())
        .flat_map(|observation| observation.output.chars())
        .take(FIXTURE_OUTPUT_CHARS)
        .collect()
}

#[tokio::test]
#[ignore = "isolated helper owned by native macOS user program journey"]
async fn native_user_programs_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    trace_case(root, "runtime-mounts");
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    private_file(
        &workspace.join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel = \"{}\"\n", pinned_channel()).as_bytes(),
    );
    let canary = root.join("home/unselected-canary");
    private_file(&canary, b"UNSELECTED_HOME_CANARY");
    let (permissions, programs) = selected_permissions(&workspace);
    assert_eq!(permissions.selected_runtime_profiles().len(), 2);
    let call = ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), programs.script(&canary)],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
    };
    let mut request =
        arany::test_support::scenario::request(&workspace, "Run selected user programs offline");
    request.include_paths.clear();
    request.goal = build_goal_request();
    let (outcome, requests) = run_offline_build_case(
        &root.join("state-users"),
        request,
        permissions,
        call.clone(),
        60_000,
    )
    .await;
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{}; output={:?}",
        crate::safe_run_diagnostics(&outcome.run),
        fixture_output(&outcome.run)
    );
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_guarded_build_observation(observation);
    let output: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["stdout"], "user-programs-ok");
    assert_eq!(output["stderr"], "");
    assert_provider_journal(&requests, &call);
    assert_closed_build_replay(root, "state-users", &outcome).await;
}
