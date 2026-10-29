use super::history::{History, MessageKind, notice_kind};
use super::view::{AGENT_CHAT_ROWS, AgentChatFrame, draw_agent_chat_frame};
use super::{
    AttachedTerminal, Composer, ComposerEdit, TerminalError, TerminalInput, TerminalMode,
    TerminalNotice, dialog_area, draw_history_above,
};
use crate::presentation::{
    AgentTarget, AgentTranscript, Appended, TranscriptPart, agent_targets, run_targets,
};
use arany::{AgentRunId, ChildSteeringTarget, RunId, SessionId, SessionView};
use std::io;

pub(super) const MAX_CHOICE_BYTES: usize = 16;
const NO_CHILDREN: &str = "No child agents in this Run";
const NO_SUCH_AGENT: &str = "No such agent in this Run; /transcript agents lists them";
const MISSING_AGENT: &str = "That agent is no longer in this Run; back to primary";
const PRIMARY_SHOWN: &str = "The primary chat is already shown";
const BACK_TO_PRIMARY: &str = "Back to primary";
const PRIMARY_LINE: &str = "Viewing primary; its conversation is above";
const INVALID_AGENT_CHAT_CHOICE: &str = "Invalid agent choice; type a number, n, p or q";

pub enum AgentChatInput {
    // The input keeps its ordinary meaning.
    Pass,
    ChildMessage {
        target: ChildSteeringTarget,
        text: String,
    },
    ChildEdited(ChildSteeringTarget),
    Handled,
    // The primary chat owns input again; the caller redraws it.
    Returned,
    Notice(&'static str),
}

#[derive(Debug, Eq, PartialEq)]
pub enum AgentChatFollow {
    // The shown chat is as it was: committed Events changed no displayed fact.
    Unchanged,
    Redraw,
    // The selected child no longer resolves; the primary is shown with this notice.
    Closed(&'static str),
}

pub(super) struct ChildDraft {
    target: ChildSteeringTarget,
    composer: Composer,
    offered: Option<ChildOffer>,
    notice: Option<String>,
}

struct ChildOffer {
    text: String,
    seen: usize,
    state: ChildOfferState,
}

enum ChildOfferState {
    Waiting,
    Unknown,
}

const MAX_CHILD_DRAFTS: usize = 8;
const DRAFT_CAP_NOTICE: &str =
    "Eight child drafts are retained; clear one before opening another message";

impl ChildDraft {
    // An unknown outcome of a child that can no longer take notes would otherwise hold its slot forever.
    fn clear(&mut self, editable: bool) {
        self.composer.clear();
        if !editable
            && self
                .offered
                .as_ref()
                .is_some_and(|offer| matches!(offer.state, ChildOfferState::Unknown))
        {
            self.offered = None;
        }
    }

    fn acknowledge(&mut self, view: &SessionView) -> bool {
        let Some(offer) = &self.offered else {
            return false;
        };
        let agent = (view.id == self.target.session_id)
            .then(|| {
                view.runs
                    .iter()
                    .find(|run| run.id == self.target.run_id)
                    .and_then(|run| {
                        run.agents
                            .iter()
                            .find(|agent| agent.id == self.target.agent_run_id)
                    })
            })
            .flatten();
        let Some(agent) = agent.filter(|agent| {
            agent
                .steering_notes
                .get(offer.seen..)
                .is_some_and(|notes| notes.contains(&offer.text))
        }) else {
            return false;
        };
        if self.composer.text() == offer.text {
            self.composer.clear();
        }
        self.notice = Some(format!(
            "Steering note recorded for child {}",
            agent.ordinal
        ));
        self.offered = None;
        true
    }
}

fn editable_child(view: &SessionView, target: ChildSteeringTarget) -> bool {
    view.id == target.session_id
        && view.runs.last().is_some_and(|run| {
            run.id == target.run_id
                && run.status == arany::RunStatus::Active
                && run.workers.stop.is_none()
                && run.agents.iter().any(|agent| {
                    agent.id == target.agent_run_id
                        && agent.role == arany::AgentRole::Child
                        && agent.status == arany::AgentStatus::Active
                })
                && view.goals.iter().any(|goal| {
                    goal.status == arany::GoalStatus::Running && goal.active_run == Some(run.id)
                })
        })
}

// A transcript fed by committed Events; the primary keeps its History, draft and anchors.
pub(super) struct AgentChat {
    session: SessionId,
    run: RunId,
    // Numbered choices bind to this snapshot of the Run's agents.
    targets: Vec<AgentTarget>,
    selected: usize,
    transcript: Option<AgentTranscript>,
    // What Classic scrollback or linear output already holds, and which notices Fullscreen recorded.
    appended: Appended,
    history: History,
    listing: bool,
    shown: bool,
    choice: String,
    invalid: bool,
    editable: bool,
    last_child_notice: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Step {
    Pass,
    Ignore,
    NoChildren,
    Select(usize),
}

// Target 0 is the primary; `admitted` means an empty composer with no popup or dialog.
pub(super) fn agent_key(
    input: TerminalInput,
    viewing: Option<usize>,
    count: usize,
    admitted: bool,
) -> Step {
    let forward = match input {
        TerminalInput::AgentNext => true,
        TerminalInput::AgentPrevious => false,
        TerminalInput::Escape if viewing.is_some() => return Step::Select(0),
        _ => return Step::Pass,
    };
    if viewing.is_none() && !admitted {
        return Step::Ignore;
    }
    if count < 2 {
        return Step::NoChildren;
    }
    let current = viewing.unwrap_or(0).min(count - 1);
    Step::Select(if forward {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    })
}

impl AgentChat {
    fn target(&self) -> Option<ChildSteeringTarget> {
        (self.selected != 0).then(|| ChildSteeringTarget {
            session_id: self.session,
            run_id: self.run,
            agent_run_id: self.targets[self.selected].id,
        })
    }

    fn label(&self) -> &str {
        self.transcript
            .as_ref()
            .map_or("primary", |transcript| transcript.label.as_str())
    }

    // Rebuilds the selected child from `view` by identity; `None` when it no longer resolves.
    fn refresh(&mut self, view: &SessionView) -> Option<bool> {
        let Some(shown) = &self.transcript else {
            return Some(false);
        };
        if shown.view.last_sequence == view.last_sequence {
            return Some(false);
        }
        let id = self.targets.get(self.selected)?.id;
        let fresh = AgentTranscript::from_session(view, self.run, id)?;
        let editable = self
            .target()
            .is_some_and(|target| editable_child(view, target));
        let changed = !fresh.same_display(shown) || self.editable != editable;
        self.transcript = Some(fresh);
        Some(changed)
    }

    fn unappended(&mut self) -> Vec<TranscriptPart<'_>> {
        self.transcript
            .as_ref()
            .map_or_else(Vec::new, |transcript| {
                transcript.unappended(&mut self.appended)
            })
    }

    // The whole view on its first print, then only a child's new records; a listing prints once.
    fn linear_draw_lines(&mut self) -> Vec<String> {
        if !self.shown {
            return self.linear_lines();
        }
        self.transcript
            .as_ref()
            .map_or_else(Vec::new, |transcript| {
                transcript.linear_update(&mut self.appended)
            })
    }

    // Lines for the first linear print of this view.
    fn linear_lines(&mut self) -> Vec<String> {
        if let Some(transcript) = &self.transcript {
            let mut lines = transcript.linear_lines(&mut self.appended);
            if self.editable {
                lines[0] = format!("Viewing {} (message)", transcript.label);
            }
            return lines;
        }
        if !self.listing {
            return vec![PRIMARY_LINE.into()];
        }
        let mut lines = vec!["Agent chats in this Run:".to_owned()];
        lines.extend(
            self.targets
                .iter()
                .enumerate()
                .map(|(index, target)| format!("{}. {}", index + 1, target.line)),
        );
        lines
    }
}

impl AttachedTerminal {
    #[must_use]
    pub fn agent_chat_open(&self) -> bool {
        self.agent_chat.is_some()
    }

    // Siblings, the primary and an unchanged reload leave a shown child view untouched.
    pub async fn follow_agent_chat(
        &mut self,
        view: &SessionView,
    ) -> Result<AgentChatFollow, TerminalError> {
        let Some(chat) = self
            .agent_chat
            .as_mut()
            .filter(|chat| chat.session == view.id)
        else {
            return Ok(AgentChatFollow::Redraw);
        };
        let was_editable = chat.editable;
        let refreshed = chat.refresh(view);
        chat.editable = chat
            .target()
            .is_some_and(|target| editable_child(view, target));
        if refreshed.is_some() && was_editable != chat.editable {
            if let Some(draft) = self.selected_child_draft_mut() {
                draft.composer.disarm_continuation();
            }
            self.cancel_clipboard_paste();
            self.admit_agent_chat(view).await?;
            return Ok(AgentChatFollow::Redraw);
        }
        match refreshed {
            Some(true) => Ok(AgentChatFollow::Redraw),
            Some(false) => Ok(AgentChatFollow::Unchanged),
            None => {
                self.leave_agent_chat().await?;
                Ok(AgentChatFollow::Closed(MISSING_AGENT))
            }
        }
    }

    // Alt+Left/Right from an empty composer, and every input while a child transcript is shown.
    pub async fn agent_chat_input(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
        composer: &Composer,
    ) -> Result<AgentChatInput, TerminalError> {
        self.primary_input_capacity = composer.input_capacity();
        if self.agent_chat.is_none() {
            if !matches!(
                input,
                TerminalInput::AgentNext | TerminalInput::AgentPrevious
            ) {
                return Ok(AgentChatInput::Pass);
            }
            let admitted = composer.is_empty()
                && !composer.has_open_menu()
                && self.clipboard.is_none()
                && !self.transcript.transcript_open
                && !self.transcript.scrollback_dump;
            return self.navigate(view, input, admitted).await;
        }
        if let Some(draft) = self.selected_child_draft_mut() {
            draft.composer.observe_input(input);
        }
        if self.linear.is_some() {
            return self.linear_agent_input(view, input).await;
        }
        let fullscreen = self.mode == TerminalMode::Fullscreen;
        match input {
            TerminalInput::Interrupt | TerminalInput::Shutdown(_) => Ok(AgentChatInput::Pass),
            TerminalInput::EndOfInput => {
                self.end_agent_chat()?;
                Ok(AgentChatInput::Pass)
            }
            TerminalInput::Suspend => {
                self.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                self.rearm_agent_chat(view).await
            }
            TerminalInput::Resize => self.rearm_agent_chat(view).await,
            TerminalInput::Paste
            | TerminalInput::ClipboardPaste
            | TerminalInput::Submit
            | TerminalInput::ModifiedEnter(_) => self.edit_child(view, input).await,
            TerminalInput::AgentNext | TerminalInput::AgentPrevious | TerminalInput::Escape => {
                let empty = self
                    .selected_child_draft()
                    .is_none_or(|draft| draft.composer.is_empty());
                if input != TerminalInput::Escape && (!empty || self.clipboard.is_some()) {
                    return Ok(AgentChatInput::Handled);
                }
                self.navigate(view, input, true).await
            }
            TerminalInput::PageUp
            | TerminalInput::PageDown
            | TerminalInput::Home
            | TerminalInput::End
            | TerminalInput::PointerScrollUp { .. }
            | TerminalInput::PointerScrollDown { .. }
                if fullscreen =>
            {
                let chat = self.agent_chat.as_mut().expect("open agent chat");
                if let Some(transcript) = &chat.transcript {
                    match input {
                        TerminalInput::Home => chat.history.scroll(true, usize::MAX),
                        TerminalInput::End => chat.history.scroll(false, usize::MAX),
                        _ => chat.history.handle(input, &transcript.view),
                    };
                }
                self.draw_agent_chat(view, None)?;
                Ok(AgentChatInput::Handled)
            }
            TerminalInput::Character(_)
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
            | TerminalInput::End => self.edit_child(view, input).await,
            TerminalInput::PageUp
            | TerminalInput::PageDown
            | TerminalInput::HalfPageUp
            | TerminalInput::IgnoredKey
            | TerminalInput::Tab
            | TerminalInput::CycleApprovalMode
            | TerminalInput::QuickActions
            | TerminalInput::HistoryFind
            | TerminalInput::HistoryLive
            | TerminalInput::Refresh
            | TerminalInput::Transcript
            | TerminalInput::CopySelection
            | TerminalInput::PanelSelectionChanged
            | TerminalInput::PointerMove { .. }
            | TerminalInput::PointerClick { .. }
            | TerminalInput::PointerDrag { .. }
            | TerminalInput::PointerRelease { .. }
            | TerminalInput::PointerScrollUp { .. }
            | TerminalInput::PointerScrollDown { .. }
            | TerminalInput::LineRejected
            | TerminalInput::LineContinued => Ok(AgentChatInput::Handled),
        }
    }

    // `/transcript agents [N]`: N selects from the current or last Run's numbered agents.
    pub async fn open_agent_chats(
        &mut self,
        view: &SessionView,
        number: Option<usize>,
    ) -> Result<AgentChatInput, TerminalError> {
        let Some((run, targets)) = agent_targets(view).filter(|(_, targets)| targets.len() > 1)
        else {
            return Ok(AgentChatInput::Notice(NO_CHILDREN));
        };
        let linear = self.linear.is_some();
        match number {
            Some(number) if number > targets.len() => Ok(AgentChatInput::Notice(NO_SUCH_AGENT)),
            Some(1) if !linear => Ok(AgentChatInput::Notice(PRIMARY_SHOWN)),
            Some(number) => self.show_agent(view, run, targets, number - 1, false).await,
            None if linear => self.show_agent(view, run, targets, 0, true).await,
            None => self.show_agent(view, run, targets, 1, false).await,
        }
    }

    // `/agents` Enter: a child opens its chat by identity; a primary row shows the primary chat.
    pub(super) async fn open_agent(
        &mut self,
        view: &SessionView,
        run: RunId,
        agent: AgentRunId,
    ) -> Result<Option<&'static str>, TerminalError> {
        let targets = view
            .runs
            .iter()
            .find(|candidate| candidate.id == run)
            .map(run_targets)
            .unwrap_or_default();
        match targets.iter().position(|target| target.id == agent) {
            Some(0) => self.leave_agent_chat().await.map(|_| None),
            Some(index) => match self.show_agent(view, run, targets, index, false).await? {
                AgentChatInput::Notice(text) => Ok(Some(text)),
                _ => Ok(None),
            },
            None => Ok(Some(MISSING_AGENT)),
        }
    }

    async fn navigate(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
        admitted: bool,
    ) -> Result<AgentChatInput, TerminalError> {
        let fresh = agent_targets(view);
        let count = fresh.as_ref().map_or(0, |(_, targets)| targets.len());
        // A newer Run restarts the ring at its primary.
        let viewing = self.agent_chat.as_ref().map(|chat| {
            fresh
                .as_ref()
                .filter(|(run, _)| *run == chat.run)
                .and_then(|(_, targets)| {
                    let shown = chat.targets.get(chat.selected)?.id;
                    targets.iter().position(|target| target.id == shown)
                })
                .unwrap_or(0)
        });
        match agent_key(input, viewing, count, admitted) {
            Step::Pass => Ok(AgentChatInput::Pass),
            Step::Ignore => Ok(AgentChatInput::Handled),
            Step::NoChildren => Ok(AgentChatInput::Notice(NO_CHILDREN)),
            Step::Select(0) => self.leave_agent_chat().await,
            Step::Select(index) => {
                let (run, targets) = fresh.expect("counted agent targets");
                self.show_agent(view, run, targets, index, false).await
            }
        }
    }

    async fn show_agent(
        &mut self,
        view: &SessionView,
        run: RunId,
        targets: Vec<AgentTarget>,
        selected: usize,
        listing: bool,
    ) -> Result<AgentChatInput, TerminalError> {
        let Some(target) = targets.get(selected) else {
            return Ok(AgentChatInput::Notice(NO_SUCH_AGENT));
        };
        let transcript = if selected == 0 {
            None
        } else if let Some(transcript) = AgentTranscript::from_session(view, run, target.id) {
            Some(transcript)
        } else {
            self.end_agent_chat()?;
            self.begin_dialog();
            self.arm_dialog().await?;
            return Ok(AgentChatInput::Notice(MISSING_AGENT));
        };
        let target = ChildSteeringTarget {
            session_id: view.id,
            run_id: run,
            agent_run_id: target.id,
        };
        let editable = selected != 0 && editable_child(view, target);
        for draft in &mut self.child_drafts {
            draft.composer.disarm_continuation();
        }
        if editable && !self.open_child_draft(view, target) {
            return Ok(AgentChatInput::Notice(DRAFT_CAP_NOTICE));
        }
        self.cancel_clipboard_paste();
        self.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected,
            transcript,
            appended: Appended::default(),
            history: History::agent(),
            listing,
            shown: false,
            choice: String::new(),
            invalid: false,
            editable,
            last_child_notice: None,
        });
        self.rearm_agent_chat(view).await
    }

    // Discards input read before this view existed.
    async fn admit_agent_chat(&mut self, view: &SessionView) -> Result<(), TerminalError> {
        self.begin_dialog();
        self.draw_agent_chat(view, None)?;
        self.arm_dialog().await?;
        self.restore_selected_draft_input(self.primary_input_capacity)
    }

    async fn rearm_agent_chat(
        &mut self,
        view: &SessionView,
    ) -> Result<AgentChatInput, TerminalError> {
        self.admit_agent_chat(view).await?;
        self.draw_agent_chat(view, None)?;
        Ok(AgentChatInput::Handled)
    }

    async fn leave_agent_chat(&mut self) -> Result<AgentChatInput, TerminalError> {
        self.end_agent_chat()?;
        // Keys meant for the child view must not reach the primary draft.
        self.begin_dialog();
        self.arm_dialog().await?;
        self.restore_draft_input(self.primary_input_capacity)?;
        Ok(AgentChatInput::Returned)
    }

    // Closes the view without input admission, as when Tool approval preempts it.
    pub(super) fn end_agent_chat(&mut self) -> Result<(), TerminalError> {
        if let Some(draft) = self.selected_child_draft_mut() {
            draft.composer.disarm_continuation();
        }
        self.cancel_clipboard_paste();
        let Some(chat) = self.agent_chat.take() else {
            return Ok(());
        };
        if let Some(linear) = &mut self.linear {
            linear.write_agent_chat(&mut io::stderr(), &[BACK_TO_PRIMARY.into()], None, false)?;
            linear.prompt_needed = true;
        } else if self.mode == TerminalMode::Classic && chat.shown {
            // Settled native rows stay; the boundary marks where the primary resumes.
            self.prepare_frame()?;
            self.insert_native(MessageKind::Notice, BACK_TO_PRIMARY, None)?;
        }
        Ok(())
    }

    fn selected_child_draft(&self) -> Option<&ChildDraft> {
        let target = self.agent_chat.as_ref()?.target()?;
        self.child_drafts
            .iter()
            .find(|draft| draft.target == target)
    }

    fn selected_child_draft_mut(&mut self) -> Option<&mut ChildDraft> {
        let target = self.agent_chat.as_ref()?.target()?;
        self.child_drafts
            .iter_mut()
            .find(|draft| draft.target == target)
    }

    pub fn child_note_feedback(
        &mut self,
        target: ChildSteeringTarget,
        notice: String,
        offered: Option<(String, usize)>,
    ) {
        if let Some(draft) = self
            .child_drafts
            .iter_mut()
            .find(|draft| draft.target == target)
        {
            draft.notice = Some(notice);
            if let Some(offered) = offered {
                draft.offered = Some(ChildOffer {
                    text: offered.0,
                    seen: offered.1,
                    state: ChildOfferState::Waiting,
                });
            }
            // A new feedback event is announced even when its text repeats.
            if let Some(chat) = self.agent_chat.as_mut()
                && chat.target() == Some(target)
            {
                chat.last_child_notice = None;
            }
        }
    }

    pub async fn withdraw_child_offer(
        &mut self,
        target: ChildSteeringTarget,
        mut withdraw: impl AsyncFnMut(ChildSteeringTarget) -> Option<String>,
    ) {
        if let Some(draft) = self
            .child_drafts
            .iter_mut()
            .find(|draft| draft.target == target)
            && draft.offered.is_some()
        {
            let withdrawn = withdraw(target).await.is_some();
            if withdrawn || draft.composer.is_empty() {
                draft.offered = None;
            } else if let Some(offer) = &mut draft.offered {
                offer.state = ChildOfferState::Unknown;
            }
            draft.notice = Some(
                if withdrawn {
                    "Steering note withdrawn; child draft retained"
                } else {
                    "Steering note outcome unknown; check child history; draft retained"
                }
                .into(),
            );
            if let Some(chat) = self.agent_chat.as_mut()
                && chat.target() == Some(target)
            {
                chat.last_child_notice = None;
            }
        }
    }

    pub fn restore_selected_draft_input(
        &mut self,
        primary_capacity: usize,
    ) -> Result<(), TerminalError> {
        let capacity = if let Some(chat) = &self.agent_chat {
            if self.linear.is_some() {
                super::MAX_DRAFT_BYTES.saturating_sub(chat.choice.len())
            } else {
                self.selected_child_draft()
                    .map_or(super::MAX_DRAFT_BYTES, |draft| {
                        draft.composer.input_capacity()
                    })
            }
        } else {
            primary_capacity
        };
        self.restore_draft_input(capacity)
    }

    pub fn child_notes_pending(&self) -> bool {
        self.child_drafts
            .iter()
            .any(|draft| draft.offered.is_some())
    }

    pub async fn settle_child_notes(
        &mut self,
        view: &SessionView,
        mut withdraw: impl AsyncFnMut(ChildSteeringTarget) -> Option<String>,
        stopped: bool,
    ) -> bool {
        let mut changed = false;
        for draft in &mut self.child_drafts {
            if draft.acknowledge(view) {
                changed = true;
                continue;
            }
            if draft.offered.is_some() && (stopped || !editable_child(view, draft.target)) {
                let withdrawn = withdraw(draft.target).await.is_some();
                if withdrawn {
                    draft.offered = None;
                } else if let Some(offer) = &mut draft.offered {
                    offer.state = ChildOfferState::Unknown;
                }
                let notice = if withdrawn {
                    "Note not delivered; child draft retained"
                } else {
                    "Steering note outcome unknown; check child history; draft retained"
                };
                changed |= draft.notice.as_deref() != Some(notice);
                draft.notice = Some(notice.into());
            }
        }
        changed
    }

    fn open_child_draft(&mut self, view: &SessionView, target: ChildSteeringTarget) -> bool {
        if self.child_drafts.iter().any(|draft| draft.target == target) {
            return true;
        }
        // Drafts of Runs the picker can no longer open are unreachable, so only offers outlive them.
        self.child_drafts.retain(|draft| {
            draft.offered.is_some()
                || (!draft.composer.is_empty()
                    && view
                        .runs
                        .iter()
                        .rev()
                        .take(crate::presentation::MAX_RECENT_RUNS)
                        .any(|run| run.id == draft.target.run_id))
        });
        if self.child_drafts.len() >= MAX_CHILD_DRAFTS {
            return false;
        }
        self.child_drafts.push(ChildDraft {
            target,
            composer: Composer::text_only(),
            offered: None,
            notice: None,
        });
        true
    }

    async fn edit_child(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
    ) -> Result<AgentChatInput, TerminalError> {
        let Some(target) = self.agent_chat.as_ref().and_then(AgentChat::target) else {
            return Ok(AgentChatInput::Handled);
        };
        let editable = editable_child(view, target);
        if !editable {
            if matches!(input, TerminalInput::Backspace | TerminalInput::Delete)
                && let Some(draft) = self.selected_child_draft_mut()
            {
                draft.clear(false);
                return Ok(AgentChatInput::ChildEdited(target));
            }
            if input == TerminalInput::Paste {
                let _ = self.take_paste();
            }
            return Ok(AgentChatInput::Notice(
                "This child is read-only; draft retained",
            ));
        }
        if !self.open_child_draft(view, target) {
            return Ok(AgentChatInput::Notice(DRAFT_CAP_NOTICE));
        }
        let shortcut = self.newline_shortcut;
        let input = self.selected_child_draft_mut().map_or(input, |draft| {
            draft.composer.set_newline_shortcut(shortcut);
            draft.composer.chat_input(input)
        });
        if input == TerminalInput::ClipboardPaste {
            return Ok(match self.request_clipboard_paste() {
                Ok(()) => AgentChatInput::Handled,
                Err(text) => AgentChatInput::Notice(text),
            });
        }
        if input == TerminalInput::Submit
            && self
                .selected_child_draft_mut()
                .is_some_and(|draft| draft.composer.apply(input) == ComposerEdit::Changed)
        {
            let bytes = self
                .selected_child_draft()
                .map_or(0, |draft| draft.composer.text().len());
            self.restore_draft_input(bytes)?;
            self.draw_agent_chat(view, None)?;
            return Ok(AgentChatInput::ChildEdited(target));
        }
        if input == TerminalInput::Submit {
            let Some(draft) = self.selected_child_draft() else {
                return Ok(AgentChatInput::Handled);
            };
            return Ok(if let Some(offer) = &draft.offered {
                AgentChatInput::Notice(match offer.state {
                    ChildOfferState::Waiting => "Note waiting for this child's next step",
                    ChildOfferState::Unknown => {
                        "Steering note outcome unknown; check child history; draft retained"
                    }
                })
            } else if draft.composer.text().trim().is_empty() {
                AgentChatInput::Notice("Child message is empty; message not offered")
            } else {
                AgentChatInput::ChildMessage {
                    target,
                    text: draft.composer.text().to_owned(),
                }
            });
        }
        let paste = if input == TerminalInput::Paste {
            match self.take_paste() {
                Ok(text) => Some(text),
                Err(text) => return Ok(AgentChatInput::Notice(text)),
            }
        } else {
            None
        };
        let Some(draft) = self.selected_child_draft_mut() else {
            return Ok(AgentChatInput::Handled);
        };
        let before = draft.composer.text().to_owned();
        let edit = match paste {
            Some(text) => match draft.composer.insert_paste(&text) {
                Ok(edit) => edit,
                Err(text) => return Ok(AgentChatInput::Notice(text)),
            },
            None => draft.composer.apply(input),
        };
        let capacity = draft.composer.input_capacity();
        let edited = before != draft.composer.text();
        if edited && draft.offered.is_none() {
            draft.notice = None;
        }
        self.restore_draft_input(capacity)?;
        self.draw_agent_chat(view, None)?;
        Ok(if edit == ComposerEdit::AtCapacity {
            AgentChatInput::Notice("Child draft exceeds 8.0 KB; text retained")
        } else if edited {
            AgentChatInput::ChildEdited(target)
        } else {
            AgentChatInput::Handled
        })
    }

    async fn linear_agent_input(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
    ) -> Result<AgentChatInput, TerminalError> {
        let chat = self.agent_chat.as_mut().expect("open agent chat");
        match input {
            TerminalInput::Character(character) => {
                let message = chat.choice.starts_with("/message ");
                if !character.is_control()
                    && (message || character.is_ascii())
                    && chat.choice.len() + character.len_utf8()
                        <= if message {
                            super::composer::MAX_DRAFT_BYTES + 9
                        } else {
                            MAX_CHOICE_BYTES
                        }
                {
                    chat.choice.push(character);
                } else {
                    chat.invalid = true;
                }
                Ok(AgentChatInput::Handled)
            }
            TerminalInput::Newline if chat.choice.starts_with("/message ") => {
                if chat.choice.len() < super::composer::MAX_DRAFT_BYTES + 9 {
                    chat.choice.push('\n');
                } else {
                    chat.invalid = true;
                }
                Ok(AgentChatInput::Handled)
            }
            TerminalInput::Backspace => {
                chat.choice.pop();
                Ok(AgentChatInput::Handled)
            }
            TerminalInput::Submit => {
                let choice = std::mem::take(&mut chat.choice);
                let invalid = std::mem::take(&mut chat.invalid);
                if let Some(text) = choice.strip_prefix("/message ") {
                    if invalid
                        || text.trim().is_empty()
                        || text.len() > super::composer::MAX_DRAFT_BYTES
                    {
                        return Ok(AgentChatInput::Notice(
                            "Child message is empty or exceeds 8.0 KB; message not offered",
                        ));
                    }
                    let target = chat.target();
                    let Some(target) = target.filter(|target| editable_child(view, *target)) else {
                        return Ok(AgentChatInput::Notice(
                            "This child is read-only; message not offered",
                        ));
                    };
                    if !self.open_child_draft(view, target) {
                        return Ok(AgentChatInput::Notice(DRAFT_CAP_NOTICE));
                    }
                    let Some(draft) = self
                        .child_drafts
                        .iter_mut()
                        .find(|draft| draft.target == target)
                    else {
                        return Ok(AgentChatInput::Handled);
                    };
                    if draft.offered.is_some()
                        || (!draft.composer.is_empty() && draft.composer.text() != text)
                    {
                        return Ok(AgentChatInput::Notice(
                            "Message not offered; /clear clears the child draft before a different message",
                        ));
                    }
                    if draft.composer.is_empty() {
                        draft.composer.insert_paste(text).expect("validated line");
                    }
                    return Ok(AgentChatInput::ChildMessage {
                        target,
                        text: text.to_owned(),
                    });
                }
                if choice == "/clear" && !invalid {
                    let editable = self
                        .agent_chat
                        .as_ref()
                        .and_then(AgentChat::target)
                        .is_some_and(|target| editable_child(view, target));
                    if let Some(draft) = self.selected_child_draft_mut() {
                        draft.clear(editable);
                        return Ok(AgentChatInput::ChildEdited(draft.target));
                    }
                    return Ok(AgentChatInput::Handled);
                }
                let chat = self.agent_chat.as_mut().expect("open agent chat");
                let count = chat.targets.len();
                let selected = chat.selected;
                let next = if invalid {
                    None
                } else {
                    match choice.as_str() {
                        "q" => return self.leave_agent_chat().await,
                        "n" => Some((selected + 1) % count),
                        "p" => Some((selected + count - 1) % count),
                        number => number
                            .parse::<usize>()
                            .ok()
                            .filter(|number| (1..=count).contains(number))
                            .map(|number| number - 1),
                    }
                };
                let Some(index) = next else {
                    return self.invalid_agent_choice();
                };
                let run = chat.run;
                let targets = chat.targets.clone();
                self.show_agent(view, run, targets, index, false).await
            }
            TerminalInput::LineContinued if chat.choice.starts_with("/message ") => {
                Ok(AgentChatInput::Handled)
            }
            TerminalInput::LineRejected if chat.choice.starts_with("/message ") => Ok(
                AgentChatInput::Notice("Invalid or overlong child message segment; draft retained"),
            ),
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                chat.choice.clear();
                chat.invalid = false;
                self.discard_draft_input();
                self.invalid_agent_choice()
            }
            TerminalInput::Interrupt | TerminalInput::Shutdown(_) => Ok(AgentChatInput::Pass),
            TerminalInput::EndOfInput => {
                self.end_agent_chat()?;
                Ok(AgentChatInput::Pass)
            }
            TerminalInput::Suspend => {
                chat.shown = false;
                self.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                self.rearm_agent_chat(view).await
            }
            TerminalInput::Paste => {
                let _ = self.take_paste();
                Ok(AgentChatInput::Handled)
            }
            _ => Ok(AgentChatInput::Handled),
        }
    }

    fn invalid_agent_choice(&mut self) -> Result<AgentChatInput, TerminalError> {
        self.linear
            .as_mut()
            .expect("linear presentation")
            .write_agent_chat(
                &mut io::stderr(),
                &[],
                Some(INVALID_AGENT_CHAT_CHOICE),
                true,
            )?;
        Ok(AgentChatInput::Handled)
    }

    // A view from another Session is dropped; the caller then draws the primary chat.
    pub(super) fn agent_chat_shown(&mut self, view: &SessionView) -> bool {
        if self
            .agent_chat
            .as_ref()
            .is_some_and(|chat| chat.session != view.id)
        {
            self.agent_chat = None;
        }
        self.agent_chat.is_some()
    }

    pub(super) fn draw_agent_chat(
        &mut self,
        view: &SessionView,
        notice: Option<TerminalNotice<'_>>,
    ) -> Result<(), TerminalError> {
        self.clear_graphics_hover();
        self.delete_graphics_placement()?;
        let mut acknowledged = false;
        for draft in &mut self.child_drafts {
            acknowledged |= draft.acknowledge(view);
        }
        if acknowledged {
            self.restore_selected_draft_input(self.primary_input_capacity)?;
        }
        let (history_notice, transient) = match notice {
            Some(TerminalNotice::History(text)) => (Some(text), None),
            Some(TerminalNotice::Transient(text)) => (None, Some(text)),
            None => (None, None),
        };
        // A missing child keeps its last validated transcript; following reports it.
        self.agent_chat
            .as_mut()
            .expect("open agent chat")
            .refresh(view);
        let child_notice = self
            .selected_child_draft()
            .and_then(|draft| draft.notice.clone());
        let retained = self
            .selected_child_draft()
            .is_some_and(|draft| !draft.composer.is_empty());
        if let Some(linear) = &mut self.linear {
            let chat = self.agent_chat.as_mut().expect("open agent chat");
            let mut lines = chat.linear_draw_lines();
            if !chat.shown && chat.editable {
                lines.push(format!(
                    "Message {} with /message TEXT; /clear clears its retained draft",
                    chat.label()
                ));
            }
            if !chat.shown && retained {
                lines.push("Child draft retained; /clear clears it".into());
            }
            let fresh_notice = child_notice
                .as_ref()
                .filter(|notice| chat.last_child_notice.as_ref() != Some(notice));
            if let Some(notice) = fresh_notice {
                lines.push(format!("Notice: {notice}"));
                chat.last_child_notice = Some(notice.clone());
            }
            if !chat.shown || !lines.is_empty() || history_notice.is_some() || transient.is_some() {
                linear.write_agent_chat(
                    &mut io::stderr(),
                    &lines,
                    history_notice.or(transient),
                    true,
                )?;
                chat.shown = true;
            }
            return Ok(());
        }
        let classic = self.mode == TerminalMode::Classic;
        let screen = self.owned.terminal().size()?;
        self.prepare_frame_height(if classic {
            AGENT_CHAT_ROWS
        } else {
            screen.height
        })?;
        if classic {
            self.write_native_agent_chat()?;
            if let Some(text) = history_notice
                && self
                    .transcript
                    .native
                    .last_notice
                    .as_ref()
                    .is_none_or(|(sequence, prior)| {
                        *sequence != view.last_sequence || prior != text
                    })
            {
                let (kind, body) = notice_kind(text);
                self.insert_native(kind, body, None)?;
                self.transcript.native.last_notice = Some((view.last_sequence, text.to_owned()));
            }
        } else if let Some(text) = history_notice {
            // The primary keeps the notice for when it is shown again.
            self.transcript
                .history
                .record_notice(TerminalNotice::History(text));
        }
        let palette = self.palette();
        let armed = self.panels.dialog_armed;
        let target = self.agent_chat.as_ref().and_then(AgentChat::target);
        let draft = target
            .and_then(|target| {
                self.child_drafts
                    .iter()
                    .find(|draft| draft.target == target)
            })
            .map(|draft| &draft.composer);
        let chat = self.agent_chat.as_mut().expect("open agent chat");
        if !classic && let Some(transcript) = &chat.transcript {
            // Notices follow the rows they describe, so History takes the new records first.
            chat.history.sync(
                &transcript.view,
                usize::from(screen.width),
                usize::from(screen.height.saturating_sub(AGENT_CHAT_ROWS)),
            );
            for part in transcript.unappended(&mut chat.appended) {
                if let TranscriptPart::Notice(text) = part {
                    chat.history.record_notice(TerminalNotice::History(text));
                }
            }
        }
        chat.shown = true;
        let label = chat.label().to_owned();
        let terminal = self.owned.terminal();
        terminal.draw(|frame| {
            let area = dialog_area(frame.area(), AGENT_CHAT_ROWS, classic);
            if !classic && let Some(transcript) = &chat.transcript {
                draw_history_above(frame, &mut chat.history, &transcript.view, area.y, palette);
            }
            draw_agent_chat_frame(
                frame,
                area,
                AgentChatFrame {
                    label: &label,
                    draft,
                    editable: chat.editable,
                },
                armed,
                transient.or(child_notice.as_deref()),
                !classic,
                palette,
            );
        })?;
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        // The child view offers no selection, so pointer gestures never reach the primary History.
        self.panels.history_area = None;
        Ok(())
    }

    // Classic appends whole records below the agent boundary and never repaints scrollback.
    fn write_native_agent_chat(&mut self) -> Result<(), TerminalError> {
        let chat = self.agent_chat.as_mut().expect("open agent chat");
        let Some(label) = chat.transcript.as_ref().map(|t| t.label.clone()) else {
            return Ok(());
        };
        let mut records = Vec::new();
        if !chat.shown {
            records.push((
                MessageKind::Notice,
                format!(
                    "Viewing {label} ({}); Esc returns to primary",
                    if chat.editable {
                        "message"
                    } else {
                        "read-only"
                    }
                ),
            ));
        }
        records.extend(chat.unappended().into_iter().map(|part| match part {
            TranscriptPart::Assignment(text) => (MessageKind::Assignment, text.to_owned()),
            TranscriptPart::SteeringNote(text) => (MessageKind::SteeringNote, text.to_owned()),
            TranscriptPart::Tool(text) => (MessageKind::Tool, text),
            TranscriptPart::Result(text) => (MessageKind::AgentResult, text.to_owned()),
            TranscriptPart::Notice(text) => {
                let (kind, body) = notice_kind(text);
                (kind, body.to_owned())
            }
        }));
        for (kind, text) in records {
            self.insert_native(kind, &text, None)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::NewlineShortcut;
    use super::*;

    fn active_child_view() -> SessionView {
        let mut view = crate::presentation::team_view();
        let workspace_identity = arany::WorkspaceIdentity {
            device: 1,
            inode: 2,
        };
        let facts = [
            arany::Event::SessionStarted {
                title: "team".into(),
                workspace_identity,
            },
            arany::Event::GoalStarted {
                include_paths: Some(Vec::new()),
                goal_id: arany::GoalId::new(),
                objective: "team".into(),
                workspace_identity,
                budget: Default::default(),
                resource_settings: Default::default(),
                phase: arany::GoalPhase::Implementation,
                autonomous: false,
                continues: None,
            },
        ];
        let events: Vec<_> = facts
            .into_iter()
            .zip(1..)
            .map(|(event, sequence)| arany::EventEnvelope {
                session_id: view.id,
                sequence,
                run_id: None,
                agent_run_id: None,
                event,
                created_at_ms: 0,
            })
            .collect();
        view.goals = SessionView::replay(view.id, &events)
            .unwrap()
            .unwrap()
            .goals;
        view.goals[0].status = arany::GoalStatus::Running;
        view.goals[0].active_run = Some(view.runs[0].id);
        view.goals[0].run_ids.push(view.runs[0].id);
        view.runs[0].status = arany::RunStatus::Active;
        for agent in &mut view.runs[0].agents {
            agent.status = arany::AgentStatus::Active;
            agent.result = None;
            agent.summary = None;
        }
        view
    }

    #[tokio::test]
    async fn linear_child_message_preserves_navigation_and_prints_commit_once() {
        async fn line(
            terminal: &mut AttachedTerminal,
            view: &SessionView,
            text: &str,
        ) -> AgentChatInput {
            for ch in text.chars() {
                terminal
                    .linear_agent_input(view, TerminalInput::Character(ch))
                    .await
                    .unwrap();
            }
            terminal
                .linear_agent_input(view, TerminalInput::Submit)
                .await
                .unwrap()
        }
        let mut view = active_child_view();
        let (run, targets) = agent_targets(&view).unwrap();
        let target = ChildSteeringTarget {
            session_id: view.id,
            run_id: run,
            agent_run_id: targets[2].id,
        };
        let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(Some(
            super::super::linear::LinearState::new("screen-reader"),
        ));
        terminal.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected: 2,
            transcript: AgentTranscript::from_session(&view, run, target.agent_run_id),
            appended: Appended::default(),
            history: History::agent(),
            listing: false,
            shown: false,
            choice: String::new(),
            invalid: false,
            editable: true,
            last_child_notice: None,
        });
        terminal.child_drafts.push(ChildDraft {
            target,
            composer: Composer::text_only(),
            offered: None,
            notice: None,
        });
        assert!(matches!(
            line(&mut terminal, &view, "/message ").await,
            AgentChatInput::Notice(_)
        ));
        assert!(matches!(
            line(
                &mut terminal,
                &view,
                &format!(
                    "/message {}",
                    "x".repeat(super::super::composer::MAX_DRAFT_BYTES + 1)
                )
            )
            .await,
            AgentChatInput::Notice(_)
        ));
        assert_eq!(terminal.agent_chat.as_ref().unwrap().selected, 2);
        for character in "/message a".chars() {
            terminal
                .linear_agent_input(&view, TerminalInput::Character(character))
                .await
                .unwrap();
        }
        terminal
            .linear_agent_input(&view, TerminalInput::Newline)
            .await
            .unwrap();
        terminal
            .linear_agent_input(&view, TerminalInput::LineContinued)
            .await
            .unwrap();
        assert!(matches!(
            terminal
                .linear_agent_input(&view, TerminalInput::LineRejected)
                .await
                .unwrap(),
            AgentChatInput::Notice(_)
        ));
        assert_eq!(terminal.agent_chat.as_ref().unwrap().choice, "/message a\n");
        terminal
            .linear_agent_input(&view, TerminalInput::Character('b'))
            .await
            .unwrap();
        assert!(
            matches!(terminal.linear_agent_input(&view, TerminalInput::Submit).await.unwrap(),
            AgentChatInput::ChildMessage { target: id, text } if id == target && text == "a\nb")
        );
        terminal.child_drafts[0].clear(true);
        for input in [
            TerminalInput::Character('a'),
            TerminalInput::Character('\\'),
            TerminalInput::Submit,
        ] {
            assert_eq!(
                terminal.child_drafts[0].composer.apply(input),
                ComposerEdit::Changed
            );
        }
        assert_eq!(terminal.child_drafts[0].composer.text(), "a\n");
        assert!(
            matches!(terminal.edit_child(&view, TerminalInput::Submit).await.unwrap(),
            AgentChatInput::ChildMessage { target: id, text } if id == target && text == "a\n")
        );
        terminal.child_drafts[0].clear(true);
        let action = line(&mut terminal, &view, "/message /literal").await;
        assert!(
            matches!(action, AgentChatInput::ChildMessage { target: id, text } if id == target && text == "/literal")
        );
        assert_eq!(terminal.child_drafts[0].composer.text(), "/literal");
        terminal.child_note_feedback(target, "waiting".into(), Some(("/literal".into(), 0)));
        terminal.withdraw_child_offer(target, async |_| None).await;
        assert!(matches!(
            terminal
                .edit_child(&view, TerminalInput::Submit)
                .await
                .unwrap(),
            AgentChatInput::Notice(
                "Steering note outcome unknown; check child history; draft retained"
            )
        ));

        let mut output = Vec::new();
        let mut state = super::super::linear::LinearState::new("screen-reader");
        let before = terminal.agent_chat.as_mut().unwrap().linear_draw_lines();
        state
            .write_agent_chat(&mut output, &before, None, true)
            .unwrap();
        terminal.agent_chat.as_mut().unwrap().shown = true;
        view.runs[0].agents[2]
            .steering_notes
            .push("/literal".into());
        view.last_sequence += 1;
        terminal.agent_chat.as_mut().unwrap().refresh(&view);
        let committed = terminal.agent_chat.as_mut().unwrap().linear_draw_lines();
        state
            .write_agent_chat(
                &mut output,
                &committed,
                Some("Steering note recorded for child 2"),
                true,
            )
            .unwrap();
        assert_eq!(
            committed,
            [
                "New activity from child 2:",
                crate::presentation::STEERING_NOTE_HEADING,
                "  /literal"
            ]
        );
        assert!(
            terminal
                .agent_chat
                .as_mut()
                .unwrap()
                .linear_draw_lines()
                .is_empty()
        );
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output
                .matches(crate::presentation::STEERING_NOTE_HEADING)
                .count(),
            1
        );
        assert!(output.contains("number opens, n next, p previous, q returns to primary:"));
        assert!(!output.contains(['\r', '\u{1b}']));
        view.runs[0].agents[2].status = arany::AgentStatus::Finished;
        assert!(matches!(
            line(&mut terminal, &view, "/message other").await,
            AgentChatInput::Notice(_)
        ));
        assert_eq!(terminal.child_drafts[0].composer.text(), "/literal");
        assert!(
            matches!(line(&mut terminal, &view, "/clear").await, AgentChatInput::ChildEdited(id) if id == target)
        );
        assert!(terminal.child_drafts[0].composer.is_empty());
    }

    fn filled_draft(run_id: RunId) -> ChildDraft {
        let mut composer = Composer::text_only();
        composer.insert_paste("kept").unwrap();
        ChildDraft {
            target: ChildSteeringTarget {
                session_id: SessionId::new(),
                run_id,
                agent_run_id: arany::AgentRunId::new(),
            },
            composer,
            offered: None,
            notice: None,
        }
    }

    #[tokio::test]
    async fn pointer_input_on_a_read_only_child_is_handled_silently() {
        let mut view = active_child_view();
        let (run, targets) = agent_targets(&view).unwrap();
        view.runs[0].agents[2].status = arany::AgentStatus::Finished;
        let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(None);
        terminal.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected: 2,
            transcript: None,
            appended: Appended::default(),
            history: History::agent(),
            listing: false,
            shown: false,
            choice: String::new(),
            invalid: false,
            editable: false,
            last_child_notice: None,
        });
        for input in [
            TerminalInput::PointerMove { column: 1, row: 1 },
            TerminalInput::PointerClick {
                column: 1,
                row: 1,
                clicks: 1,
            },
            TerminalInput::CopySelection,
            TerminalInput::HistoryFind,
        ] {
            let action = terminal
                .agent_chat_input(&view, input, &Composer::text_only())
                .await
                .unwrap();
            assert!(matches!(action, AgentChatInput::Handled), "{input:?}");
        }
    }

    #[tokio::test]
    async fn repeated_child_feedback_is_announced_again() {
        let view = active_child_view();
        let (run, targets) = agent_targets(&view).unwrap();
        let target = ChildSteeringTarget {
            session_id: view.id,
            run_id: run,
            agent_run_id: targets[2].id,
        };
        let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(None);
        terminal.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected: 2,
            transcript: None,
            appended: Appended::default(),
            history: History::agent(),
            listing: false,
            shown: true,
            choice: String::new(),
            invalid: false,
            editable: true,
            last_child_notice: Some("No room".into()),
        });
        let mut draft = filled_draft(run);
        draft.target = target;
        terminal.child_drafts = vec![draft];
        terminal.child_note_feedback(target, "No room".into(), None);
        assert_eq!(
            terminal.agent_chat.as_ref().unwrap().last_child_notice,
            None
        );
    }

    #[tokio::test]
    async fn blank_child_draft_is_not_submitted_and_stays_unchanged() {
        let view = active_child_view();
        let (run, targets) = agent_targets(&view).unwrap();
        let target = ChildSteeringTarget {
            session_id: view.id,
            run_id: run,
            agent_run_id: targets[2].id,
        };
        let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(None);
        terminal.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected: 2,
            transcript: None,
            appended: Appended::default(),
            history: History::agent(),
            listing: false,
            shown: false,
            choice: String::new(),
            invalid: false,
            editable: true,
            last_child_notice: None,
        });
        let mut draft = filled_draft(run);
        draft.target = target;
        draft.composer.clear();
        draft.composer.insert_paste("  ").unwrap();
        terminal.child_drafts = vec![draft];
        assert!(matches!(
            terminal
                .edit_child(&view, TerminalInput::Submit)
                .await
                .unwrap(),
            AgentChatInput::Notice("Child message is empty; message not offered")
        ));
        assert_eq!(terminal.child_drafts[0].composer.text(), "  ");
    }

    #[tokio::test]
    async fn full_draft_table_refuses_without_dropping_the_shown_chat_and_prunes_old_runs() {
        let view = active_child_view();
        let (run, targets) = agent_targets(&view).unwrap();
        let target = ChildSteeringTarget {
            session_id: view.id,
            run_id: run,
            agent_run_id: targets[2].id,
        };
        let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(Some(
            super::super::linear::LinearState::new("screen-reader"),
        ));
        terminal.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected: 0,
            transcript: None,
            appended: Appended::default(),
            history: History::agent(),
            listing: true,
            shown: false,
            choice: String::new(),
            invalid: false,
            editable: false,
            last_child_notice: None,
        });
        terminal.child_drafts = (0..MAX_CHILD_DRAFTS).map(|_| filled_draft(run)).collect();
        for ch in "3".chars() {
            terminal
                .linear_agent_input(&view, TerminalInput::Character(ch))
                .await
                .unwrap();
        }
        let action = terminal
            .linear_agent_input(&view, TerminalInput::Submit)
            .await
            .unwrap();
        assert!(matches!(action, AgentChatInput::Notice(DRAFT_CAP_NOTICE)));
        assert!(!terminal.agent_chat.as_ref().unwrap().targets.is_empty());

        terminal.child_drafts = (0..MAX_CHILD_DRAFTS)
            .map(|_| filled_draft(RunId::new()))
            .collect();
        assert!(terminal.open_child_draft(&view, target));
        assert_eq!(terminal.child_drafts.len(), 1);
    }

    #[tokio::test]
    async fn child_drafts_primary_capacity_and_shortcut_survive_a_released_terminal() {
        for shortcut in [NewlineShortcut::AltEnter, NewlineShortcut::CtrlEnter] {
            let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(None);
            terminal.set_newline_shortcut(shortcut);
            terminal.child_drafts = vec![filled_draft(RunId::new())];
            terminal.primary_input_capacity = 7;
            terminal.replace_released(super::super::clipboard::cleanup_tests::released_terminal(
                None,
            ));
            assert_eq!(terminal.child_drafts.len(), 1);
            assert_eq!(terminal.primary_input_capacity, 7);
            assert_eq!(terminal.newline_shortcut, shortcut);
        }
    }

    #[tokio::test]
    async fn child_message_input_stays_with_its_armed_target() {
        let mut terminal = super::super::clipboard::cleanup_tests::released_terminal(None);
        let mut view = crate::presentation::team_view();
        let target = ChildSteeringTarget {
            session_id: view.id,
            run_id: view.runs[0].id,
            agent_run_id: view.runs[0].agents[2].id,
        };
        let sibling = ChildSteeringTarget {
            agent_run_id: view.runs[0].agents[1].id,
            ..target
        };
        let mut primary = Composer::default();
        primary.restore_prompt("primary draft").unwrap();
        primary.apply(TerminalInput::Left);
        let caret = primary.cursor_byte_offset();
        let mut composer = Composer::text_only();
        composer.restore_prompt("/literal").unwrap();
        assert!(!composer.has_open_menu());
        assert!(composer.file_query().is_none());
        assert!(matches!(
            composer.submission(),
            Ok(super::super::Submission::Objective("/literal"))
        ));
        composer.apply(TerminalInput::Tab);
        assert_eq!(composer.text(), "/literal");
        terminal.child_drafts.push(ChildDraft {
            target,
            composer,
            offered: None,
            notice: None,
        });
        terminal.child_note_feedback(target, "waiting".into(), Some(("/literal".into(), 0)));
        view.runs[0].agents[1]
            .steering_notes
            .push("/literal".into());
        assert!(
            terminal
                .settle_child_notes(&view, async |_| None, false)
                .await
        );
        assert_eq!(
            terminal.child_drafts[0].composer.text(),
            "/literal",
            "sibling cannot acknowledge target"
        );
        assert!(
            terminal.child_drafts[0]
                .notice
                .as_ref()
                .unwrap()
                .contains("unknown")
        );
        view.runs[0].agents[2]
            .steering_notes
            .push("/literal".into());
        assert!(
            terminal
                .settle_child_notes(&view, async |_| None, false)
                .await
        );
        assert!(terminal.child_drafts[0].composer.is_empty());
        assert_eq!(
            terminal.child_drafts[0].notice.as_deref(),
            Some("Steering note recorded for child 2")
        );
        assert!(
            !terminal
                .settle_child_notes(&view, async |_| None, false)
                .await
        );
        terminal.child_drafts[0]
            .composer
            .insert_paste("newer")
            .unwrap();
        terminal.child_note_feedback(target, "waiting".into(), Some(("older".into(), 1)));
        view.runs[0].agents[2].steering_notes.push("older".into());
        terminal
            .settle_child_notes(&view, async |_| None, false)
            .await;
        assert_eq!(terminal.child_drafts[0].composer.text(), "newer");
        assert_eq!(primary.text(), "primary draft");
        assert_eq!(primary.cursor_byte_offset(), caret);
        assert!(!editable_child(&view, sibling));
        terminal.child_note_feedback(
            ChildSteeringTarget {
                run_id: RunId::new(),
                ..target
            },
            "stale".into(),
            None,
        );
        assert_ne!(terminal.child_drafts[0].notice.as_deref(), Some("stale"));
    }

    #[test]
    fn a_linear_listing_prints_once_across_redraws() {
        let view = crate::presentation::team_view();
        let (run, targets) = agent_targets(&view).unwrap();
        let mut chat = AgentChat {
            session: view.id,
            run,
            targets,
            selected: 0,
            transcript: None,
            appended: Appended::default(),
            history: History::agent(),
            listing: true,
            shown: false,
            choice: String::new(),
            invalid: false,
            editable: false,
            last_child_notice: None,
        };
        let first = chat.linear_draw_lines();
        assert_eq!(first[0], "Agent chats in this Run:");
        assert_eq!(first.len(), 4);
        chat.shown = true;
        // The armed redraw after the barrier, and later Event bursts, repeat nothing.
        assert!(chat.linear_draw_lines().is_empty());
        assert!(chat.linear_draw_lines().is_empty());
    }

    #[test]
    fn agent_keys_switch_only_from_an_idle_composer_wrap_and_escape_returns() {
        use TerminalInput::{AgentNext as Next, AgentPrevious as Previous, Escape, Left, Right};
        for (input, viewing, count, admitted, expected) in [
            // An empty composer with no popup cycles primary → children → primary, both ways.
            (Next, None, 3, true, Step::Select(1)),
            (Previous, None, 3, true, Step::Select(2)),
            (Next, Some(1), 3, true, Step::Select(2)),
            (Next, Some(2), 3, true, Step::Select(0)),
            (Previous, Some(1), 3, true, Step::Select(0)),
            (Previous, Some(2), 3, true, Step::Select(1)),
            // A draft, image, menu, clipboard read or modal keeps the keys with its owner.
            (Next, None, 3, false, Step::Ignore),
            (Previous, None, 3, false, Step::Ignore),
            // Plain arrows keep their composer meaning; Escape only returns from a child.
            (Left, None, 3, true, Step::Pass),
            (Right, None, 3, true, Step::Pass),
            (Escape, None, 3, true, Step::Pass),
            (Escape, Some(2), 3, true, Step::Select(0)),
            (Escape, Some(0), 1, true, Step::Select(0)),
            // Without children nothing opens.
            (Next, None, 1, true, Step::NoChildren),
            (Previous, None, 0, true, Step::NoChildren),
            // A stale selection beyond a shorter Run stays in range.
            (Next, Some(9), 3, true, Step::Select(0)),
        ] {
            assert_eq!(
                agent_key(input, viewing, count, admitted),
                expected,
                "{input:?} viewing {viewing:?} of {count}, admitted {admitted}"
            );
        }
    }
}
