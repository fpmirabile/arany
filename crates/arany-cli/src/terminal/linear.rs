use crate::presentation::{
    AgentInspectorModel, MAX_AGENT_ROWS, escape_terminal, image_message_lines,
    linear_session_lines, safe_truncate, user_message_lines,
};
#[cfg(test)]
use arany::ModelEntry;
use arany::{SessionDefaults, SessionId, SessionListItem, SessionView};
use std::io::{self, Write};

use super::CommandAvailability;
use super::commands::{help_entry, help_len};

pub(super) struct LinearState {
    mode_label: &'static str,
    announced: bool,
    last_view: Option<(SessionId, u64)>,
    tools_written: Option<(arany::RunId, usize)>,
    last_unsaved: Option<(SessionDefaults, String)>,
    pub(super) prompt_needed: bool,
    pub(super) last_picker_page: Option<usize>,
    pub(super) last_model_page: Option<(usize, super::ModelCatalogState)>,
    last_setup: Option<(u8, String, String)>,
    last_approval: Option<uuid::Uuid>,
    unarmed_approval: Option<uuid::Uuid>,
    permission_mode: Option<arany::ApprovalMode>,
    pub(super) line_open: bool,
    pub(super) newline_pending: bool,
    unarmed: bool,
    held_prompt: Option<&'static str>,
}

// A Run's unescaped detail parts: provisional or incomplete answer, public commentary, limit cue, Tools and verdicts.
pub(super) fn transcript_details(run: &arany::RunView) -> Vec<String> {
    let mut parts = Vec::new();
    if run.assistant_message.is_none()
        && let Some(answer) = crate::presentation::display_answer(run)
    {
        // Text of a Run still streaming is provisional; only a settled Run's text is incomplete.
        let label = if matches!(
            run.status,
            arany::RunStatus::Pending | arany::RunStatus::Active
        ) {
            "Arany · provisional:"
        } else {
            "Arany · incomplete:"
        };
        parts.push(format!("{label}\n{answer}"));
    }
    parts.extend(
        crate::presentation::display_commentary(run)
            .into_iter()
            .map(|(_, _, text)| format!("Arany · public commentary:\n{text}")),
    );
    if run
        .display
        .iter()
        .any(|chunk| chunk.category == arany::DisplayCategory::Limit)
    {
        parts.push(arany::DISPLAY_LIMIT_TEXT.to_owned());
    }
    parts.extend(run.tools.iter().map(|tool| {
        crate::presentation::plain_tool_summary(&crate::presentation::tool_detail_text(tool))
    }));
    parts.extend(crate::presentation::supervisor_lines(run));
    parts
}

impl LinearState {
    // An unarmed selector prints its rows and holds the prompt until input is admitted.
    pub(super) fn disarm(&mut self) {
        self.unarmed = true;
        self.held_prompt = None;
    }

    pub(super) fn arm(&mut self, writer: &mut impl Write) -> io::Result<()> {
        self.unarmed = false;
        if let Some(prompt) = self.held_prompt.take() {
            writeln!(writer, "{prompt}")?;
            self.line_open = true;
            self.prompt_needed = false;
        }
        writer.flush()
    }

    fn write_prompt(&mut self, writer: &mut impl Write, prompt: &'static str) -> io::Result<()> {
        if self.unarmed {
            self.held_prompt = Some(prompt);
            return Ok(());
        }
        writeln!(writer, "{prompt}")?;
        self.line_open = true;
        Ok(())
    }

    pub(super) fn draw_tool_approval(
        &mut self,
        id: uuid::Uuid,
        lines: &[String],
        choices: &[crate::presentation::ToolApprovalChoice],
        armed: bool,
    ) -> io::Result<()> {
        self.write_tool_approval(&mut io::stderr(), id, lines, choices, armed)
    }

    fn write_tool_approval(
        &mut self,
        writer: &mut impl Write,
        id: uuid::Uuid,
        lines: &[String],
        choices: &[crate::presentation::ToolApprovalChoice],
        armed: bool,
    ) -> io::Result<()> {
        let mut lines = lines.to_vec();
        lines.push("Do you want to proceed?".into());
        lines.extend(
            choices
                .iter()
                .enumerate()
                .map(|(index, choice)| format!("{}. {}", index + 1, choice.label())),
        );
        if self.unarmed_approval == Some(id) {
            lines.clear();
        }
        if !armed {
            if self.unarmed_approval == Some(id) || self.last_approval == Some(id) {
                return Ok(());
            }
            if self.newline_pending || self.line_open {
                writeln!(writer)?;
            }
            for line in &lines {
                writeln!(writer, "{line}")?;
            }
            self.newline_pending = false;
            self.line_open = false;
            self.unarmed_approval = Some(id);
            return writer.flush();
        }
        self.write_permission_prompt(
            writer,
            id,
            &lines,
            &format!("Select 1-{}, yes or no; empty Enter denies:", choices.len()),
        )
    }

    pub(super) fn draw_permission_prompt(
        &mut self,
        id: uuid::Uuid,
        lines: &[String],
        prompt: &str,
    ) -> io::Result<()> {
        self.write_permission_prompt(&mut io::stderr(), id, lines, prompt)
    }

    fn write_permission_prompt(
        &mut self,
        writer: &mut impl Write,
        id: uuid::Uuid,
        lines: &[String],
        prompt: &str,
    ) -> io::Result<()> {
        if self.last_approval == Some(id) {
            return Ok(());
        }
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
        }
        for line in lines {
            writeln!(writer, "{line}")?;
        }
        writeln!(writer, "{prompt}")?;
        self.newline_pending = false;
        self.line_open = true;
        self.prompt_needed = false;
        self.last_approval = Some(id);
        writer.flush()
    }
    pub(super) fn new(mode_label: &'static str) -> Self {
        Self {
            mode_label,
            announced: false,
            last_view: None,
            tools_written: None,
            last_unsaved: None,
            prompt_needed: true,
            last_picker_page: None,
            last_model_page: None,
            last_setup: None,
            last_approval: None,
            unarmed_approval: None,
            permission_mode: None,
            line_open: false,
            newline_pending: false,
            unarmed: false,
            held_prompt: None,
        }
    }

    pub(super) fn draw(
        &mut self,
        view: &SessionView,
        notice: Option<&str>,
        mode: Option<arany::ApprovalMode>,
    ) -> io::Result<()> {
        if self.permission_mode != mode {
            self.permission_mode = mode;
            self.last_view = None;
        }
        self.write_view(&mut io::stderr(), view, notice)
    }

    pub(super) fn write_transcript(
        &mut self,
        writer: &mut impl Write,
        view: &SessionView,
    ) -> io::Result<()> {
        const HEADING: &str = "Transcript details:";
        const LIMIT_NOTICE: &str =
            "Transcript detail limit reached; use show --output jsonl for the complete journal.";
        let mut remaining = super::history::MAX_CLASSIC_HISTORY_BYTES
            - (HEADING.len() + 1)
            - (LIMIT_NOTICE.len() + 1);
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            remaining -= 1;
            self.newline_pending = false;
            self.line_open = false;
        }
        writeln!(writer, "{HEADING}")?;
        for run in view
            .runs
            .iter()
            .rev()
            .take(super::history::MAX_CLASSIC_HISTORY_RUNS)
            .rev()
        {
            let parts = transcript_details(run);
            for text in parts {
                let text = crate::presentation::escape_terminal(&text);
                if text.len().saturating_add(1) > remaining {
                    writeln!(writer, "{LIMIT_NOTICE}")?;
                    return Ok(());
                }
                remaining = remaining.saturating_sub(text.len() + 1);
                writeln!(writer, "{text}")?;
            }
        }
        if view.runs.len() > super::history::MAX_CLASSIC_HISTORY_RUNS {
            writeln!(writer, "{LIMIT_NOTICE}")?;
        }
        writer.flush()
    }

    pub(super) fn write_usage(
        &mut self,
        writer: &mut impl Write,
        pages: &[String],
    ) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
        }
        self.newline_pending = false;
        self.line_open = false;
        for (index, page) in pages.iter().enumerate() {
            writeln!(writer, "Usage page {} of {}", index + 1, pages.len())?;
            write!(writer, "{page}")?;
        }
        self.prompt_needed = true;
        Ok(())
    }

    pub(super) fn write_help(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        self.last_setup = None;
        writeln!(writer, "Help: local commands; arguments are literal")?;
        writeln!(
            writer,
            "During a Run: Enter offers a steering note; /goal pause pauses at the next step; Ctrl+C cancels; Session settings stay pinned"
        )?;
        for index in 0..help_len() {
            let entry = help_entry(index).expect("compiled command entry");
            let availability = match entry.command.availability(true) {
                CommandAvailability::Available => "available",
                CommandAvailability::ViewOnly => "view-only",
                CommandAvailability::Locked => "locked",
            };
            writeln!(
                writer,
                "Command: /{}{}; {}; during Run: {availability}",
                entry.name,
                entry
                    .argument
                    .map_or_else(String::new, |argument| format!(" {argument}")),
                entry.description,
            )?;
        }
        self.prompt_needed = true;
        writer.flush()
    }

    pub(super) fn write_clipboard_draft(
        &mut self,
        writer: &mut impl Write,
        composer: &super::Composer,
    ) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        writeln!(writer, "Draft (not sent):")?;
        for line in composer.text().split('\n') {
            writeln!(writer, "  {}", escape_terminal(line))?;
        }
        for line in image_message_lines(composer.images(), 80) {
            writeln!(writer, "{line}")?;
        }
        writeln!(
            writer,
            "Draft: {} characters; Enter submits when idle; Ctrl+C clears when idle",
            composer.character_count()
        )?;
        self.prompt_needed = true;
        writer.flush()
    }

    pub(super) fn write_user_message(
        &mut self,
        writer: &mut impl Write,
        objective: &str,
    ) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        for line in user_message_lines(objective, 80) {
            writeln!(writer, "{line}")?;
        }
        writer.flush()
    }

    pub(super) fn written_tools(&self) -> Option<(arany::RunId, usize)> {
        self.tools_written
    }

    pub(super) fn keep_written_tools(&mut self, written: Option<(arany::RunId, usize)>) {
        self.tools_written = written;
    }

    pub(super) fn write_tools(
        &mut self,
        writer: &mut impl Write,
        run: &arany::RunView,
    ) -> io::Result<()> {
        let written = self
            .tools_written
            .filter(|(id, _)| *id == run.id)
            .map_or(0, |(_, count)| count);
        for (range, summary) in crate::presentation::settled_tool_groups(run) {
            if range.start >= written {
                if self.newline_pending || self.line_open {
                    writeln!(writer)?;
                    self.newline_pending = false;
                    self.line_open = false;
                }
                writeln!(
                    writer,
                    "{}",
                    crate::presentation::plain_tool_summary(&summary)
                )?;
                self.tools_written = Some((run.id, range.end));
            }
        }
        writer.flush()
    }

    pub(super) fn draw_setup(
        &mut self,
        step: u8,
        title: &str,
        instruction: &str,
        notice: Option<&str>,
    ) -> io::Result<()> {
        self.write_setup(&mut io::stderr(), step, title, instruction, notice)
    }

    pub(super) fn draw_setup_warning(
        &mut self,
        title: &str,
        warning: &str,
        notice: Option<&str>,
    ) -> io::Result<()> {
        self.write_setup_with_limit(&mut io::stderr(), 1, title, warning, notice, (1024, false))
    }

    pub(super) fn draw_setup_choices(
        &mut self,
        step: u8,
        title: &str,
        instruction: &str,
        notice: Option<&str>,
    ) -> io::Result<()> {
        self.write_setup_with_limit(
            &mut io::stderr(),
            step,
            title,
            instruction,
            notice,
            (240, true),
        )
    }

    fn write_setup(
        &mut self,
        writer: &mut impl Write,
        step: u8,
        title: &str,
        instruction: &str,
        notice: Option<&str>,
    ) -> io::Result<()> {
        let instruction_limit = if step == 3 { 512 } else { 240 };
        self.write_setup_with_limit(
            writer,
            step,
            title,
            instruction,
            notice,
            (instruction_limit, matches!(step, 1 | 2 | 3 | 5)),
        )
    }

    fn write_setup_with_limit(
        &mut self,
        writer: &mut impl Write,
        step: u8,
        title: &str,
        instruction: &str,
        notice: Option<&str>,
        (instruction_limit, takes_input): (usize, bool),
    ) -> io::Result<()> {
        let unchanged = self.last_setup.as_ref().is_some_and(|previous| {
            previous.0 == step && previous.1 == title && previous.2 == instruction
        });
        if unchanged && notice.is_none() {
            return Ok(());
        }
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        if !unchanged {
            writeln!(
                writer,
                "{}: {}",
                if step == 0 { "Settings" } else { "Setup" },
                safe_truncate(title, 160)
            )?;
            let displayed_instruction = if instruction_limit > 240 {
                if instruction.len() > instruction_limit
                    || !instruction
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() || matches!(byte, b' ' | b'\n'))
                {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                instruction.to_owned()
            } else {
                safe_truncate(instruction, instruction_limit)
            };
            writeln!(writer, "Choose: {displayed_instruction}")?;
            self.last_setup = Some((step, title.to_owned(), instruction.to_owned()));
        }
        if let Some(notice) = notice {
            writeln!(writer, "Notice: {}", safe_truncate(notice, 240))?;
        }
        if self.prompt_needed && takes_input {
            writeln!(writer, "Input:")?;
            self.prompt_needed = false;
            self.line_open = true;
        }
        writer.flush()
    }

    fn write_view(
        &mut self,
        writer: &mut impl Write,
        view: &SessionView,
        notice: Option<&str>,
    ) -> io::Result<()> {
        self.last_setup = None;
        if self.newline_pending {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        if !self.announced {
            writeln!(writer, "Presentation: {}", self.mode_label)?;
            #[cfg(unix)]
            writeln!(
                writer,
                "Input: Enter submits; backslash then Enter adds a newline; empty Ctrl+D exits",
            )?;
            writeln!(
                writer,
                "Modified newline shortcuts apply only to visual sessions; use backslash then Enter here"
            )?;
            writeln!(
                writer,
                "Paste: /paste safely inserts multiline clipboard text; unframed paste can submit lines and consumes trailing backslashes"
            )?;
            self.announced = true;
        }
        let identity = (view.id, view.last_sequence);
        let unsaved = (view.created_sequence == 0)
            .then(|| (view.defaults.clone(), view.conversation_title()));
        if self.last_view != Some(identity) || self.last_unsaved != unsaved {
            for line in linear_session_lines(view) {
                if line.starts_with("Permissions:")
                    && let Some(mode) = self.permission_mode
                {
                    writeln!(
                        writer,
                        "Permissions: {} for this folder; commands run offline in an isolated copy",
                        mode.label()
                    )?;
                } else {
                    writeln!(writer, "{line}")?;
                }
            }
            if let Some(run) = view.runs.last() {
                self.write_tools(writer, run)?;
            }
            self.last_view = Some(identity);
            self.last_unsaved = unsaved;
        }
        if let Some(notice) = notice {
            writeln!(writer, "Notice: {}", safe_truncate(notice, 240))?;
        }
        if self.prompt_needed {
            writeln!(writer, "Input:")?;
            self.prompt_needed = false;
            self.line_open = true;
        }
        writer.flush()
    }

    pub(super) fn draw_settings(
        &mut self,
        rows: &[String],
        notice: Option<&str>,
    ) -> io::Result<()> {
        self.write_settings(&mut io::stderr(), rows, notice)
    }

    fn write_settings(
        &mut self,
        writer: &mut impl Write,
        rows: &[String],
        notice: Option<&str>,
    ) -> io::Result<()> {
        let listing = rows.join("\n");
        let unchanged = self
            .last_setup
            .as_ref()
            .is_some_and(|(step, title, previous)| {
                *step == 0 && title == "Settings" && *previous == listing
            });
        if unchanged && notice.is_none() {
            return Ok(());
        }
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        if !unchanged {
            writeln!(
                writer,
                "Settings: type NUMBER VALUE to change a row; empty Enter saves; q cancels"
            )?;
            for (index, row) in rows.iter().enumerate() {
                writeln!(writer, "Choice {}: {}", index + 1, safe_truncate(row, 240))?;
            }
            self.last_setup = Some((0, "Settings".into(), listing));
        }
        if let Some(notice) = notice {
            writeln!(writer, "Notice: {}", safe_truncate(notice, 240))?;
        }
        if self.prompt_needed {
            self.write_prompt(writer, "Input:")?;
            self.prompt_needed = false;
        }
        writer.flush()
    }

    pub(super) fn draw_sessions(
        &mut self,
        items: &[SessionListItem],
        start: usize,
    ) -> io::Result<()> {
        self.write_sessions(&mut io::stderr(), items, start)
    }

    fn write_sessions(
        &mut self,
        writer: &mut impl Write,
        items: &[SessionListItem],
        start: usize,
    ) -> io::Result<()> {
        if self.last_picker_page == Some(start) {
            return Ok(());
        }
        writeln!(writer, "Resume: {} in this Workspace", items.len())?;
        for (index, item) in items.iter().skip(start).take(10).enumerate() {
            writeln!(
                writer,
                "Choice {}: {}",
                index + 1,
                safe_truncate(&item.title, 240)
            )?;
            writeln!(
                writer,
                "Created: {} UTC; last activity: {} UTC",
                item.created_at, item.last_activity_at
            )?;
            writeln!(
                writer,
                "Access: {}; model: {}",
                safe_truncate(
                    crate::presentation::session_access_label(&item.defaults),
                    240
                ),
                safe_truncate(
                    item.defaults.model.as_deref().unwrap_or("Default model"),
                    240
                )
            )?;
        }
        self.write_prompt(
            writer,
            "Choose number, exact ID, n next, p previous, or q close:",
        )?;
        self.last_picker_page = Some(start);
        writer.flush()
    }

    pub(super) fn write_picker_invalid(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.newline_pending {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        writeln!(writer, "Notice: invalid Session selection")?;
        writer.flush()
    }

    pub(super) fn draw_models(
        &mut self,
        profile: &str,
        picker: &super::ModelPicker<'_>,
    ) -> io::Result<()> {
        self.write_models(&mut io::stderr(), profile, picker)
    }

    fn write_models(
        &mut self,
        writer: &mut impl Write,
        profile: &str,
        picker: &super::ModelPicker<'_>,
    ) -> io::Result<()> {
        self.last_setup = None;
        let start = picker.selected;
        if self.last_model_page == Some((start, picker.catalog_state)) && !picker.invalid {
            return Ok(());
        }
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        if picker.invalid {
            writeln!(writer, "Notice: invalid model or effort choice")?;
        }
        writeln!(
            writer,
            "Models: {}; {} available",
            safe_truncate(profile, 128),
            picker.items.len()
        )?;
        if picker.catalog_state != super::ModelCatalogState::Current {
            let status = if picker.catalog_state == super::ModelCatalogState::Updated {
                "refreshed; reopen /model to see updates; these choice numbers are unchanged"
            } else {
                picker.catalog_state.label()
            };
            writeln!(writer, "Catalog: {status}")?;
        }
        for (index, item) in picker.items.iter().skip(start).take(10).enumerate() {
            let status = super::model_catalog_status(item, picker.exact_custom);
            let effort = picker
                .efforts
                .get(start + index)
                .copied()
                .flatten()
                .map_or("default", arany::Effort::as_str);
            writeln!(
                writer,
                "Choice {}: {}; selected effort {}; {}",
                index + 1,
                safe_truncate(&item.id, 128),
                effort,
                status
            )?;
        }
        self.write_prompt(
            writer,
            "Choose number [effort|default], n next, p previous, or q close:",
        )?;
        self.last_model_page = Some((start, picker.catalog_state));
        writer.flush()
    }

    // Append-only agent chat output; the prompt waits for the armed frame like a selector.
    pub(super) fn write_agent_chat(
        &mut self,
        writer: &mut impl Write,
        lines: &[String],
        notice: Option<&str>,
        prompt: bool,
    ) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        for line in lines {
            writeln!(writer, "{}", escape_terminal(line))?;
        }
        if let Some(notice) = notice {
            writeln!(writer, "Notice: {}", safe_truncate(notice, 240))?;
        }
        if prompt {
            self.write_prompt(
                writer,
                "Agent chat: number opens, n next, p previous, q returns to primary:",
            )?;
        }
        writer.flush()
    }

    // Choice numbers continue from `start`, so they bind to the rows printed here.
    pub(super) fn write_agents(
        &mut self,
        writer: &mut impl Write,
        model: &AgentInspectorModel,
        start: usize,
        notice: Option<&str>,
    ) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        if let Some(notice) = notice {
            writeln!(writer, "Notice: {notice}")?;
        }
        for line in &model.lines[..model.header] {
            writeln!(writer, "{line}")?;
        }
        writeln!(writer, "{}", model.search)?;
        let shown = model.choices.iter().skip(start).take(MAX_AGENT_ROWS);
        if shown.len() > 0 {
            writeln!(
                writer,
                "Choices {}-{} of {}:",
                start + 1,
                start + shown.len(),
                model.choices.len()
            )?;
        }
        for (index, choice) in shown.enumerate() {
            let focus = if model.selected == Some(start + index) {
                " (focused)"
            } else {
                ""
            };
            writeln!(writer, "{}. {choice}{focus}", start + index + 1)?;
        }
        for line in &model.lines[model.header..] {
            writeln!(writer, "{line}")?;
        }
        self.write_prompt(
            writer,
            "Agent choice: n next, p previous, number select, o open, /TEXT search, q close:",
        )?;
        writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::AgentQuery;

    #[test]
    fn tool_approval_linear_corpus_numbers_only_supported_choices() {
        use crate::presentation::ToolApprovalChoice::{
            AutoEdits, ExactCommand, GitPrefix, No, Yes,
        };
        for (choices, expected) in [
            (
                vec![
                    Yes,
                    GitPrefix(arany::permissions::TypedGitPrefix::Commit),
                    No,
                ],
                "1. Yes\n2. Yes, and allow git commit in this folder from the next Run\n3. No\nSelect 1-3, yes or no; empty Enter denies:\n",
            ),
            (
                vec![Yes, No],
                "1. Yes\n2. No\nSelect 1-2, yes or no; empty Enter denies:\n",
            ),
            (
                vec![Yes, AutoEdits, No],
                "1. Yes\n2. Yes, and don't ask again for: file edits\n3. No\nSelect 1-3, yes or no; empty Enter denies:\n",
            ),
            (
                vec![Yes, ExactCommand, No],
                "1. Yes\n2. Yes, and don't ask again for: this exact command in this project (unguarded)\n3. No\nSelect 1-3, yes or no; empty Enter denies:\n",
            ),
        ] {
            let mut state = LinearState::new("linear");
            let mut output = Vec::new();
            let id = uuid::Uuid::now_v7();
            state
                .write_tool_approval(
                    &mut output,
                    id,
                    &["Tool approval".into(), "Read src/file".into()],
                    &choices,
                    false,
                )
                .unwrap();
            let unarmed = String::from_utf8(output.clone()).unwrap();
            assert!(unarmed.ends_with("No\n") && !unarmed.contains("Select"));
            state
                .write_tool_approval(&mut output, id, &[], &choices, true)
                .unwrap();
            assert_eq!(
                String::from_utf8(output.clone()).unwrap(),
                format!("Tool approval\nRead src/file\nDo you want to proceed?\n{expected}")
            );
            state
                .write_tool_approval(&mut output, id, &[], &choices, true)
                .unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!("Tool approval\nRead src/file\nDo you want to proceed?\n{expected}")
            );
        }
    }

    #[test]
    fn linear_agent_picker_numbers_its_window_and_marks_focus_in_words() {
        let view = crate::presentation::team_view();
        let query = AgentQuery {
            filter: "child".into(),
            ..AgentQuery::default()
        };
        let model = AgentInspectorModel::from_session(&view, &query, 120, false);
        let mut state = LinearState::new("screen-reader");
        let mut output = Vec::new();
        state
            .write_agents(&mut output, &model, 1, Some("invalid agent selection"))
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[..8],
            [
                "Notice: invalid agent selection",
                "Last Run: 3 agents; children: 2",
                "History: 3 agents in 1 recent Runs; 0 older Runs",
                "Next Run: single",
                "Set next Run: /agents single|auto N|team N",
                "Search: child · 2 of 3 agents",
                "Choices 2-2 of 2:",
                "2. Run 1: child 2; finished · Inspect\\u{001b}[2J\\u{000a}You: forged heading",
            ]
        );
        assert_eq!(lines[8], "Agent 1/2: child 1; failed");
        assert_eq!(
            lines.last(),
            Some(
                &"Agent choice: n next, p previous, number select, o open, /TEXT search, q close:"
            )
        );
        assert!(!text.contains(['\u{1b}', '\r']));
        let none = AgentInspectorModel::from_session(
            &view,
            &AgentQuery {
                filter: "zz".into(),
                ..AgentQuery::default()
            },
            120,
            false,
        );
        let mut output = Vec::new();
        state.write_agents(&mut output, &none, 0, None).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains(
                "\nSearch: zz · no matching agents\nNo matching agents; edit the search\n"
            )
        );
        assert!(!text.contains("Choices") && !text.contains("(focused)"));
    }

    #[test]
    fn linear_agent_chat_lists_numbered_agents_and_prints_an_inert_child_transcript() {
        use crate::presentation::{AgentTranscript, Appended, agent_targets, team_view};
        let view = team_view();
        let run = &view.runs[0];
        let (_, targets) = agent_targets(&view).unwrap();
        let mut state = LinearState::new("screen-reader");
        let mut output = Vec::new();
        let mut list = vec!["Agent chats in this Run:".to_owned()];
        list.extend(
            targets
                .iter()
                .enumerate()
                .map(|(index, target)| format!("{}. {}", index + 1, target.line)),
        );
        state.disarm();
        state
            .write_agent_chat(&mut output, &list, None, true)
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&output).contains("Agent chat:"),
            "an unarmed view holds its prompt"
        );
        state.arm(&mut output).unwrap();
        let transcript = AgentTranscript::from_session(&view, run.id, run.agents[2].id).unwrap();
        state
            .write_agent_chat(
                &mut output,
                &transcript.linear_lines(&mut Appended::default()),
                None,
                true,
            )
            .unwrap();
        state
            .write_agent_chat(
                &mut output,
                &[],
                Some("Invalid agent choice; type a number, n, p or q"),
                true,
            )
            .unwrap();
        state
            .write_agent_chat(&mut output, &["Back to primary".into()], None, false)
            .unwrap();
        state
            .write_agent_chat(
                &mut output,
                &[],
                Some("Copy unavailable: \u{1b}]52;c;eA==\u{7}\u{1b}[2J\u{9b}31m"),
                false,
            )
            .unwrap();
        let prompt = "Agent chat: number opens, n next, p previous, q returns to primary:";
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "Agent chats in this Run:\n\
                 1. primary · finished\n\
                 2. child 1 · failed · Child assignment\n\
                 3. child 2 · finished · Inspect\\u{{001b}}[2J\\u{{000a}}You: forged heading\n\
                 {prompt}\n\
                 \n\
                 Viewing child 2 (read-only)\n\
                 Assignment:\n  \
                 Inspect\\u{{001b}}[2J\n  \
                 You: forged heading\n\
                 Read(src/a.rs)\n\
                 Result: 2 lines\n\
                 Read(src/b\\u{{001b}}]52;c;x\\u{{0007}}.rs)\n\
                 Result: uncertain: {{\"text\":\"a\\nb\\n\"}}\n\
                 Agent result:\n  \
                 Found\\u{{202e}} it\n\
                 child 2 finished\n\
                 {prompt}\n\
                 \n\
                 Notice: Invalid agent choice; type a number, n, p or q\n\
                 {prompt}\n\
                 \n\
                 Back to primary\n\
                 Notice: Copy unavailable: \\u{{001b}}]52;c;eA==\\u{{0007}}\\u{{001b}}[2J\\u{{009b}}31m\n"
            )
        );
    }

    #[test]
    fn linear_settings_number_rows_hold_the_prompt_until_armed_and_reprint_after_a_change() {
        let mut state = LinearState::new("screen-reader");
        let mut output = Vec::new();
        let rows = vec![
            "Slash menu: tabs (tabs|combined)".to_owned(),
            "Goal tokens: unlimited (number or unlimited)".to_owned(),
        ];
        state.disarm();
        state.write_settings(&mut output, &rows, None).unwrap();
        assert!(
            !String::from_utf8_lossy(&output).contains("Input:"),
            "an unarmed selector holds its prompt"
        );
        state.arm(&mut output).unwrap();
        state.write_settings(&mut output, &rows, None).unwrap();
        state.prompt_needed = true;
        state.line_open = false;
        state
            .write_settings(
                &mut output,
                &rows,
                Some("Type NUMBER VALUE from the list; q cancels"),
            )
            .unwrap();
        state.prompt_needed = true;
        state.line_open = false;
        let changed = vec![
            "Slash menu: combined (tabs|combined)".to_owned(),
            "Goal tokens: unlimited (number or unlimited)".to_owned(),
        ];
        state.write_settings(&mut output, &changed, None).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Settings: type NUMBER VALUE to change a row; empty Enter saves; q cancels\n\
             Choice 1: Slash menu: tabs (tabs|combined)\n\
             Choice 2: Goal tokens: unlimited (number or unlimited)\n\
             Input:\n\
             Notice: Type NUMBER VALUE from the list; q cancels\n\
             Input:\n\
             Settings: type NUMBER VALUE to change a row; empty Enter saves; q cancels\n\
             Choice 1: Slash menu: combined (tabs|combined)\n\
             Choice 2: Goal tokens: unlimited (number or unlimited)\n\
             Input:\n"
        );
    }

    #[test]
    fn rewind_linear_parity_preserves_every_state_choice_and_hostile_path_without_clipping() {
        let mut state = LinearState::new("screen-reader");
        let mut output = Vec::new();
        let rows = vec![
            "Rewind: preview".into(),
            "1. Run 1 · prompt · no code changes since".into(),
            "2. Before rewind".into(),
            "Restore code and conversation".into(),
            "Restore conversation".into(),
            "Restore code".into(),
            "Never mind".into(),
            "restore +1 -2".into(),
            "delete +0 -2".into(),
            "recreate +3 -0".into(),
            "drifted".into(),
            "unavailable".into(),
            "unsafe".into(),
            "outside grant".into(),
            "kept".into(),
            "Code restore blocked: unknown or open outcome; inspect files".into(),
            "Not rewindable: MCP external effect".into(),
            crate::presentation::rewind::KEEP.into(),
            crate::presentation::rewind::OVERWRITE.into(),
            "Compare".into(),
            "Code restored; conversation unchanged".into(),
            "Rewind outcome unknown: inspect files before continuing".into(),
            "Older checkpoints pruned".into(),
            escape_terminal(&format!(
                "src/{}\u{1b}]52;bad\u{7}\r\u{202e}",
                "long/".repeat(100)
            )),
        ];
        let id = uuid::Uuid::now_v7();
        state
            .write_permission_prompt(
                &mut output,
                id,
                &rows,
                "Type a number or visible choice name; empty Enter cancels:",
            )
            .unwrap();
        let length = output.len();
        state
            .write_permission_prompt(&mut output, id, &rows, "prompt")
            .unwrap();
        assert_eq!(output.len(), length);
        let text = String::from_utf8(output).unwrap();
        for row in rows {
            assert!(text.contains(&row));
        }
        assert!(
            !text
                .chars()
                .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}')
        );
    }
    use arany::{
        AgentRole, AgentRunId, AgentStatus, AgentView, RunId, RunStatus, RunView, SessionDefaults,
    };

    #[test]
    fn committed_user_message_is_append_only_and_separated_from_prompt() {
        let mut linear = LinearState::new("screen-reader");
        linear.line_open = true;
        let mut output = Vec::new();
        linear
            .write_user_message(&mut output, "hello\u{1b}[2J\nworld")
            .expect("write committed user message");
        assert_eq!(output, b"\nYou:\n  hello\\u{001b}[2J\n  world\n");
        assert!(!output.contains(&0x1b));
        assert!(!output.contains(&b'\r'));
        assert!(!linear.line_open);

        let mut composer = super::super::Composer::default();
        composer.insert_paste("alpha\tbeta\n🙂").expect("draft");
        linear.line_open = true;
        let mut pasted = Vec::new();
        linear
            .write_clipboard_draft(&mut pasted, &composer)
            .expect("pasted draft preview");
        assert_eq!(
            pasted,
            "\nDraft (not sent):\n  alpha\\u{0009}beta\n  🙂\nDraft: 12 characters; Enter submits when idle; Ctrl+C clears when idle\n".as_bytes()
        );
        assert_eq!(composer.text(), "alpha\tbeta\n🙂");
        assert!(!pasted.contains(&0x1b));
        assert!(!pasted.contains(&b'\r'));
        assert!(!linear.line_open);
        assert!(linear.prompt_needed);
        composer
            .attach_image(arany::test_support::test_image())
            .unwrap();
        let mut with_image = Vec::new();
        linear
            .write_clipboard_draft(&mut with_image, &composer)
            .unwrap();
        assert_eq!(with_image, "Draft (not sent):\n  alpha\\u{0009}beta\n  🙂\n  Image 1: PNG 1x1, 69 B\nDraft: 12 characters; Enter submits when idle; Ctrl+C clears when idle\n".as_bytes());
        composer.clear();
        composer.insert_paste("e\u{301}👩‍💻").unwrap();
        let mut graphemes = Vec::new();
        linear
            .write_clipboard_draft(&mut graphemes, &composer)
            .unwrap();
        assert_eq!(graphemes, "Draft (not sent):\n  e\u{301}👩‍💻\nDraft: 2 characters; Enter submits when idle; Ctrl+C clears when idle\n".as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn linear_presentation_is_labeled_append_only_and_control_free() {
        let mut view = SessionView {
            usage: Default::default(),
            rewinds: Default::default(),
            id: SessionId::new(),
            title: "bad\u{1b}[31m\nSession: forged".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            runs: Vec::new(),
            compactions: Vec::new(),
            goals: Vec::new(),
        };
        let mut linear = LinearState::new("screen-reader");
        let mut output = Vec::new();
        linear
            .write_view(&mut output, &view, Some("ready\u{1b}[2J"))
            .expect("first linear frame");
        linear
            .write_view(&mut output, &view, None)
            .expect("unchanged facts do not repeat");
        assert_eq!(
            String::from_utf8(output.clone()).expect("UTF-8"),
            format!(
                "Presentation: screen-reader\n\
                 Input: Enter submits; backslash then Enter adds a newline; empty Ctrl+D exits\n\
                 Modified newline shortcuts apply only to visual sessions; use backslash then Enter here\n\
                 Paste: /paste safely inserts multiline clipboard text; unframed paste can submit lines and consumes trailing backslashes\n\
                 Session: {}\n\
                 Title: bad\\u{{001b}}[31m\\u{{000a}}Session: forged\n\
                 State: ready\n\
                 Provider: unset\n\
                 Model: unset\n\
                 Setup: type /setup to choose an account\n\
                 Permissions: read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox\n\
                 Collaboration: auto; max children: 3\n\
                 Notice: ready\\u{{001b}}[2J\n\
                 Input:\n",
                view.id
            )
        );
        assert!(!output.contains(&b'\r'));
        assert!(!output.contains(&0x1b));

        linear.newline_pending = true;
        linear.prompt_needed = true;
        let mut interrupted = Vec::new();
        linear
            .write_view(
                &mut interrupted,
                &view,
                Some("Ctrl+Shift+C copies. Ctrl+C again exits."),
            )
            .expect("interrupt notice");
        assert_eq!(
            interrupted,
            b"\nNotice: Ctrl+Shift+C copies. Ctrl+C again exits.\nInput:\n"
        );

        let run_id = RunId::new();
        view.last_sequence = 2;
        view.runs.push(RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: run_id,
            objective: "Question".into(),
            images: Vec::new(),
            config: None,
            agents: vec![AgentView {
                profile: None,
                assignment: None,
                definition: None,
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                steering_notes: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 2,
            finished_sequence: None,
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
        });
        let mut progress = Vec::new();
        linear
            .write_view(&mut progress, &view, Some("Run is active; Ctrl+C cancels"))
            .expect("committed progress");
        assert_eq!(
            String::from_utf8(progress.clone()).expect("UTF-8 progress"),
            format!(
                "Session: {}\n\
                 Title: bad\\u{{001b}}[31m\\u{{000a}}Session: forged\n\
                 State: working\n\
                 Provider: unset\n\
                 Model: unset\n\
                 Permissions: read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox\n\
                 Collaboration: auto; max children: 3\n\
                 Run: {run_id}\n\
                 Agent: primary; state: active\n\
                 Notice: Run is active; Ctrl+C cancels\n",
                view.id
            )
        );
        assert!(!progress.contains(&b'\r'));
        assert!(!progress.contains(&0x1b));

        linear.line_open = true;
        let mut help = Vec::new();
        linear.write_help(&mut help).expect("complete linear help");
        assert_eq!(
            help,
            concat!(
                "\nHelp: local commands; arguments are literal\n",
                "During a Run: Enter offers a steering note; /goal pause pauses at the next step; Ctrl+C cancels; Session settings stay pinned\n",
                "Command: /help; List commands; during Run: available\n",
                "Command: /setup; Set up an account; during Run: locked\n",
                "Command: /paste; Paste clipboard into draft; during Run: view-only\n",
                "Command: /copy; Copy the latest answer to the clipboard; during Run: locked\n",
                "Command: /usage; Reported token usage; during Run: available\n",
                "Command: /status; Session status and usage; during Run: available\n",
                "Command: /new; New Session; during Run: locked\n",
                "Command: /clear; New Session; during Run: locked\n",
                "Command: /rewind; Restore code, conversation or both; compare changes; during Run: locked\n",
                "Command: /resume [session-id]; Resume Session; during Run: locked\n",
                "Command: /goal [pause|resume|continue|objective]; Goal status; pause during a Run; resume, continue or start when idle; during Run: available\n",
                "Command: /fork [session-id]; Fork Session; during Run: locked\n",
                "Command: /rename <title>; Rename Session; during Run: locked\n",
                "Command: /compact; Compact context; during Run: locked\n",
                "Command: /agents <single|auto|team> [max-active-children]; Agent details; during Run: view-only\n",
                "Command: /provider <provider>; Choose Provider; during Run: view-only\n",
                "Command: /model <model-id> [effort|default]; Choose model; during Run: view-only\n",
                "Command: /permissions; Folder trust and approval settings; during Run: available\n",
                "Command: /cache clear [all]; Clear private build caches; during Run: locked\n",
                "Command: /settings; Slash menu, chat view and newline shortcut; during Run: locked\n",
                "Command: /transcript [agents [number]]; Write or toggle transcript details; agents lists this Run's agent chats; during Run: available\n",
                "Command: /exit; Exit Session; during Run: view-only\n",
            ).as_bytes()
        );
        assert!(!help.contains(&b'\r'));
        assert!(!help.contains(&0x1b));
        assert!(!linear.line_open);
        let mut after_help = Vec::new();
        linear
            .write_view(&mut after_help, &view, None)
            .expect("idle prompt after help");
        assert_eq!(after_help, b"Input:\n");

        let items = vec![SessionListItem {
            id: view.id,
            title: view.title.clone(),
            last_sequence: view.last_sequence,
            created_at: "2026-10-06 12:00:00".into(),
            last_activity_at: "2026-10-06 12:05:00".into(),
            defaults: view.defaults.clone(),
        }];
        let mut picker = Vec::new();
        linear
            .write_sessions(&mut picker, &items, 0)
            .expect("linear picker");
        linear
            .write_sessions(&mut picker, &items, 0)
            .expect("unchanged picker page does not repeat");
        assert_eq!(
            String::from_utf8(picker.clone()).expect("UTF-8 picker"),
            concat!(
                "Resume: 1 in this Workspace\n",
                "Choice 1: bad\\u{001b}[31m\\u{000a}Session: forged\n",
                "Created: 2026-10-06 12:00:00 UTC; last activity: 2026-10-06 12:05:00 UTC\n",
                "Access: User default; model: Default model\n",
                "Choose number, exact ID, n next, p previous, or q close:\n",
            )
        );
        assert!(!picker.contains(&b'\r'));
        assert!(!picker.contains(&0x1b));

        linear.newline_pending = true;
        let mut invalid = Vec::new();
        linear
            .write_picker_invalid(&mut invalid)
            .expect("partial choice does not fuse with notice");
        assert_eq!(invalid, b"\nNotice: invalid Session selection\n");

        let model = AgentInspectorModel::from_session(&view, &AgentQuery::default(), 120, true);
        let agent_id = view.runs[0].agents[0].id;
        let mut inspector = LinearState::new("screen-reader");
        let mut detail = Vec::new();
        inspector
            .write_agents(&mut detail, &model, 0, None)
            .expect("linear agent details");
        assert_eq!(
            String::from_utf8(detail.clone()).expect("UTF-8 agent details"),
            format!(
                "Current Run: 1 agent; children: 0\n\
                 History: 1 agents in 1 recent Runs; 0 older Runs\n\
                 Next Run: auto · up to 3 children\n\
                 Current Run: admission in progress; topology locked\n\
                 Type to search 1 agent\n\
                 Choices 1-1 of 1:\n\
                 1. Run 1: primary; active · Question (focused)\n\
                 Agent 1/1: primary; active\n\
                 Session: {}\n\
                 Run: {run_id}\n\
                 AgentRun: {agent_id}\n\
                 Objective: Question\n\
                 Agent choice: n next, p previous, number select, o open, /TEXT search, q close:\n",
                view.id
            )
        );
        assert!(!detail.contains(&b'\r'));
        assert!(!detail.contains(&0x1b));

        let mut refreshed = Vec::new();
        inspector
            .write_agents(&mut refreshed, &model, 0, None)
            .expect("progress redraw after partial choice");
        assert!(refreshed.starts_with(b"\nCurrent Run:"));

        let read = crate::presentation::tool_fixture(
            arany::ToolCall::Read {
                path: "a".into(),
                offset: None,
                limit: Some(100),
            },
            r#"{"text":"a\nb\n"}"#,
        );
        let mut pending = read.clone();
        pending.observation = None;
        view.runs[0].tools = vec![read.clone(), pending];
        view.last_sequence += 1;
        let mut rows = Vec::new();
        linear.write_view(&mut rows, &view, None).unwrap();
        assert!(!String::from_utf8(rows).unwrap().contains("Read("));
        view.runs[0].tools[1] = read;
        view.last_sequence += 1;
        let mut rows = Vec::new();
        linear.write_view(&mut rows, &view, None).unwrap();
        assert!(!String::from_utf8(rows).unwrap().contains("Read("));
        view.runs[0].status = RunStatus::Cancelled;
        view.last_sequence += 1;
        let mut rows = Vec::new();
        linear.write_view(&mut rows, &view, None).unwrap();
        let rows = String::from_utf8(rows).unwrap();
        assert!(rows.ends_with("Read 2 files\nResult: 4 lines\n"));
        assert!(!rows.contains(['⏺', '⎿', '\r', '\u{1b}']));
        view.last_sequence += 1;
        let mut redraw = Vec::new();
        linear.write_view(&mut redraw, &view, None).unwrap();
        let redraw = String::from_utf8(redraw).unwrap();
        assert!(!redraw.contains("Read("));
        assert!(!redraw.contains(['⏺', '⎿', '\r', '\u{1b}']));
        view.runs[0].tools.clear();
        view.runs[0].status = RunStatus::Active;
        view.runs[0].supervisor =
            crate::presentation::supervisor_fixture(view.runs[0].agents[0].id, 1);
        let mut linear = LinearState::new("screen-reader");
        let mut output = Vec::new();
        linear.write_transcript(&mut output, &view).unwrap();
        assert_eq!(output, b"Transcript details:\nSupervisor (shadow): possible loop, possible probing, possibly disproportionate changes; no action taken\n");
        view.runs[0].supervisor =
            crate::presentation::supervisor_fixture(view.runs[0].agents[0].id, 256);
        let inspector =
            AgentInspectorModel::from_session(&view, &AgentQuery::default(), 120, false);
        let mut inspected = Vec::new();
        linear
            .write_agents(&mut inspected, &inspector, 0, None)
            .unwrap();
        let inspected = String::from_utf8(inspected).unwrap();
        assert_eq!(inspected.matches("Supervisor (shadow):").count(), 256);
        assert_eq!(inspected.matches("; no action taken").count(), 256);
        assert!(inspected.contains("Supervisor (shadow): possible loop, possible probing, possibly disproportionate changes; no action taken; verdict limit reached\n"));
        assert!(!inspected.contains(['\u{1b}', '\r']));
        let read = crate::presentation::tool_fixture(
            arany::ToolCall::Read {
                path: "a".into(),
                offset: None,
                limit: Some(100),
            },
            r#"{"text":"a\nb\n"}"#,
        );
        let mut pending = read.clone();
        pending.observation = None;
        view.runs[0].tools = vec![read.clone(), pending];
        view.last_sequence += 1;
        let mut rows = Vec::new();
        linear.write_view(&mut rows, &view, None).unwrap();
        assert!(!String::from_utf8(rows).unwrap().contains("Read("));
        view.runs[0].tools[1] = read;
        view.last_sequence += 1;
        let mut rows = Vec::new();
        linear.write_view(&mut rows, &view, None).unwrap();
        assert!(!String::from_utf8(rows).unwrap().contains("Read("));
        view.runs[0].status = RunStatus::Cancelled;
        view.last_sequence += 1;
        let mut rows = Vec::new();
        linear.write_view(&mut rows, &view, None).unwrap();
        let rows = String::from_utf8(rows).unwrap();
        assert!(rows.ends_with("Read 2 files\nResult: 4 lines\n"));
        assert!(!rows.contains(['⏺', '⎿', '\r', '\u{1b}']));
        view.last_sequence += 1;
        let mut redraw = Vec::new();
        linear.write_view(&mut redraw, &view, None).unwrap();
        let redraw = String::from_utf8(redraw).unwrap();
        assert!(!redraw.contains("Read("));
        assert!(!redraw.contains(['⏺', '⎿', '\r', '\u{1b}']));
        view.runs[0].tools.clear();
        view.runs[0].status = RunStatus::Active;
        view.runs[0].supervisor =
            crate::presentation::supervisor_fixture(view.runs[0].agents[0].id, 1);
        view.runs = vec![view.runs[0].clone(); 17];
        output.clear();
        linear.write_transcript(&mut output, &view).unwrap();
        let text = std::str::from_utf8(&output).unwrap();
        assert_eq!(text.matches("Supervisor (shadow):").count(), 16);
        assert!(text.ends_with(
            "Transcript detail limit reached; use show --output jsonl for the complete journal.\n"
        ));
        for run in &mut view.runs {
            run.supervisor = crate::presentation::supervisor_fixture(run.agents[0].id, 256);
        }
        output.clear();
        linear.write_transcript(&mut output, &view).unwrap();
        assert!(output.ends_with(
            b"Transcript detail limit reached; use show --output jsonl for the complete journal.\n"
        ));
        assert!(output.len() <= super::super::history::MAX_CLASSIC_HISTORY_BYTES);
        assert!(!output.contains(&0x1b));
        assert!(!output.contains(&b'\r'));

        for (run_count, line_open, newline_pending) in
            [(17, false, false), (3, true, false), (17, false, true)]
        {
            let mut run = view.runs[0].clone();
            run.supervisor = Default::default();
            run.display.clear();
            view.runs = vec![run; run_count];
            for (index, run) in view.runs.iter_mut().rev().take(16).rev().enumerate() {
                let text = match index {
                    0 | 1 => "x".repeat(32_700),
                    2 if run_count == 3 => "x".repeat(100),
                    _ => continue,
                };
                run.display = text
                    .as_bytes()
                    .chunks(2048)
                    .enumerate()
                    .map(|(index, bytes)| arany::DisplayChunk {
                        call_index: 0,
                        phase: arany::AgentPhase::RootPlan,
                        category: arany::DisplayCategory::Commentary,
                        item: 0,
                        offset: u32::try_from(index * 2048).unwrap(),
                        text: std::str::from_utf8(bytes).unwrap().to_owned(),
                    })
                    .collect();
            }
            linear.line_open = line_open;
            linear.newline_pending = newline_pending;
            output.clear();
            linear.write_transcript(&mut output, &view).unwrap();
            assert!(
                output.len() <= super::super::history::MAX_CLASSIC_HISTORY_BYTES,
                "transcript exceeded byte limit: runs={run_count}, line_open={line_open}, newline_pending={newline_pending}, bytes={}",
                output.len()
            );
            assert!(output.ends_with(
                b"Transcript detail limit reached; use show --output jsonl for the complete journal.\n"
            ));
            assert_eq!(output.starts_with(b"\n"), line_open || newline_pending);
        }

        view.runs.clear();
        view.created_sequence = 0;
        view.last_sequence = 0;
        let mut unsaved = LinearState::new("screen-reader");
        let mut output = Vec::new();
        unsaved
            .write_view(&mut output, &view, None)
            .expect("unsaved frame");
        let text = String::from_utf8(output.clone()).expect("UTF-8");
        assert!(text.contains("Conversation: new · not saved\n"));
        assert!(!text.contains(&format!("Session: {}", view.id)));
        let boundary = output.len();
        unsaved
            .write_view(&mut output, &view, None)
            .expect("quiet unsaved redraw");
        assert_eq!(output.len(), boundary);
        view.defaults.model = Some("selected-model".into());
        view.title = "Local title".into();
        view.title_is_explicit = true;
        unsaved
            .write_view(&mut output, &view, None)
            .expect("local selection redraw");
        let changed = std::str::from_utf8(&output[boundary..]).expect("UTF-8");
        assert!(changed.contains("Model: selected-model\n"));
        assert!(changed.contains("Title: Local title\n"));
    }

    #[test]
    fn linear_consent_announces_the_complete_warning_once() {
        let mut linear = LinearState::new("screen-reader");
        let warning = format!(
            "{} Choose Accept to continue, or Back to cancel.",
            "Risk detail. ".repeat(30)
        );
        let mut output = Vec::new();
        linear
            .write_setup_with_limit(
                &mut output,
                1,
                "ChatGPT plan consent",
                &warning,
                None,
                (1024, false),
            )
            .expect("complete warning");
        assert_eq!(
            output,
            format!("Setup: ChatGPT plan consent\nChoose: {warning}\n").as_bytes()
        );
        let mut unchanged = Vec::new();
        linear
            .write_setup_with_limit(
                &mut unchanged,
                1,
                "ChatGPT plan consent",
                &warning,
                None,
                (1024, false),
            )
            .expect("no repeated announcement");
        assert!(unchanged.is_empty());
    }

    #[test]
    fn linear_setup_announces_distinct_choices_with_the_same_step_number() {
        let mut linear = LinearState::new("screen-reader");
        let mut output = Vec::new();
        linear
            .write_setup(&mut output, 1, "Choose access method", "1 API key", None)
            .expect("access choice");
        linear.prompt_needed = true;
        linear.line_open = false;
        linear
            .write_setup(&mut output, 1, "File? 1 yes 2 no", "NOT encrypted", None)
            .expect("file consent");
        assert!(
            String::from_utf8_lossy(&output)
                .contains("Setup: File? 1 yes 2 no\nChoose: NOT encrypted\nInput:\n")
        );
        linear.prompt_needed = true;
        linear.line_open = false;
        output.clear();
        linear
            .write_setup(&mut output, 3, "API workspace ID", "Enter wrkspc_ ID", None)
            .expect("non-secret field prompt");
        assert_eq!(
            output,
            b"Setup: API workspace ID\nChoose: Enter wrkspc_ ID\nInput:\n"
        );
        output.clear();
        linear
            .write_setup(&mut output, 3, "API workspace ID", "Enter wrkspc_ ID", None)
            .expect("quiet visible field edits");
        assert!(output.is_empty());
        assert!(!output.contains(&0x1b) && !output.contains(&b'\r'));
    }

    #[test]
    fn linear_model_catalog_pages_are_labeled_bounded_and_inert() {
        let items = (0..12)
            .map(|index| ModelEntry {
                id: if index == 0 {
                    "bad\u{1b}[31m".into()
                } else {
                    format!("model-{index}")
                },
                runnable: index == 1,
                efforts: if index == 1 {
                    vec![arany::Effort::Low]
                } else {
                    Vec::new()
                },
            })
            .collect::<Vec<_>>();
        let mut linear = LinearState::new("screen-reader");
        linear.last_setup = Some((5, "Choose model effort".into(), "1 none".into()));
        let mut output = Vec::new();
        linear
            .write_models(
                &mut output,
                "openai",
                &super::super::ModelPicker {
                    items: &items,
                    efforts: &vec![Some(arany::Effort::Low); items.len()],
                    selected: 0,
                    exact_custom: false,
                    filter: "",
                    invalid: false,
                    catalog_state: crate::terminal::ModelCatalogState::Current,
                },
            )
            .expect("first page");
        assert_eq!(linear.last_setup, None);
        assert!(output.starts_with(b"Models: openai; 12 available\n"));
        assert!(
            String::from_utf8_lossy(&output)
                .contains("Choice 1: bad\\u{001b}[31m; selected effort low; availability only; compatibility unknown")
        );
        assert!(
            String::from_utf8_lossy(&output)
                .contains("Choice 2: model-1; selected effort low; selectable; effort low")
        );
        assert!(!output.contains(&0x1b));
        assert!(!output.contains(&b'\r'));
        let count = output.len();
        linear
            .write_models(
                &mut output,
                "openai",
                &super::super::ModelPicker {
                    items: &items,
                    efforts: &vec![Some(arany::Effort::Low); items.len()],
                    selected: 0,
                    exact_custom: false,
                    filter: "",
                    invalid: false,
                    catalog_state: crate::terminal::ModelCatalogState::Current,
                },
            )
            .expect("unchanged page");
        assert_eq!(output.len(), count);
        linear
            .write_models(
                &mut output,
                "openai",
                &super::super::ModelPicker {
                    items: &items,
                    efforts: &vec![Some(arany::Effort::Low); items.len()],
                    selected: 10,
                    exact_custom: false,
                    filter: "",
                    invalid: false,
                    catalog_state: crate::terminal::ModelCatalogState::Current,
                },
            )
            .expect("second page");
        let text = String::from_utf8(output).expect("UTF-8 catalog");
        assert!(text.contains(
            "Choice 1: model-10; selected effort low; availability only; compatibility unknown"
        ));
        assert!(!text.contains("Choice 3: model-12"));

        let mut hostile = Vec::new();
        LinearState::new("screen-reader")
            .write_models(
                &mut hostile,
                "bad\u{1b}[31m",
                &super::super::ModelPicker {
                    items: &items,
                    efforts: &vec![Some(arany::Effort::Low); items.len()],
                    selected: 0,
                    exact_custom: false,
                    filter: "",
                    invalid: false,
                    catalog_state: crate::terminal::ModelCatalogState::Current,
                },
            )
            .expect("hostile profile label");
        assert!(String::from_utf8_lossy(&hostile).contains("Models: bad\\u{001b}[31m"));
        assert!(!hostile.contains(&0x1b));
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use arany::{TokenCount, UsageGroup};

    #[test]
    fn usage_report_pages_keep_scopes_unknowns_and_complete_linear_text() {
        let mut view = crate::presentation::team_view();
        view.defaults.provider = Some("chatgpt".into());
        let run_id = view.runs.last().unwrap().id;
        let known = |value| TokenCount {
            known: Some(value),
            unreported: 0,
        };
        for index in 0..20 {
            view.usage.groups.push(UsageGroup {
                session_id: view.id,
                run_id,
                provider: if index == 0 {
                    "chatgpt".into()
                } else {
                    format!("custom:route-{index:02}")
                },
                model: format!("model-{index:02}\u{1b}[31m"),
                input: known(12_340),
                output: TokenCount {
                    known: Some(8),
                    unreported: 1,
                },
                cache_read: known(0),
                cache_creation: TokenCount {
                    known: None,
                    unreported: 1,
                },
            });
        }
        let inherited = arany::SessionId::new();
        let mut group = view.usage.groups[0].clone();
        group.session_id = inherited;
        group.input = known(999);
        view.usage.groups.push(group);
        view.usage.pending.push((view.id, run_id, None));
        let pages = crate::presentation::usage::usage_pages(&view, true, None);
        assert!(pages.len() > 2);
        for page in &pages {
            assert!(page.len() <= crate::presentation::usage::MAX_USAGE_PAGE_BYTES);
            assert!(page.matches("Provider: ").count() <= 16);
            assert!(page.contains("totals incomplete"));
            assert_eq!(page.matches("Groups ").count(), 1);
        }
        let mut linear = LinearState::new("screen-reader");
        linear.line_open = true;
        let mut bytes = Vec::new();
        linear.write_usage(&mut bytes, &pages).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let mut expected = String::from("\n");
        for (index, page) in pages.iter().enumerate() {
            use std::fmt::Write;
            writeln!(expected, "Usage page {} of {}", index + 1, pages.len()).unwrap();
            expected.push_str(page);
        }
        assert_eq!(text, expected);
        assert!(text.contains("Output: 8 known; some calls unreported"));
        assert!(text.contains("Cache read (included in input): 0 known; some calls unreported"));
        assert!(text.contains("Cache write (included in input): unknown (not reported)"));
        assert!(text.contains("ChatGPT weekly: unknown (not reported)"));
        assert_eq!(
            text.matches("ChatGPT weekly: unknown (not reported)")
                .count(),
            text.matches("Provider: chatgpt\n").count()
        );
        assert!(!text.contains("5-hour"));
        assert!(text.contains("Session totals\nProvider: chatgpt\nModel: model-00\\u{001b}[31m\nInput: 12,340 known; some calls unreported"));
        assert!(text.contains("Inherited history (excluded from Session totals)\nProvider: chatgpt\nModel: model-00\\u{001b}[31m\nInput: 999\n"));
        assert!(!text.contains(['\r', '\u{1b}']));
        assert!(linear.prompt_needed);
        view.usage.groups.truncate(1);
        view.usage.pending.clear();
        let pages = crate::presentation::usage::usage_pages(&view, false, None);
        assert_eq!(pages.len(), 1);
        assert!(!pages[0].contains("Groups "));
        view.usage.groups.clear();
        let pages = crate::presentation::usage::usage_pages(&view, false, None);
        assert_eq!(pages[0].matches("ChatGPT weekly:").count(), 1);
    }
}
