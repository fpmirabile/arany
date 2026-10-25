use super::{
    ActiveStop, ApprovalPage, Feedback, RunProgress, RunUi, abandon_drafts_for_exit, active_escape,
    exits_after_settled_run,
};
use crate::terminal::{AgentChatInput, ComposerEdit, TerminalError, TerminalInput, TerminalNotice};

pub(super) enum InputFailure {
    Shutdown(String),
    Return(String),
}

pub(super) enum InputRoute {
    Handled,
    Composer,
}

impl RunProgress<'_> {
    pub(super) async fn on_input(
        &mut self,
        ui: &mut RunUi<'_>,
        input: Result<TerminalInput, TerminalError>,
        page: ApprovalPage,
    ) -> Result<(), InputFailure> {
        let input = match input {
            Ok(input) => input,
            Err(_) => return Err(InputFailure::Shutdown("terminal input failed".to_owned())),
        };
        ui.composer.observe_input(input);
        if ui.terminal.agent_chat_open()
            || self.approval_ui.pending.is_some()
            || ui.terminal.help_open()
        {
            ui.composer.disarm_continuation();
        }
        if self.approval_ui.pending.is_some()
            && !matches!(input, TerminalInput::Interrupt | TerminalInput::Shutdown(_))
        {
            return self.handle_approval(ui, input, page);
        }
        match self.route_help(ui, input)? {
            InputRoute::Handled => return Ok(()),
            InputRoute::Composer => {}
        }
        match self.route_agents(ui, input).await? {
            InputRoute::Handled => return Ok(()),
            InputRoute::Composer => {}
        }
        match self.route_chat(ui, input).await? {
            InputRoute::Handled => return Ok(()),
            InputRoute::Composer => {}
        }
        match self.route_history(ui, input).await? {
            InputRoute::Handled => return Ok(()),
            InputRoute::Composer => {}
        }
        let input = ui.composer.chat_input(input);
        let input = active_escape(
            input,
            ui.composer.has_open_menu(),
            ui.terminal.clipboard_loading(),
            self.stop,
        );
        let notice = self.composer_input(ui, input).await?;
        self.finish_input(ui, input, notice)
    }

    fn finish_input(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
        mut notice: Option<Feedback>,
    ) -> Result<(), InputFailure> {
        if !ui.terminal.is_linear()
            && self
                .offered
                .as_deref()
                .is_some_and(|text| ui.composer.text() != text || !ui.composer.images().is_empty())
        {
            self.offered = None;
            notice = Some(if self.steering.withdraw_note().is_some() {
                Feedback::Transient("Steering note withdrawn; draft retained".into())
            } else {
                // The Engine already took the note, so it is sent; the edited draft stays.
                Feedback::History(super::STEERING_SENT.into())
            });
        }
        if (notice.is_some() || matches!(input, TerminalInput::Interrupt))
            && self
                .draw(ui, notice.as_ref().map(Feedback::notice))
                .is_err()
        {
            return Err(InputFailure::Shutdown(
                "terminal rendering failed".to_owned(),
            ));
        }
        Ok(())
    }

    pub(super) async fn submit_input(
        &mut self,
        ui: &mut RunUi<'_>,
    ) -> Result<Option<Feedback>, InputFailure> {
        if !ui.terminal.is_linear() && ui.composer.continue_line() {
            self.draw(ui, None)
                .map_err(|_| InputFailure::Shutdown("terminal rendering failed".into()))?;
            return Ok(None);
        }
        if ui.terminal.clipboard_loading() && !ui.terminal.is_linear() {
            ui.terminal
                .restore_draft_input(ui.composer.input_capacity())
                .map_err(|_| InputFailure::Shutdown("terminal input failed".into()))?;
            return Ok(Some(Feedback::Transient(
                "Clipboard loading; press Enter after paste to submit".into(),
            )));
        }
        if !ui.terminal.is_linear()
            && ui.composer.apply(TerminalInput::Submit) == ComposerEdit::Changed
        {
            self.draw(ui, None)
                .map_err(|_| InputFailure::Shutdown("terminal rendering failed".into()))?;
            return Ok(None);
        }
        if exits_after_settled_run(ui.composer, ui.view, self.starting_sequence) {
            abandon_drafts_for_exit(ui.composer, &mut self.held, &self.steering);
            self.offered = None;
            ui.terminal
                .restore_draft_input(crate::terminal::MAX_DRAFT_BYTES)
                .map_err(|_| InputFailure::Shutdown("terminal input failed".into()))?;
            ui.terminal.defer_input(TerminalInput::EndOfInput);
            self.exit_after_run = true;
            return Ok(None);
        }
        self.active_submission(ui).await
    }

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
