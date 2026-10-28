use super::ToolError;
use crate::tools::config::Program;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(in crate::tools) enum PayloadProgram<'a> {
    Configured(&'a Program),
    Prefix(&'a Program),
}

impl<'a> From<&'a Program> for PayloadProgram<'a> {
    fn from(program: &'a Program) -> Self {
        Self::Configured(program)
    }
}

impl<'a> PayloadProgram<'a> {
    pub(super) fn definition(self) -> &'a Program {
        match self {
            Self::Configured(program) | Self::Prefix(program) => program,
        }
    }

    pub(super) fn executable(self, resources: &Path) -> Result<PathBuf, ToolError> {
        let program = self.definition();
        if matches!(self, Self::Prefix(_)) || super::native_system_program(&program.executable)? {
            Ok(Path::new(&program.executable).to_owned())
        } else {
            Ok(resources.join("programs").join(&program.name))
        }
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn prefix_runtime_compat_script(
    script: &mut std::ffi::OsString,
    path: &str,
) -> Result<(), ToolError> {
    let text = script.to_str().ok_or(ToolError::Path)?;
    *script = format!(
        "export DYLD_INSERT_LIBRARIES='{}'\n{text}",
        path.replace('\'', "'\\''"),
    )
    .into();
    Ok(())
}
