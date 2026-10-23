use super::{
    Admission, EMPTY_INTERRUPT_WINDOW, Feedback, Selector,
    active::{self, CompactionOutcome, GoalStart, compact_current},
    append_notice, archive_notice_drawn, close_engine, configure_account, consume_local_command,
    controls, feedback_or_end, load_view, materialize_session, permissions,
    picker::{QuickChoice, pick_quick_action},
    readmit_host_permissions, refresh_runtime_skills, reload_view_or_keep, retained_draft_notice,
    rewind, run_selector,
};
use crate::terminal::{
    AgentChatFollow, AgentChatInput, AttachedTerminal, Composer, ComposerEdit, InteractiveCommand,
    Submission, TerminalInput, TerminalMode, transcript_agents,
};
use arany::{Engine, SessionId, SessionView, native_protection_supported};
use std::{io::Write, time::Instant};

pub(super) struct AttachedSession {
    ui: IdleUi,
    engine: Engine,
    view: SessionView,
    archived_history: Option<arany::ArchivedHistory>,
    composer: Composer,
    terminal: AttachedTerminal,
    admission: Admission,
    session_id: SessionId,
}

struct IdleUi {
    notice: Option<Feedback>,
    archive_notice: Option<String>,
    empty_interrupt: Option<Instant>,
    idle_escape: Option<Instant>,
    saved_view: TerminalMode,
    skills_supported: bool,
}

pub(super) struct SessionParts {
    pub(super) notice: Option<Feedback>,
    pub(super) archive_notice: Option<String>,
    pub(super) engine: Engine,
    pub(super) view: SessionView,
    pub(super) archived_history: Option<arany::ArchivedHistory>,
    pub(super) composer: Composer,
    pub(super) terminal: AttachedTerminal,
    pub(super) admission: Admission,
    pub(super) session_id: SessionId,
    pub(super) saved_view: TerminalMode,
    pub(super) skills_supported: bool,
}

enum IdleAction {
    Continue,
    Handled,
    Exit,
    Close,
}

enum Routed {
    Handled,
    Pass(TerminalInput),
}

enum SubmissionAdmission {
    Ready,
    Rejected,
}

enum FolderPermissions {
    Selected(arany::WorkspacePermissions),
    Handled,
    Close,
}

impl AttachedSession {
    pub(super) fn new(parts: SessionParts) -> Self {
        let SessionParts {
            notice,
            archive_notice,
            engine,
            view,
            archived_history,
            composer,
            terminal,
            admission,
            session_id,
            saved_view,
            skills_supported,
        } = parts;
        Self {
            ui: IdleUi {
                notice,
                archive_notice,
                empty_interrupt: None,
                idle_escape: None,
                saved_view,
                skills_supported,
            },
            engine,
            view,
            archived_history,
            composer,
            terminal,
            admission,
            session_id,
        }
    }

    pub(super) async fn run(mut self) -> Result<(), String> {
        loop {
            self.refresh_composer();
            self.draw_idle().await?;
            let input = self.next_input().await?;
            let input = match self.route_input(input).await? {
                Routed::Handled => continue,
                Routed::Pass(input) => input,
            };
            match self.handle_input(input).await? {
                IdleAction::Continue | IdleAction::Handled => {}
                IdleAction::Exit => break,
                IdleAction::Close => return close_engine(self.engine).await,
            }
        }
        self.finish().await
    }

    async fn draw_idle(&mut self) -> Result<(), String> {
        if self.terminal.agent_chat_open()
            && let AgentChatFollow::Closed(text) = self
                .terminal
                .follow_agent_chat(&self.view)
                .await
                .map_err(|error| error.to_string())?
        {
            self.ui.notice = Some(Feedback::History(text.into()));
        }
        self.terminal
            .draw_notice(
                &self.view,
                &self.composer,
                self.ui.notice.as_ref().map(Feedback::notice),
            )
            .map_err(|error| error.to_string())?;
        let archive_drawn =
            archive_notice_drawn(self.ui.notice.as_ref(), self.ui.archive_notice.as_deref());
        self.ui.notice = self
            .archived_history
            .take_if(|_| archive_drawn)
            .and_then(|archived| archived.acknowledge().err())
            .map(|error| {
                Feedback::History(format!("Error: {error}; the archive notice may show again"))
            });
        self.terminal
            .restore_selected_draft_input(self.composer.input_capacity())
            .map_err(|error| error.to_string())?;

        Ok(())
    }

    async fn next_input(&mut self) -> Result<TerminalInput, String> {
        loop {
            let input = self
                .terminal
                .next_chat_input()
                .await
                .map_err(|error| error.to_string())?;
            self.composer.observe_input(input);
            if self.terminal.agent_chat_open() || self.terminal.help_open() {
                self.composer.disarm_continuation();
            }
            if !self.terminal.help_open()
                && (self.terminal.ignores_page_key(input) || self.terminal.ignores_pointer(input))
            {
                continue;
            }
            return Ok(input);
        }
    }

    async fn route_input(&mut self, input: TerminalInput) -> Result<Routed, String> {
        if self.terminal.help_open()
            && !matches!(input, TerminalInput::Suspend | TerminalInput::Shutdown(_))
        {
            self.terminal.help_input(input);
            return Ok(Routed::Handled);
        }
        match self
            .terminal
            .agent_chat_input(&self.view, input, &self.composer)
            .await
            .map_err(|error| error.to_string())?
        {
            AgentChatInput::ChildMessage { .. } => {
                self.ui.notice = Some(Feedback::Transient(
                    "Historical children are read-only; draft retained".into(),
                ));
                return Ok(Routed::Handled);
            }
            AgentChatInput::ChildEdited(_) => return Ok(Routed::Handled),
            AgentChatInput::Pass => {}
            AgentChatInput::Handled | AgentChatInput::Returned => {
                self.ui.idle_escape = None;
                self.ui.empty_interrupt = None;
                return Ok(Routed::Handled);
            }
            AgentChatInput::Notice(text) => {
                self.ui.idle_escape = None;
                self.ui.empty_interrupt = None;
                self.ui.notice = Some(Feedback::Transient(text.into()));
                return Ok(Routed::Handled);
            }
        }
        if self.terminal.handle_draft_detail(&mut self.composer, input) {
            return Ok(Routed::Handled);
        }
        if self
            .terminal
            .handle_transcript_input(input, &self.view)
            .map_err(|error| error.to_string())?
        {
            return Ok(Routed::Handled);
        }
        if self
            .terminal
            .handle_history_input(input, &self.view, &self.composer, false)
            .await
        {
            return Ok(Routed::Handled);
        }

        Ok(Routed::Pass(input))
    }

    async fn handle_input(&mut self, input: TerminalInput) -> Result<IdleAction, String> {
        if input != TerminalInput::Escape {
            self.ui.idle_escape = None;
        }
        let input = self.composer.chat_input(input);
        match input {
            TerminalInput::Escape if !self.composer.has_open_menu() => self.escape().await?,
            TerminalInput::QuickActions if !self.terminal.is_linear() => {
                self.quick_action().await?
            }
            TerminalInput::Submit => return self.submit().await,
            TerminalInput::Interrupt => return Ok(self.interrupt()),
            TerminalInput::EndOfInput if self.composer.is_empty() => return Ok(IdleAction::Exit),
            TerminalInput::EndOfInput => {
                self.ui.notice = Some(Feedback::History(
                    "Draft retained; press Enter to submit or Ctrl+C to clear".into(),
                ));
            }
            TerminalInput::Suspend => {
                self.terminal
                    .suspend_and_resume(self.composer.input_capacity())
                    .map_err(|error| error.to_string())?;
                self.ui.notice = Some(Feedback::Transient("Terminal resumed".into()));
            }
            TerminalInput::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
            input => self.edit_input(input),
        }
        Ok(IdleAction::Continue)
    }

    async fn escape(&mut self) -> Result<(), String> {
        if self
            .ui
            .idle_escape
            .take()
            .is_some_and(|at| at.elapsed() <= EMPTY_INTERRUPT_WINDOW)
        {
            if !self.composer.is_empty() {
                let saved = self.composer.remember_and_clear();
                self.ui.notice = Some(Feedback::History(
                    if saved {
                        "Draft saved to input history; Up recalls it"
                    } else {
                        "Other drafts are waiting; submit or clear this draft, then press Up"
                    }
                    .into(),
                ));
            } else {
                self.ui.notice = Some(Feedback::History(
                    rewind::open(
                        &self.engine,
                        &mut self.terminal,
                        &self.admission,
                        &mut self.session_id,
                        &mut self.view,
                        &mut self.composer,
                    )
                    .await?,
                ));
            }
        } else {
            self.ui.idle_escape = Some(Instant::now());
            self.ui.notice = Some(Feedback::Transient(
                if self.composer.is_empty() {
                    "Esc again: rewind"
                } else {
                    "Esc again: save and clear draft"
                }
                .into(),
            ));
        }

        Ok(())
    }

    fn edit_input(&mut self, input: TerminalInput) {
        match input {
            TerminalInput::CycleApprovalMode => {
                self.ui.notice = Some(Feedback::History(permissions::cycle(&mut self.composer)));
            }
            TerminalInput::ClipboardPaste => self.clipboard_paste(),
            TerminalInput::Paste => {
                if let Err(error) = self.terminal.paste_into(&mut self.composer) {
                    self.ui.notice = Some(Feedback::History(format!(
                        "Error: {error}; draft unchanged"
                    )));
                }
            }
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
            | TerminalInput::Escape) => {
                if self.composer.apply(input) == ComposerEdit::AtCapacity {
                    self.ui.notice = Some(Feedback::History(
                        "Error: message is too long; shorten it; draft unchanged".into(),
                    ));
                }
            }
            TerminalInput::LineRejected => {
                self.ui.notice = Some(Feedback::History(
                    "Error: invalid or overlong terminal line; draft unchanged".into(),
                ));
            }
            TerminalInput::LineContinued => {
                self.ui.notice = Some(Feedback::History(format!(
                    "Draft: {} characters; Enter submits",
                    self.composer.character_count()
                )));
            }
            TerminalInput::IgnoredKey
            | TerminalInput::ModifiedEnter(_)
            | TerminalInput::PageUp
            | TerminalInput::PageDown
            | TerminalInput::HalfPageUp
            | TerminalInput::QuickActions
            | TerminalInput::HistoryFind
            | TerminalInput::HistoryLive
            | TerminalInput::Transcript
            | TerminalInput::AgentPrevious
            | TerminalInput::AgentNext
            | TerminalInput::Submit
            | TerminalInput::Interrupt
            | TerminalInput::EndOfInput
            | TerminalInput::Suspend
            | TerminalInput::Shutdown(_)
            | TerminalInput::Resize
            | TerminalInput::CopySelection
            | TerminalInput::PointerMove { .. }
            | TerminalInput::PointerClick { .. }
            | TerminalInput::PointerDrag { .. }
            | TerminalInput::PointerRelease { .. }
            | TerminalInput::PointerScrollUp { .. }
            | TerminalInput::PointerScrollDown { .. } => return,
        }
        self.ui.empty_interrupt = None;
    }

    async fn quick_action(&mut self) -> Result<(), String> {
        match pick_quick_action(&mut self.terminal, &self.composer, &self.view).await? {
            QuickChoice::Selected(Selector::Resume) if !self.composer.is_empty() => {
                self.ui.notice = Some(Feedback::History(
                    "Submit or clear this draft before switching Sessions".into(),
                ));
            }
            QuickChoice::Selected(selector) => self.select(selector).await?,
            QuickChoice::Closed => {}
            QuickChoice::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
        }
        self.ui.empty_interrupt = None;
        Ok(())
    }

    async fn submit(&mut self) -> Result<IdleAction, String> {
        if !self.terminal.is_linear() && self.composer.continue_line() {
            self.ui.empty_interrupt = None;
            return Ok(IdleAction::Continue);
        }
        if self.terminal.clipboard_loading() {
            self.ui.notice = Some(Feedback::Transient(
                "Clipboard loading; press Enter after paste to submit".into(),
            ));
            self.terminal
                .restore_draft_input(self.composer.input_capacity())
                .map_err(|error| error.to_string())?;
            self.ui.empty_interrupt = None;
            return Ok(IdleAction::Continue);
        }
        if !self.terminal.is_linear()
            && self.composer.apply(TerminalInput::Submit) == ComposerEdit::Changed
        {
            self.ui.empty_interrupt = None;
            return Ok(IdleAction::Continue);
        }
        match self.validate_submission() {
            SubmissionAdmission::Ready => {}
            SubmissionAdmission::Rejected => {
                self.ui.empty_interrupt = None;
                return Ok(IdleAction::Continue);
            }
        }
        let line = self.composer.text().to_owned();
        let submission = match if self.composer.has_text_pastes() {
            Ok(Submission::Objective(
                &line[self.composer.objective_escape_len()..],
            ))
        } else {
            self.composer.parse_submission(&line)
        } {
            Err(crate::terminal::CommandParseError::Empty)
                if !self.composer.images().is_empty() =>
            {
                Submission::Objective(&line)
            }
            result => result.expect("validated submission"),
        };
        consume_local_command(&mut self.composer, &submission);
        match self.execute_submission(submission, &line).await? {
            IdleAction::Continue => {
                self.ui.empty_interrupt = None;
                Ok(IdleAction::Continue)
            }
            IdleAction::Handled => Ok(IdleAction::Handled),
            IdleAction::Exit => Ok(IdleAction::Exit),
            IdleAction::Close => Ok(IdleAction::Close),
        }
    }

    fn validate_submission(&mut self) -> SubmissionAdmission {
        let validation = match self.composer.submission() {
            Err(error) => {
                self.reject_submission(error);
                return SubmissionAdmission::Rejected;
            }
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some(argument),
            }) if argument != "pause" => {
                let start = if argument == "resume" {
                    GoalStart::Resume
                } else if argument == "continue" {
                    GoalStart::Continue
                } else {
                    GoalStart::Autonomous
                };
                active::preflight_idle_run(
                    &self.admission,
                    &self.view,
                    start,
                    &self.composer.editable_text(),
                    !self.composer.images().is_empty(),
                    self.terminal.is_linear(),
                )
            }
            Ok(Submission::Command { command, argument }) => {
                if !self.composer.images().is_empty()
                    && matches!(
                        command,
                        InteractiveCommand::New
                            | InteractiveCommand::Resume
                            | InteractiveCommand::Rewind
                            | InteractiveCommand::Fork
                    )
                {
                    Err("Error: Submit or clear image attachments before switching Sessions".into())
                } else {
                    controls::validate_idle_command(&self.view, command, argument)
                        .map_err(|error| format!("Error: {error}"))
                }
            }
            Ok(Submission::Objective(_) | Submission::Skill { .. }) => active::preflight_idle_run(
                &self.admission,
                &self.view,
                GoalStart::Plain,
                &self.composer.editable_text(),
                !self.composer.images().is_empty(),
                self.terminal.is_linear(),
            ),
        };
        match validation {
            Ok(()) => SubmissionAdmission::Ready,
            Err(message) => {
                self.ui.notice = Some(Feedback::History(message));
                self.discard_linear_command();
                SubmissionAdmission::Rejected
            }
        }
    }

    fn reject_submission(&mut self, error: crate::terminal::CommandParseError) {
        match error {
            crate::terminal::CommandParseError::Empty => self.composer.clear(),
            crate::terminal::CommandParseError::Unknown { suggestion } => {
                self.ui.notice = Some(Feedback::History(match suggestion {
                    Some(name) => {
                        format!("Error: Unknown command; did you mean /{name}?")
                    }
                    None => "Error: Unknown command".into(),
                }));
            }
            error => self.ui.notice = Some(Feedback::History(format!("Error: {error}"))),
        }
        self.discard_linear_command();
    }

    fn discard_linear_command(&mut self) {
        if self.terminal.is_linear() && !self.composer.has_text_pastes() {
            self.composer.take();
        }
    }

    async fn submit_idle_run(
        &mut self,
        goal: GoalStart,
        objective: String,
        skill: Option<String>,
    ) -> Result<Option<Feedback>, String> {
        materialize_session(&self.admission, &mut self.session_id, &mut self.view).await?;
        let submitted = if matches!(goal, GoalStart::Resume | GoalStart::Continue) {
            self.composer.take();
            crate::terminal::ComposerDraft::default()
        } else {
            self.composer.take_draft()
        };
        active::PendingSubmission {
            engine: &self.engine,
            admission: &self.admission,
            goal,
            objective,
            skill,
            draft: Some(submitted),
        }
        .submit(&mut self.terminal, &mut self.view, &mut self.composer)
        .await
    }

    async fn execute_submission(
        &mut self,
        submission: Submission<'_>,
        line: &str,
    ) -> Result<IdleAction, String> {
        match submission {
            Submission::Objective(_) | Submission::Skill { .. } => {
                let (objective, skill) = match submission {
                    Submission::Objective(objective) => (objective.to_owned(), None),
                    Submission::Skill { name, .. } => (line.to_owned(), Some(name.to_owned())),
                    Submission::Command { .. } => unreachable!("task submission"),
                };
                self.ui.notice = self
                    .submit_idle_run(GoalStart::Plain, objective, skill)
                    .await?;
            }
            Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some(argument),
            } if argument != "pause" => {
                let (start, objective) = if argument == "resume" {
                    (GoalStart::Resume, String::new())
                } else if argument == "continue" {
                    (GoalStart::Continue, String::new())
                } else {
                    (GoalStart::Autonomous, argument.to_owned())
                };
                self.ui.notice = self.submit_idle_run(start, objective, None).await?;
            }
            Submission::Command { command, argument } => {
                return self.command(command, argument).await;
            }
        }
        Ok(IdleAction::Continue)
    }

    async fn command(
        &mut self,
        command: InteractiveCommand,
        argument: Option<&str>,
    ) -> Result<IdleAction, String> {
        match command {
            InteractiveCommand::Exit => return Ok(IdleAction::Exit),
            InteractiveCommand::Rewind => self.rewind().await?,
            InteractiveCommand::Paste => self.clipboard_paste(),
            InteractiveCommand::Copy => {
                self.ui.notice = Some(match self.terminal.copy_latest_answer(&self.view).await {
                    Ok(copied) => Feedback::Transient(copied),
                    Err(error) => Feedback::History(error),
                });
            }
            InteractiveCommand::Transcript => self.transcript(argument).await?,
            InteractiveCommand::Settings => self.settings().await?,
            InteractiveCommand::Permissions => return self.permissions().await,
            InteractiveCommand::Setup => {
                self.ui.notice = Some(
                    configure_account(
                        &mut self.terminal,
                        &mut self.admission,
                        self.session_id,
                        &mut self.view,
                        &mut self.composer,
                    )
                    .await?,
                );
            }
            InteractiveCommand::Usage | InteractiveCommand::Status => {
                self.terminal
                    .open_usage(&self.view, command == InteractiveCommand::Status, None)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            InteractiveCommand::Storage => {
                crate::cli::storage::attached(
                    &mut self.terminal,
                    self.admission.workspace.clone(),
                    self.admission.state_dir.clone(),
                    &self.view,
                    &mut self.composer,
                )
                .await?;
            }
            InteractiveCommand::Help => {
                self.terminal
                    .open_help(&self.view)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            InteractiveCommand::Resume if argument.is_none() => {
                self.select(Selector::Resume).await?
            }
            InteractiveCommand::Compact => return self.compact().await,
            InteractiveCommand::Model | InteractiveCommand::Agents if argument.is_none() => {
                self.select(if command == InteractiveCommand::Agents {
                    Selector::Agents
                } else {
                    Selector::Models
                })
                .await?;
            }
            InteractiveCommand::Resume
            | InteractiveCommand::Model
            | InteractiveCommand::Agents
            | InteractiveCommand::New
            | InteractiveCommand::Fork
            | InteractiveCommand::Goal
            | InteractiveCommand::Rename
            | InteractiveCommand::Provider
            | InteractiveCommand::Cache => {
                self.control_command(command, argument).await?;
            }
        }
        Ok(IdleAction::Continue)
    }

    async fn settings(&mut self) -> Result<(), String> {
        self.ui.notice = Some(Feedback::History(
            controls::settings(
                &mut self.terminal,
                &self.admission,
                &mut self.composer,
                &mut self.ui.saved_view,
                &self.view,
            )
            .await?,
        ));
        Ok(())
    }

    async fn rewind(&mut self) -> Result<(), String> {
        self.ui.notice = Some(Feedback::History(
            rewind::open(
                &self.engine,
                &mut self.terminal,
                &self.admission,
                &mut self.session_id,
                &mut self.view,
                &mut self.composer,
            )
            .await?,
        ));

        Ok(())
    }

    fn clipboard_paste(&mut self) {
        self.ui.notice = Some(match self.terminal.request_clipboard_paste() {
            Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
            Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
        });
    }

    async fn transcript(&mut self, argument: Option<&str>) -> Result<(), String> {
        match argument.and_then(transcript_agents) {
            Some(number) => {
                if let AgentChatInput::Notice(text) = self
                    .terminal
                    .open_agent_chats(&self.view, number)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    self.ui.notice = Some(Feedback::Transient(text.into()));
                }
            }
            None => self
                .terminal
                .toggle_transcript(&self.view)
                .map_err(|error| error.to_string())?,
        }

        Ok(())
    }

    async fn control_command(
        &mut self,
        command: InteractiveCommand,
        argument: Option<&str>,
    ) -> Result<(), String> {
        if matches!(
            command,
            InteractiveCommand::Resume | InteractiveCommand::New | InteractiveCommand::Fork
        ) {
            self.engine
                .revoke_host_approvals()
                .map_err(|error| error.to_string())?;
        }
        self.ui.notice = Some(feedback_or_end(
            controls::handle_command(
                &self.admission,
                &mut self.session_id,
                &mut self.view,
                command,
                argument,
            )
            .await,
        )?);

        Ok(())
    }

    async fn select(&mut self, selector: Selector) -> Result<(), String> {
        self.ui.notice = run_selector(
            &self.engine,
            selector,
            &mut self.terminal,
            &mut self.admission,
            &mut self.session_id,
            &mut self.view,
            &mut self.composer,
        )
        .await
        .transpose()
        .map(feedback_or_end)
        .transpose()?;
        Ok(())
    }

    async fn compact(&mut self) -> Result<IdleAction, String> {
        if self.view.created_sequence == 0 {
            self.ui.notice = Some(Feedback::History(
                "No messages to compact; send a message first".into(),
            ));
            return Ok(IdleAction::Handled);
        }
        let maintenance = match compact_current(
            &self.engine,
            &mut self.terminal,
            &self.admission,
            &self.view,
            &mut self.composer,
            None,
        )
        .await?
        {
            CompactionOutcome::Notice(text) | CompactionOutcome::Interrupted(text) => text,
            CompactionOutcome::Unavailable(error) => {
                format!("Error: {error}")
            }
            CompactionOutcome::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
        };
        let text = match retained_draft_notice(&self.composer) {
            Some(draft) => format!("{maintenance}; {draft}"),
            None => maintenance,
        };
        self.ui.notice = reload_view_or_keep(
            &mut self.view,
            load_view(&self.admission.state_dir, self.session_id).await,
            Some(text),
            None,
        )
        .map(Feedback::History);

        Ok(IdleAction::Continue)
    }

    async fn permissions(&mut self) -> Result<IdleAction, String> {
        self.engine
            .revoke_host_approvals()
            .map_err(|error| error.to_string())?;
        match permissions::choose_area(&mut self.terminal).await? {
            None => {
                self.terminal
                    .restore_draft_input(self.composer.input_capacity())
                    .map_err(|error| error.to_string())?;
                self.ui.notice = Some(Feedback::History("Permissions unchanged".into()));
                return Ok(IdleAction::Handled);
            }
            Some(permissions::PermissionArea::TypedGitRules) => {
                let summary = permissions::manage_rules(
                    &mut self.terminal,
                    &self.admission.state_dir,
                    &self.admission.workspace,
                )
                .await?;
                self.terminal
                    .restore_draft_input(self.composer.input_capacity())
                    .map_err(|error| error.to_string())?;
                self.ui.notice = Some(Feedback::History(summary));
                readmit_host_permissions(&self.engine, &mut self.ui.notice);
                return Ok(IdleAction::Handled);
            }
            Some(permissions::PermissionArea::FolderTrust) => {}
        }

        let access = match self.choose_folder_permissions().await? {
            FolderPermissions::Selected(access) => access,
            FolderPermissions::Handled => return Ok(IdleAction::Handled),
            FolderPermissions::Close => return Ok(IdleAction::Close),
        };
        self.ui.notice = Some(Feedback::History(if !native_protection_supported() {
            format!(
                "Folder {} · {}",
                if access.is_trusted() {
                    "trusted"
                } else {
                    "read only"
                },
                permissions::UNAVAILABLE_NOTICE
            )
        } else if access.is_trusted() {
            format!(
                "Folder trusted · {} · Shift+Tab changes mode",
                access.mode().label()
            )
        } else {
            "Read only · /permissions can enable guarded Tools".into()
        }));
        self.admission.workspace_permissions = Some(access);
        readmit_host_permissions(&self.engine, &mut self.ui.notice);
        self.admission.tools = false;
        refresh_runtime_skills(
            &self.admission,
            &mut self.composer,
            &mut self.ui.notice,
            self.ui.skills_supported,
        );

        Ok(IdleAction::Continue)
    }

    async fn choose_folder_permissions(&mut self) -> Result<FolderPermissions, String> {
        let permissions = match permissions::open_or_recover(
            &mut self.terminal,
            &self.admission.workspace,
            "Cancel",
        )
        .await?
        {
            permissions::TrustAdmission::Loaded(access) => access.prepare_profile_selection(),
            permissions::TrustAdmission::Exit => {
                self.ui.notice = Some(Feedback::History(
                    "Folder trust could not be loaded; existing permissions unchanged".into(),
                ));
                return Ok(FolderPermissions::Handled);
            }
        };
        let choice =
            permissions::choose(&mut self.terminal, permissions, &mut self.composer, false).await?;
        let access = match choice {
            permissions::PermissionChoice::Selected(access) => access,
            permissions::PermissionChoice::Closed(_) => {
                self.ui.notice = Some(Feedback::History("Permissions unchanged".into()));
                return Ok(FolderPermissions::Handled);
            }
            permissions::PermissionChoice::Rejected(_, error) => {
                self.ui.notice = Some(Feedback::History(format!(
                    "Error: {error}; existing permissions unchanged"
                )));
                return Ok(FolderPermissions::Handled);
            }
            permissions::PermissionChoice::Exit => {
                return Ok(FolderPermissions::Close);
            }
        };

        Ok(FolderPermissions::Selected(access))
    }

    fn interrupt(&mut self) -> IdleAction {
        if self.terminal.clipboard_loading() {
            self.terminal.cancel_clipboard_paste();
            self.ui.notice = Some(Feedback::Transient(
                "Clipboard read cancelled; draft retained".into(),
            ));
            self.ui.empty_interrupt = None;
        } else if !self.composer.is_empty() {
            self.composer.clear();
            self.terminal.discard_draft_input();
            self.ui.empty_interrupt = None;
        } else if self
            .ui
            .empty_interrupt
            .is_some_and(|time| time.elapsed() <= EMPTY_INTERRUPT_WINDOW)
        {
            return IdleAction::Exit;
        } else {
            self.ui.empty_interrupt = Some(Instant::now());
            self.ui.notice = Some(Feedback::History(self.terminal.empty_interrupt_hint()));
        }
        IdleAction::Continue
    }

    async fn finish(self) -> Result<(), String> {
        let Self {
            terminal,
            engine,
            view,
            session_id,
            ..
        } = self;
        drop(terminal);
        let closed = close_engine(engine).await;
        if view.created_sequence != 0 {
            writeln!(std::io::stderr(), "Session: {session_id}")
                .map_err(|_| "output failed".to_owned())?;
        } else {
            writeln!(std::io::stderr(), "No conversation saved.")
                .map_err(|_| "output failed".to_owned())?;
        }
        closed
    }

    fn refresh_composer(&mut self) {
        let supported = permissions::supports_tools(self.view.defaults.provider.as_deref());
        if supported != self.ui.skills_supported {
            refresh_runtime_skills(
                &self.admission,
                &mut self.composer,
                &mut self.ui.notice,
                supported,
            );
            self.ui.skills_supported = supported;
        }
        permissions::refresh_mode(
            &mut self.composer,
            self.view.defaults.provider.as_deref(),
            self.admission.tools,
            self.admission.workspace_permissions.as_ref(),
        );
        if let Some(error) =
            permissions::refresh_files(&self.admission.workspace, &mut self.composer)
        {
            append_notice(&mut self.ui.notice, &error);
        }
        self.composer.set_completion_selection(
            self.view.defaults.provider.as_deref(),
            self.view.defaults.model.as_deref(),
            self.view.defaults.account_id,
        );
    }
}
