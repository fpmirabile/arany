use super::history::{History, MessageKind, notice_kind};
use super::view::{AGENT_CHAT_ROWS, draw_agent_chat_frame};
use super::{
    AttachedTerminal, Composer, TerminalError, TerminalInput, TerminalMode, TerminalNotice,
    dialog_area, draw_history_above,
};
use crate::presentation::{AgentTarget, AgentTranscript, agent_targets, tool_groups};
use arany::{RunId, SessionId, SessionView};
use std::io;

const MAX_CHOICE_BYTES: usize = 16;
const NO_CHILDREN: &str = "No child agents in this Run";
const NO_SUCH_AGENT: &str = "No such agent in this Run; /transcript agents lists them";
const MISSING_AGENT: &str = "That agent is no longer in this Run; back to primary";
const PRIMARY_SHOWN: &str = "The primary chat is already shown";
const BACK_TO_PRIMARY: &str = "Back to primary";
const PRIMARY_LINE: &str = "Viewing primary; its conversation is above";
const INVALID_CHOICE: &str = "Invalid agent choice; type a number, n, p or q";

pub enum AgentChatInput {
    // The input keeps its ordinary meaning.
    Pass,
    Handled,
    // The primary chat owns input again; the caller redraws it.
    Returned,
    Notice(&'static str),
}

// One selected read-only snapshot; the primary keeps its own History, draft and anchors.
pub(super) struct AgentChat {
    session: SessionId,
    run: RunId,
    // Numbered choices bind to this snapshot of the Run's agents.
    targets: Vec<AgentTarget>,
    selected: usize,
    transcript: Option<AgentTranscript>,
    history: History,
    listing: bool,
    shown: bool,
    choice: String,
    invalid: bool,
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
    fn label(&self) -> &str {
        self.transcript
            .as_ref()
            .map_or("primary", |transcript| transcript.label.as_str())
    }

    fn linear_lines(&self) -> Vec<String> {
        if let Some(transcript) = &self.transcript {
            return transcript.linear_lines();
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

    // Alt+Left/Right from an empty composer, and every input while a child transcript is shown.
    pub async fn agent_chat_input(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
        composer: &Composer,
    ) -> Result<AgentChatInput, TerminalError> {
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
                && !self.transcript_open
                && !self.scrollback_dump;
            return self.navigate(view, input, admitted).await;
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
                self.suspend_and_resume(0)?;
                self.rearm_agent_chat(view).await
            }
            TerminalInput::Resize => self.rearm_agent_chat(view).await,
            TerminalInput::Paste => {
                let _ = self.take_paste();
                Ok(AgentChatInput::Handled)
            }
            TerminalInput::AgentNext | TerminalInput::AgentPrevious | TerminalInput::Escape => {
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
            // The composer is disabled: text, Enter and other keys neither edit nor submit.
            _ => Ok(AgentChatInput::Handled),
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
        self.cancel_clipboard_paste();
        self.agent_chat = Some(AgentChat {
            session: view.id,
            run,
            targets,
            selected,
            transcript,
            history: History::agent(),
            listing,
            shown: false,
            choice: String::new(),
            invalid: false,
        });
        self.rearm_agent_chat(view).await
    }

    // Draws unarmed, discards input read before this view existed, then draws armed.
    async fn rearm_agent_chat(
        &mut self,
        view: &SessionView,
    ) -> Result<AgentChatInput, TerminalError> {
        self.begin_dialog();
        self.draw_agent_chat(view, None)?;
        self.arm_dialog().await?;
        self.draw_agent_chat(view, None)?;
        Ok(AgentChatInput::Handled)
    }

    async fn leave_agent_chat(&mut self) -> Result<AgentChatInput, TerminalError> {
        self.end_agent_chat()?;
        // Keys meant for the child view must not reach the primary draft.
        self.begin_dialog();
        self.arm_dialog().await?;
        Ok(AgentChatInput::Returned)
    }

    // Closes the view without input admission, as when Tool approval preempts it.
    pub(super) fn end_agent_chat(&mut self) -> Result<(), TerminalError> {
        let Some(chat) = self.agent_chat.take() else {
            return Ok(());
        };
        if let Some(linear) = &mut self.linear {
            linear.write_agent_chat(&mut io::stderr(), &[BACK_TO_PRIMARY.into()], None, false)?;
            linear.prompt_needed = true;
        } else if self.mode == TerminalMode::Classic && chat.shown {
            // Settled native rows stay; the boundary marks where the primary resumes.
            self.prepare_frame()?;
            self.insert_native(MessageKind::Notice, BACK_TO_PRIMARY)?;
        }
        Ok(())
    }

    async fn linear_agent_input(
        &mut self,
        view: &SessionView,
        input: TerminalInput,
    ) -> Result<AgentChatInput, TerminalError> {
        let chat = self.agent_chat.as_mut().expect("open agent chat");
        match input {
            TerminalInput::Character(character) => {
                if character.is_ascii_graphic() && chat.choice.len() < MAX_CHOICE_BYTES {
                    chat.choice.push(character);
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
                let count = chat.targets.len();
                let selected = chat.selected;
                let next = if std::mem::take(&mut chat.invalid) {
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
                let targets = std::mem::take(&mut chat.targets);
                self.show_agent(view, run, targets, index, false).await
            }
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
                self.suspend_and_resume(0)?;
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
            .write_agent_chat(&mut io::stderr(), &[], Some(INVALID_CHOICE), true)?;
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
        let (history_notice, transient) = match notice {
            Some(TerminalNotice::History(text)) => (Some(text), None),
            Some(TerminalNotice::Transient(text)) => (None, Some(text)),
            None => (None, None),
        };
        if let Some(linear) = &mut self.linear {
            let chat = self.agent_chat.as_mut().expect("open agent chat");
            if !chat.shown || history_notice.is_some() {
                let lines = if chat.shown {
                    Vec::new()
                } else {
                    chat.linear_lines()
                };
                linear.write_agent_chat(&mut io::stderr(), &lines, history_notice, true)?;
                chat.shown = true;
            }
            return Ok(());
        }
        let classic = self.mode == TerminalMode::Classic;
        let screen = self.terminal.as_mut().expect("owned terminal").size()?;
        self.prepare_frame_height(if classic {
            AGENT_CHAT_ROWS
        } else {
            screen.height
        })?;
        if classic {
            if !self.agent_chat.as_ref().expect("open agent chat").shown {
                self.write_native_agent_chat()?;
            }
            if let Some(text) = history_notice
                && self
                    .native
                    .last_notice
                    .as_ref()
                    .is_none_or(|(sequence, prior)| {
                        *sequence != view.last_sequence || prior != text
                    })
            {
                let (kind, body) = notice_kind(text);
                self.insert_native(kind, body)?;
                self.native.last_notice = Some((view.last_sequence, text.to_owned()));
            }
        } else if let Some(text) = history_notice {
            // The primary keeps the notice for when it is shown again.
            self.history.record_notice(TerminalNotice::History(text));
        }
        let palette = self.palette();
        let armed = self.dialog_armed;
        let chat = self.agent_chat.as_mut().expect("open agent chat");
        if !classic
            && !chat.shown
            && let Some(transcript) = &chat.transcript
        {
            chat.history.sync(
                &transcript.view,
                usize::from(screen.width),
                usize::from(screen.height.saturating_sub(AGENT_CHAT_ROWS)),
            );
            for text in &transcript.notices {
                chat.history.record_notice(TerminalNotice::History(text));
            }
        }
        chat.shown = true;
        let label = chat.label().to_owned();
        let terminal = self.terminal.as_mut().expect("owned terminal");
        terminal.draw(|frame| {
            let area = dialog_area(frame.area(), AGENT_CHAT_ROWS, classic);
            if !classic && let Some(transcript) = &chat.transcript {
                draw_history_above(frame, &mut chat.history, &transcript.view, area.y, palette);
            }
            draw_agent_chat_frame(frame, area, &label, armed, transient, !classic, palette);
        })?;
        let area = terminal.get_frame().area();
        terminal.backend_mut().set_area(area);
        // The child view offers no selection, so pointer gestures never reach the primary History.
        self.history_area = None;
        Ok(())
    }

    // Classic appends the snapshot once below an unmistakable read-only boundary.
    fn write_native_agent_chat(&mut self) -> Result<(), TerminalError> {
        let chat = self.agent_chat.as_ref().expect("open agent chat");
        let Some(transcript) = &chat.transcript else {
            return Ok(());
        };
        let run = transcript.run();
        let mut parts = vec![
            (
                MessageKind::Notice,
                format!(
                    "Viewing {} (read-only); Esc returns to primary",
                    transcript.label
                ),
            ),
            (MessageKind::Assignment, run.objective.clone()),
        ];
        parts.extend(tool_groups(&run.tools).map(|(_, text)| (MessageKind::Tool, text)));
        if let Some(answer) = &run.assistant_message {
            parts.push((MessageKind::AgentResult, answer.clone()));
        }
        parts.extend(transcript.notices.iter().map(|text| {
            let (kind, body) = notice_kind(text);
            (kind, body.to_owned())
        }));
        for (kind, text) in parts {
            self.insert_native(kind, &text)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
