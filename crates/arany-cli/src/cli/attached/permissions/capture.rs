use super::*;
use arany::{Engine, EnvironmentCaptureIntent, SessionId, SessionView};

pub(in crate::cli::attached) struct EnvironmentSession<'a> {
    pub engine: &'a Engine,
    pub admission: &'a super::super::Admission,
    pub id: &'a mut SessionId,
    pub view: &'a mut SessionView,
}

impl EnvironmentSession<'_> {
    pub(super) async fn capture(
        &mut self,
        terminal: &mut AttachedTerminal,
    ) -> Result<String, SetupError> {
        super::super::materialize_session(self.admission, self.id, self.view).await?;
        let outcome = self
            .engine
            .capture_command_environment(*self.id, &self.admission.workspace, |intent| async move {
                match approve(terminal, &intent).await {
                    Ok(reply) => reply,
                    Err(SetupError::Shutdown(signal)) => {
                        terminal.defer_input(TerminalInput::Shutdown(signal));
                        arany::ApprovalReply::Unavailable
                    }
                    Err(_) => arany::ApprovalReply::Unavailable,
                }
            })
            .await;
        *self.view = super::super::load_view(&self.admission.state_dir, *self.id).await?;
        Ok(match outcome {
            Ok(stage) => stage.summary().into(),
            Err(_) => "Login-shell capture unavailable or unconfirmed; inspect /permissions before a fresh capture".into(),
        })
    }
}

async fn approve(
    terminal: &mut AttachedTerminal,
    intent: &EnvironmentCaptureIntent,
) -> Result<arany::ApprovalReply, SetupError> {
    let instruction = format!(
        "host / unguarded: {} -lc 'env -0'\ncwd {}\nShell SHA-256 {}. This runs your login files with your authority over files, network and Arany state. Capture replaces selected environment values; values are never journaled. Runtime {}, combined output {}. Detached descendants are not confined. Approve this capture once?",
        crate::presentation::escape_terminal(&intent.shell.path),
        crate::presentation::escape_terminal(&intent.home),
        intent.shell.sha256,
        crate::presentation::format_duration(EnvironmentCaptureIntent::RUNTIME_MS),
        crate::presentation::format_bytes(EnvironmentCaptureIntent::OUTPUT_BYTES as u64),
    );
    Ok(super::super::setup::review_warning(
        terminal,
        "Capture from login shell",
        &instruction,
        "Approve this host capture once?",
    )
    .await?
    .into())
}
