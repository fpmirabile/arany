use super::{ActiveStop, InputFailure, RunProgress, RunUi};
use crate::terminal::{AgentChatInput, TerminalInput, TerminalNotice};

pub(super) enum InputRoute {
    Handled,
    Composer,
}

impl RunProgress<'_> {
    pub(super) fn route_help(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<InputRoute, InputFailure> {
        if ui.terminal.help_open() {
            match input {
                TerminalInput::Interrupt => ui.terminal.close_help(),
                TerminalInput::Suspend | TerminalInput::Shutdown(_) => {}
                _ => {
                    ui.terminal.help_input(input);
                    if self.draw(ui, None).is_err() {
                        return Err(InputFailure::Shutdown(
                            "terminal rendering failed".to_owned(),
                        ));
                    }
                    return Ok(InputRoute::Handled);
                }
            }
        }
        Ok(InputRoute::Composer)
    }

    pub(super) async fn route_agents(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<InputRoute, InputFailure> {
        if ui.terminal.agents_open()
            && !matches!(
                input,
                TerminalInput::Interrupt | TerminalInput::Suspend | TerminalInput::Shutdown(_)
            )
        {
            let notice = match ui.terminal.agent_input(ui.view, input).await {
                Ok(notice) => notice.map(TerminalNotice::Transient),
                Err(_) => {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
            };
            if !ui.terminal.agents_open() && self.draw(ui, notice).is_err() {
                return Err(InputFailure::Shutdown(
                    "terminal rendering failed".to_owned(),
                ));
            }
            return Ok(InputRoute::Handled);
        }
        if ui.terminal.agents_open()
            && matches!(input, TerminalInput::Interrupt | TerminalInput::Shutdown(_))
            && ui.terminal.close_agents().is_err()
        {
            return Err(InputFailure::Shutdown(
                "terminal input cleanup failed".to_owned(),
            ));
        }
        Ok(InputRoute::Composer)
    }

    pub(super) async fn route_chat(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<InputRoute, InputFailure> {
        // A child view takes Escape before Run cancellation; Ctrl+C passes through.
        let agent_notice = match ui
            .terminal
            .agent_chat_input(ui.view, input, ui.composer)
            .await
        {
            Ok(AgentChatInput::ChildMessage { target, text }) => {
                let agent = ui
                    .view
                    .runs
                    .iter()
                    .find(|run| run.id == target.run_id)
                    .and_then(|run| {
                        run.agents
                            .iter()
                            .find(|agent| agent.id == target.agent_run_id)
                    });
                let ordinal = agent.map_or(0, |agent| agent.ordinal);
                let count = agent.map_or(0, |agent| agent.steering_notes.len());
                let (notice, offer) = if self.stop == ActiveStop::Running {
                    match self.steering.offer_child_note(target, text.clone()) {
                        Ok(()) => (
                            format!("Note waiting for child {ordinal}'s next step"),
                            Some((text, count)),
                        ),
                        Err(refused) => (super::steering_refusal(refused, self.autonomous), None),
                    }
                } else {
                    (
                        "Child note unavailable while stopping; draft retained".into(),
                        None,
                    )
                };
                ui.terminal.child_note_feedback(target, notice, offer);
                Some(None)
            }
            Ok(AgentChatInput::ChildEdited(target)) => {
                ui.terminal.withdraw_child_offer(target, &self.steering);
                Some(None)
            }
            Ok(AgentChatInput::Pass) => None,
            Ok(AgentChatInput::Handled) => return Ok(InputRoute::Handled),
            Ok(AgentChatInput::Returned) => Some(None),
            Ok(AgentChatInput::Notice(text)) => Some(Some(TerminalNotice::Transient(text))),
            Err(_) => {
                return Err(InputFailure::Shutdown(
                    "terminal rendering failed".to_owned(),
                ));
            }
        };
        if let Some(notice) = agent_notice {
            if self.draw(ui, notice).is_err() {
                return Err(InputFailure::Shutdown(
                    "terminal rendering failed".to_owned(),
                ));
            }
            return Ok(InputRoute::Handled);
        }
        Ok(InputRoute::Composer)
    }

    pub(super) async fn route_history(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<InputRoute, InputFailure> {
        if ui.terminal.handle_draft_detail(ui.composer, input) {
            if self.draw(ui, None).is_err() {
                return Err(InputFailure::Shutdown("terminal rendering failed".into()));
            }
            return Ok(InputRoute::Handled);
        }
        if ui.terminal.ignores_page_key(input) {
            return Ok(InputRoute::Handled);
        }
        match ui.terminal.handle_transcript_input(input, ui.view) {
            Ok(true) => {
                if self.draw(ui, None).is_err() {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
                return Ok(InputRoute::Handled);
            }
            Ok(false) => {}
            Err(_) => {
                return Err(InputFailure::Shutdown(
                    "terminal rendering failed".to_owned(),
                ));
            }
        }
        if ui
            .terminal
            .handle_history_input(input, ui.view, ui.composer, true)
            .await
        {
            if self.draw(ui, None).is_err() {
                return Err(InputFailure::Shutdown(
                    "terminal rendering failed".to_owned(),
                ));
            }
            return Ok(InputRoute::Handled);
        }
        Ok(InputRoute::Composer)
    }
}
