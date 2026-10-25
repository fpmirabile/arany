use super::{
    ActiveStop, ApprovalPage, Feedback, RunProgress, RunUi, abandon_drafts_for_exit, active_escape,
    draft_retained_notice, exits_after_settled_run, hold_draft,
};
use crate::cli::attached::controls::ActiveSubmission;
use crate::terminal::{
    AgentChatInput, Composer, ComposerEdit, TerminalError, TerminalInput, TerminalNotice,
};
use arany::SteeringRefused;

pub(super) const STEERING_SENT: &str = "Steering note sent to the agent";

pub(super) fn steering_refusal(refused: SteeringRefused, autonomous: bool) -> String {
    match refused {
        SteeringRefused::RunFull => format!(
            "No room for more steering notes this Run. {}",
            draft_retained_notice(autonomous)
        ),
        SteeringRefused::TooLarge => "Steering note exceeds 8.0 KB; draft retained".into(),
        SteeringRefused::Empty => "Steering note is empty; draft retained".into(),
        SteeringRefused::Unavailable => "Steering note unavailable; draft retained".into(),
        SteeringRefused::ChildLimit => {
            "Only eight children accept steering notes at once; draft retained".into()
        }
    }
}

pub(super) fn settle_recorded_note(
    delivered: u64,
    seen: &mut u64,
    offered: &mut Option<String>,
    composer: &mut Composer,
) -> bool {
    if delivered == *seen {
        return false;
    }
    *seen = delivered;
    let Some(text) = offered.take() else {
        return false;
    };
    if composer.text() == text && composer.images().is_empty() {
        composer.take();
    }
    true
}

pub(super) enum InputFailure {
    Shutdown(String),
    Return(String),
}

pub(super) enum InputRoute {
    Handled,
    Composer,
}

impl RunProgress<'_> {
    pub(super) fn offer_note(
        &mut self,
        ui: &mut RunUi<'_>,
        text: String,
    ) -> Result<Option<Feedback>, InputFailure> {
        let feedback = if ui.terminal.is_linear() && self.held.is_some() {
            Feedback::Transient("Both drafts retained; wait for the Run to finish".into())
        } else if !ui.composer.images().is_empty() {
            Feedback::Transient("Steering notes are text only; draft retained".into())
        } else if self.offered.is_some() && !ui.terminal.is_linear() {
            // The composer still holds the waiting note; editing it withdraws the offer.
            Feedback::Transient(
                "Steering note waits for the agent's next step; editing withdraws it".into(),
            )
        } else {
            match self.steering.offer_note(text) {
                Ok(()) => {
                    self.offered = Some(ui.composer.text().to_owned());
                    if ui.terminal.is_linear() {
                        self.linear_offers.push(ui.composer.take_draft());
                        Feedback::Transient("Steering note waits for the agent's next step".into())
                    } else {
                        Feedback::Transient(
                            "Steering note waits for the agent's next step; editing withdraws it"
                                .into(),
                        )
                    }
                }
                Err(refused) => Feedback::Transient(steering_refusal(refused, self.autonomous)),
            }
        };
        // A linear draft kept by a refusal is invisible, so it waits aside until the Run ends.
        if ui.terminal.is_linear()
            && self.held.is_none()
            && let Some(draft) = hold_draft(ui.composer)
        {
            self.held = Some(draft);
        }
        if ui
            .terminal
            .restore_draft_input(ui.composer.input_capacity())
            .is_err()
        {
            return Err(InputFailure::Shutdown("terminal input failed".to_owned()));
        }
        Ok(Some(feedback))
    }

    pub(super) fn pause(&mut self, ui: &mut RunUi<'_>) -> Result<Option<Feedback>, InputFailure> {
        let notice = match self.stop {
            ActiveStop::Running => {
                self.steering.pause();
                self.stop = ActiveStop::Pausing;
                ui.terminal
                    .settle_child_notes(ui.view, &self.steering, true);
                "Pausing at the next step; Ctrl+C cancels now"
            }
            ActiveStop::Pausing => "Already pausing at the next step",
            ActiveStop::Cancelling => "Run is already cancelling",
        };
        // Classic hides the history notice from the status row; linear prints notices as lines.
        if self
            .draw(ui, Some(TerminalNotice::History(notice)))
            .is_err()
            || (!ui.terminal.is_linear() && self.draw(ui, None).is_err())
        {
            return Err(InputFailure::Shutdown(
                "terminal rendering failed".to_owned(),
            ));
        }
        Ok(None)
    }

    pub(super) async fn active_submission(
        &mut self,
        ui: &mut RunUi<'_>,
    ) -> Result<Option<Feedback>, InputFailure> {
        let submission = crate::cli::attached::controls::handle_active_submission(
            ui.composer,
            ui.view,
            self.starting_sequence,
        );
        Ok(match submission {
            ActiveSubmission::SteeringNote(text) => return self.offer_note(ui, text),
            ActiveSubmission::Pause => return self.pause(ui),
            ActiveSubmission::Rejected(notice) => {
                if ui.terminal.is_linear() && !ui.composer.has_text_pastes() {
                    ui.composer.take();
                }
                Some(Feedback::History(format!("Error: {notice}")))
            }
            ActiveSubmission::Notice(notice) => Some(Feedback::History(notice)),
            ActiveSubmission::ReadClipboard => Some(match ui.terminal.request_clipboard_paste() {
                Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
                Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
            }),
            ActiveSubmission::OpenAgents => {
                if ui.terminal.open_agents(ui.view, true).await.is_err() {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
                None
            }
            ActiveSubmission::OpenUsage { status } => {
                if ui
                    .terminal
                    .open_usage(ui.view, status, Some(self.starting_sequence))
                    .await
                    .is_err()
                {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
                None
            }
            ActiveSubmission::OpenHelp => {
                if ui.terminal.open_help(ui.view).await.is_err() {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
                None
            }
            ActiveSubmission::OpenAgentChats(number) => {
                match ui.terminal.open_agent_chats(ui.view, number).await {
                    Ok(AgentChatInput::Notice(text)) => Some(Feedback::Transient(text.into())),
                    Ok(
                        AgentChatInput::ChildMessage { .. }
                        | AgentChatInput::ChildEdited(_)
                        | AgentChatInput::Pass
                        | AgentChatInput::Handled
                        | AgentChatInput::Returned,
                    ) => None,
                    Err(_) => {
                        return Err(InputFailure::Shutdown(
                            "terminal rendering failed".to_owned(),
                        ));
                    }
                }
            }
            ActiveSubmission::OpenTranscript => {
                if ui.terminal.toggle_transcript(ui.view).is_err() || self.draw(ui, None).is_err() {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
                None
            }
        })
    }

    pub(super) async fn composer_input(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<Option<Feedback>, InputFailure> {
        Ok(match input {
            TerminalInput::Escape if ui.terminal.clipboard_loading() => {
                ui.terminal.cancel_clipboard_paste();
                Some(Feedback::Transient(
                    "Clipboard read cancelled; draft retained".into(),
                ))
            }
            TerminalInput::CycleApprovalMode => {
                Some(Feedback::History(self.cycle_approval(ui.composer)))
            }
            TerminalInput::ClipboardPaste => Some(match ui.terminal.request_clipboard_paste() {
                Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
                Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
            }),
            TerminalInput::Paste => match ui.terminal.paste_into_admitted_workspace(
                ui.composer,
                &self.admission.workspace,
                self.admission.workspace_identity,
            ) {
                Err(error) => Some(Feedback::History(format!(
                    "Error: {error}; draft unchanged"
                ))),
                Ok(ComposerEdit::Changed) => {
                    if self.draw(ui, None).is_err() {
                        return Err(InputFailure::Shutdown(
                            "terminal rendering failed".to_owned(),
                        ));
                    }
                    None
                }
                Ok(ComposerEdit::AtCapacity | ComposerEdit::Unchanged) => None,
            },
            input @ (TerminalInput::Character(_)
            | TerminalInput::Newline
            | TerminalInput::Backspace
            | TerminalInput::Delete
            | TerminalInput::Left
            | TerminalInput::Right
            | TerminalInput::WordLeft
            | TerminalInput::WordRight
            | TerminalInput::BackspaceWord
            | TerminalInput::Up
            | TerminalInput::Down
            | TerminalInput::Home
            | TerminalInput::End
            | TerminalInput::Tab
            | TerminalInput::Escape) => return self.edit_composer(ui, input),
            TerminalInput::LineRejected => Some(Feedback::History(
                "Error: invalid or overlong terminal line; draft unchanged".into(),
            )),
            TerminalInput::LineContinued => return self.continued_line(ui),
            TerminalInput::QuickActions => Some(Feedback::History(
                "Quick actions are unavailable during a Run; /agents opens details".into(),
            )),
            TerminalInput::Submit => return self.submit_input(ui).await,
            _ => return self.control_input(ui, input),
        })
    }

    fn continued_line(&self, ui: &mut RunUi<'_>) -> Result<Option<Feedback>, InputFailure> {
        if self.held.is_some() {
            ui.terminal
                .restore_draft_input(ui.composer.input_capacity())
                .map_err(|_| InputFailure::Shutdown("terminal input failed".to_owned()))?;
            Ok(Some(Feedback::Transient(
                "Both drafts retained; wait for the Run to finish".into(),
            )))
        } else {
            Ok(Some(Feedback::History(format!(
                "Draft: {} characters; Enter offers it as a steering note",
                ui.composer.character_count()
            ))))
        }
    }

    fn edit_composer(
        &self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<Option<Feedback>, InputFailure> {
        Ok(match ui.composer.apply(input) {
            ComposerEdit::AtCapacity => Some(Feedback::History(
                "Error: message is too long; shorten it; draft unchanged".into(),
            )),
            ComposerEdit::Changed if !ui.terminal.is_linear() => {
                if self.draw(ui, None).is_err() {
                    return Err(InputFailure::Shutdown(
                        "terminal rendering failed".to_owned(),
                    ));
                }
                None
            }
            ComposerEdit::Unchanged => None,
            ComposerEdit::Changed => None,
        })
    }

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
        if input == TerminalInput::PanelSelectionChanged {
            return self
                .draw(ui, None)
                .map_err(|_| InputFailure::Shutdown("terminal rendering failed".to_owned()));
        }
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
        let history_route = self.route_history(ui, input).await?;
        let hover_changed = ui.terminal.take_graphics_hover_changed();
        match history_route {
            InputRoute::Handled => return Ok(()),
            InputRoute::Composer if hover_changed => {
                return self
                    .draw(ui, None)
                    .map_err(|_| InputFailure::Shutdown("terminal rendering failed".to_owned()));
            }
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
                Feedback::History(STEERING_SENT.into())
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
                        Err(refused) => (steering_refusal(refused, self.autonomous), None),
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
