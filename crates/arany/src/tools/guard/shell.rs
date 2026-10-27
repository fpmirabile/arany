use super::{BootstrapFailure, BootstrapStage, Config, GuardCheck, ToolCall, ToolError};
use std::path::Path;

pub(super) fn validate_command(config: &Config, call: &ToolCall) -> Result<(), ToolError> {
    let ToolCall::Command { program, args, .. } = call else {
        return Ok(());
    };
    let definition = config
        .commands
        .iter()
        .find(|definition| &definition.name == program)
        .ok_or(ToolError::Configuration)?;
    let executable = Path::new(&definition.executable);
    let shell_path = crate::tools::executable::EXECUTABLES.shell.path;
    match std::fs::canonicalize(shell_path) {
        Ok(shell) if executable == shell => script(args).map(|_| ()),
        Err(_) if executable == Path::new(shell_path) => Err(ToolError::ProtectionUnavailable),
        _ => Ok(()),
    }
}

pub(super) fn script<T: AsRef<std::ffi::OsStr>>(args: &[T]) -> Result<&str, ToolError> {
    let refused = || {
        ToolError::GuardBootstrap(BootstrapFailure {
            check: Some(GuardCheck::ShellArguments),
            ..BootstrapFailure::at(BootstrapStage::Admission)
        })
    };
    if !args.first().is_some_and(|arg| arg.as_ref() == "-c") {
        return Err(refused());
    }
    let script = args.get(1).ok_or_else(refused)?;
    let text = script.as_ref().to_str().ok_or(ToolError::Path)?;
    if text.starts_with(['-', '+']) {
        return Err(refused());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolDisposition;
    use crate::tools::guard::dispatch_result;
    use crate::validate::hex_digest;

    #[test]
    fn pinned_shell_commands_refuse_non_script_forms_before_dispatch() {
        let shell =
            std::fs::canonicalize(crate::tools::executable::EXECUTABLES.shell.path).unwrap();
        let mut config: Config =
            serde_json::from_value(crate::tools::tests::config_value()).unwrap();
        config.commands.push(crate::tools::config::Program {
            name: "renamed-shell".into(),
            executable: shell.to_str().unwrap().into(),
            sha256: hex_digest(&std::fs::read(&shell).unwrap()),
            interpreter: true,
            inputs: Vec::new(),
        });
        for args in [
            vec![],
            vec!["-lc", "printf payload"],
            vec!["-l", "-c", "printf payload"],
            vec!["-i", "-c", "printf payload"],
            vec!["-ic", "printf payload"],
            vec!["--", "-c", "printf payload"],
            vec!["script.sh", "-c"],
            vec!["-c"],
            vec!["-c", "-i", "printf payload"],
            vec!["-c", "+i", "printf payload"],
        ] {
            let call = ToolCall::Command {
                program: "renamed-shell".into(),
                args: args.iter().map(|arg| (*arg).into()).collect(),
                cwd: ".".into(),
            };
            let error = validate_command(&config, &call).unwrap_err();
            let observation =
                dispatch_result(&crate::tools::guard::tests::intent(call), false, Err(error))
                    .unwrap();
            assert_eq!(observation.disposition, ToolDisposition::Failed, "{args:?}");
            assert!(observation.attestation.is_none(), "{args:?}");
            assert!(observation.output.contains("no operation was dispatched"));
            assert!(
                observation
                    .output
                    .contains("Cause: pinned shell requires -c SCRIPT")
            );
        }
        for args in [vec!["-c", ""], vec!["-c", "printf payload", "name", "-e"]] {
            validate_command(
                &config,
                &ToolCall::Command {
                    program: "renamed-shell".into(),
                    args: args.into_iter().map(str::to_owned).collect(),
                    cwd: ".".into(),
                },
            )
            .unwrap();
        }
        validate_command(
            &config,
            &ToolCall::McpList {
                server: "peer".into(),
            },
        )
        .unwrap();
    }
}
