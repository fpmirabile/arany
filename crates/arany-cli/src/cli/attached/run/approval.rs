use super::{ActiveStop, InputFailure, InputRoute, RunProgress, RunUi};
use crate::cli::serve::embedded::Approval;
use crate::terminal::{Composer, TerminalInput};

#[derive(Default)]
pub(super) struct ApprovalUi {
    pub(super) pending: Option<Approval>,
    pub(super) page: usize,
    pub(super) choice: usize,
    pub(super) answer: Composer,
    pub(super) unarmed: bool,
    pub(super) review_notice: bool,
}

#[derive(Clone, Copy)]
pub(super) enum ApprovalPage {
    More,
    Last,
}

pub(super) fn linear_approval_choice(
    answer: &str,
    choices: &[crate::presentation::ToolApprovalChoice],
) -> Option<crate::presentation::ToolApprovalChoice> {
    use crate::presentation::ToolApprovalChoice::{No, Yes};
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "no" => Some(No),
        "yes" => Some(Yes),
        number => number
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| choices.get(index))
            .copied(),
    }
}

impl RunProgress<'_> {
    pub(super) fn receive_approval(&mut self, ui: &mut RunUi<'_>) {
        if self.approval_ui.pending.is_some() {
            ui.composer.disarm_continuation();
        }
        self.approval_ui.page = 0;
        self.approval_ui.choice = self
            .approval_ui
            .pending
            .as_ref()
            .filter(|approval| approval.prompt.unguarded)
            .map_or(0, |approval| approval.prompt.choices.len() - 1);
        self.approval_ui.unarmed = self.approval_ui.pending.is_some();
        self.approval_ui.review_notice = false;
        self.approval_ui.answer.clear();
    }

    pub(super) async fn handle_approval(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
        page: ApprovalPage,
    ) -> Result<(), InputFailure> {
        let choices = self
            .approval_ui
            .pending
            .as_ref()
            .expect("pending action")
            .prompt
            .choices
            .clone();
        self.approval_ui.choice = self.approval_ui.choice.min(choices.len() - 1);
        match self.approval_control(ui, input, choices.len()).await? {
            InputRoute::Handled => return Ok(()),
            InputRoute::Composer => {}
        }
        if ui.terminal.is_linear() {
            if let TerminalInput::Character(character) = input {
                self.approval_ui
                    .answer
                    .apply(TerminalInput::Character(character));
                return Ok(());
            }
            if input == TerminalInput::Backspace {
                self.approval_ui.answer.apply(TerminalInput::Backspace);
                return Ok(());
            }
        }
        if matches!(input, TerminalInput::Character(_) | TerminalInput::Submit) {
            if let Some(choice) = self.approval_reply(ui, input, page, &choices) {
                self.decide_approval(ui, choice).await?;
            }
        } else {
            self.navigate_approval(input, page, choices.len());
        }
        Ok(())
    }

    async fn approval_control(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
        choices: usize,
    ) -> Result<InputRoute, InputFailure> {
        if input == TerminalInput::CycleApprovalMode {
            let _ = self.cycle_approval(ui.composer).await?;
            return Ok(InputRoute::Handled);
        }
        if matches!(input, TerminalInput::Escape | TerminalInput::EndOfInput) {
            self.stop = ActiveStop::Cancelling;
            self.controls.cancel().await?;
            self.approval_ui.pending.take();
            ui.terminal
                .settle_child_notes(ui.view, self.controls.child_withdrawal(), true)
                .await;
            ui.terminal
                .restore_draft_input(ui.composer.input_capacity())
                .map_err(|error| InputFailure::Return(error.to_string()))?;
            return Ok(InputRoute::Handled);
        }
        if input == TerminalInput::Resize {
            self.reset_approval_review(choices);
            return Ok(InputRoute::Handled);
        }
        if input == TerminalInput::Suspend {
            ui.terminal
                .suspend_and_resume(ui.composer.input_capacity())
                .map_err(|error| InputFailure::Return(error.to_string()))?;
            self.reset_approval_focus(choices);
            return Ok(InputRoute::Handled);
        }
        Ok(InputRoute::Composer)
    }

    fn navigate_approval(&mut self, input: TerminalInput, page: ApprovalPage, choices: usize) {
        match input {
            TerminalInput::Tab if matches!(page, ApprovalPage::Last) => {
                self.approval_ui.choice = (self.approval_ui.choice + 1) % choices
            }
            TerminalInput::Up
                if matches!(page, ApprovalPage::Last)
                    && self.approval_ui.choice == 0
                    && self.approval_ui.page > 0 =>
            {
                self.approval_ui.page -= 1
            }
            TerminalInput::Up if matches!(page, ApprovalPage::Last) => {
                self.approval_ui.choice = self.approval_ui.choice.saturating_sub(1)
            }
            TerminalInput::Down if matches!(page, ApprovalPage::Last) => {
                self.approval_ui.choice = (self.approval_ui.choice + 1).min(choices - 1)
            }
            TerminalInput::Down | TerminalInput::PageDown if matches!(page, ApprovalPage::More) => {
                self.approval_ui.page += 1;
                self.approval_ui.review_notice = false;
            }
            TerminalInput::Up | TerminalInput::PageUp => {
                self.approval_ui.page = self.approval_ui.page.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn approval_reply(
        &mut self,
        ui: &RunUi<'_>,
        input: TerminalInput,
        page: ApprovalPage,
        choices: &[crate::presentation::ToolApprovalChoice],
    ) -> Option<crate::presentation::ToolApprovalChoice> {
        if input == TerminalInput::Submit {
            if matches!(page, ApprovalPage::More) {
                self.approval_ui.page += 1;
                return None;
            }
            return if ui.terminal.is_linear() {
                let choice = linear_approval_choice(self.approval_ui.answer.text(), choices);
                self.approval_ui.answer.clear();
                choice
            } else {
                Some(choices[self.approval_ui.choice])
            };
        }
        let choice = input
            .choice_index(choices.len())
            .map(|index| choices[index])?;
        if matches!(page, ApprovalPage::More)
            && choice != crate::presentation::ToolApprovalChoice::No
        {
            self.approval_ui.review_notice = true;
            None
        } else {
            Some(choice)
        }
    }

    async fn decide_approval(
        &mut self,
        ui: &mut RunUi<'_>,
        choice: crate::presentation::ToolApprovalChoice,
    ) -> Result<(), InputFailure> {
        if choice == crate::presentation::ToolApprovalChoice::AutoEdits {
            for _ in 0..2 {
                if ui.composer.approval_mode() == Some(arany::ApprovalMode::AutoEdits) {
                    break;
                }
                let _ = super::super::permissions::cycle(ui.composer);
            }
            self.controls
                .set_mode(arany::ApprovalMode::AutoEdits)
                .await?;
        }
        self.approval_ui
            .pending
            .take()
            .expect("pending action")
            .decide(&self.controls, choice.reply())
            .await?;
        ui.terminal
            .restore_draft_input(ui.composer.input_capacity())
            .map_err(|error| InputFailure::Return(error.to_string()))?;
        Ok(())
    }

    fn reset_approval_focus(&mut self, choices: usize) {
        self.approval_ui.page = 0;
        self.approval_ui.choice = if self
            .approval_ui
            .pending
            .as_ref()
            .is_some_and(|approval| approval.prompt.unguarded)
        {
            choices - 1
        } else {
            0
        };
    }

    fn reset_approval_review(&mut self, choices: usize) {
        self.reset_approval_focus(choices);
        self.approval_ui.review_notice = false;
    }

    pub(super) async fn cycle_approval(
        &self,
        composer: &mut Composer,
    ) -> Result<String, InputFailure> {
        let notice = super::super::permissions::cycle(composer);
        self.controls
            .set_mode(composer.approval_mode().unwrap_or_default())
            .await?;
        Ok(notice)
    }
}
