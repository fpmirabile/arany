use super::{InputFailure, InputRoute, RunProgress, RunUi};
use crate::terminal::{Composer, TerminalInput};

#[derive(Default)]
pub(super) struct ApprovalUi {
    pub(super) pending: Option<arany::ToolApproval>,
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

pub(super) async fn withdraw_abandoned_approval(
    pending: &mut Option<arany::ToolApproval>,
    answer: &mut Composer,
) {
    match pending.as_mut() {
        Some(approval) => approval.abandoned().await,
        None => std::future::pending().await,
    }
    pending.take();
    answer.clear();
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
            .filter(|approval| approval.unguarded.is_some())
            .map_or(0, |approval| {
                crate::presentation::tool_approval_choices(approval).len() - 1
            });
        self.approval_ui.unarmed = self.approval_ui.pending.is_some();
        self.approval_ui.review_notice = false;
        self.approval_ui.answer.clear();
    }

    pub(super) fn handle_approval(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
        page: ApprovalPage,
    ) -> Result<(), InputFailure> {
        let choices = crate::presentation::tool_approval_choices(
            self.approval_ui.pending.as_ref().expect("pending action"),
        );
        self.approval_ui.choice = self.approval_ui.choice.min(choices.len() - 1);
        match self.approval_control(ui, input, choices.len())? {
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
                self.decide_approval(ui, choice)?;
            }
        } else {
            self.navigate_approval(input, page, choices.len());
        }
        Ok(())
    }

    pub(super) fn cycle_approval(&self, composer: &mut Composer) -> String {
        let notice = super::super::permissions::cycle(composer);
        self.approvals
            .set_mode(composer.approval_mode().unwrap_or_default());
        notice
    }
}
