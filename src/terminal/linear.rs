use crate::presentation::{
    AgentInspectorModel, escape_terminal, image_message_lines, linear_session_lines, safe_truncate,
    user_message_lines,
};
#[cfg(test)]
use crate::provider::ModelEntry;
use crate::session::{SessionId, SessionListItem, SessionView};
use std::io::{self, Write};

use super::CommandAvailability;
use super::commands::{help_entry, help_len};

pub(super) struct LinearState {
    mode_label: &'static str,
    announced: bool,
    last_view: Option<(SessionId, u64)>,
    pub(super) prompt_needed: bool,
    pub(super) last_picker_page: Option<usize>,
    pub(super) last_model_page: Option<(usize, super::ModelCatalogState)>,
    last_setup: Option<(u8, String, String)>,
    pub(super) line_open: bool,
    pub(super) newline_pending: bool,
}

impl LinearState {
    pub(super) fn new(mode_label: &'static str) -> Self {
        Self {
            mode_label,
            announced: false,
            last_view: None,
            prompt_needed: true,
            last_picker_page: None,
            last_model_page: None,
            last_setup: None,
            line_open: false,
            newline_pending: false,
        }
    }

    pub(super) fn draw(&mut self, view: &SessionView, notice: Option<&str>) -> io::Result<()> {
        self.write_view(&mut io::stderr(), view, notice)
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
            "During a Run: Enter retains task drafts; Ctrl+C cancels; Session settings stay pinned"
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
        warning: &str,
        notice: Option<&str>,
    ) -> io::Result<()> {
        self.write_setup_with_limit(
            &mut io::stderr(),
            1,
            "ChatGPT plan consent",
            warning,
            notice,
            (1024, false),
        )
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
        self.write_setup_with_limit(
            writer,
            step,
            title,
            instruction,
            notice,
            (240, matches!(step, 1 | 2 | 3 | 5)),
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
            writeln!(writer, "Setup: {}", safe_truncate(title, 160))?;
            let displayed_instruction = if instruction_limit > 240 {
                if instruction.len() > instruction_limit
                    || !instruction
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
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
                "Input: Enter submits; Ctrl+D after text continues; empty Ctrl+D exits",
            )?;
            writeln!(
                writer,
                "Commands: /help lists local controls; type arguments literally; Tab and Ctrl+O are inline-only"
            )?;
            self.announced = true;
        }
        let identity = (view.id, view.last_sequence);
        if self.last_view != Some(identity) {
            for line in linear_session_lines(view) {
                writeln!(writer, "{line}")?;
            }
            self.last_view = Some(identity);
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
        writeln!(
            writer,
            "Choose number, exact ID, n next, p previous, or q close:"
        )?;
        self.last_picker_page = Some(start);
        self.line_open = true;
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
                .map_or("default", crate::provider::Effort::as_str);
            writeln!(
                writer,
                "Choice {}: {}; selected effort {}; {}",
                index + 1,
                safe_truncate(&item.id, 128),
                effort,
                status
            )?;
        }
        writeln!(
            writer,
            "Choose number [effort|default], n next, p previous, or q close:"
        )?;
        self.last_model_page = Some((start, picker.catalog_state));
        self.line_open = true;
        writer.flush()
    }

    pub(super) fn write_agents(
        &mut self,
        writer: &mut impl Write,
        model: &AgentInspectorModel,
        invalid: bool,
    ) -> io::Result<()> {
        if self.newline_pending || self.line_open {
            writeln!(writer)?;
            self.newline_pending = false;
            self.line_open = false;
        }
        if invalid {
            writeln!(writer, "Notice: invalid agent selection")?;
        }
        for line in &model.lines {
            writeln!(writer, "{line}")?;
        }
        writeln!(
            writer,
            "Agent choice: n next, p previous, number select, q close:"
        )?;
        self.line_open = true;
        writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
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
            .attach_image(crate::provider::test_image())
            .unwrap();
        let mut with_image = Vec::new();
        linear
            .write_clipboard_draft(&mut with_image, &composer)
            .unwrap();
        assert_eq!(with_image, "Draft (not sent):\n  alpha\\u{0009}beta\n  🙂\n  Image 1: PNG 1x1, 0.1 KB\nDraft: 12 characters; Enter submits when idle; Ctrl+C clears when idle\n".as_bytes());
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
                 Input: Enter submits; Ctrl+D after text continues; empty Ctrl+D exits\n\
                 Commands: /help lists local controls; type arguments literally; Tab and Ctrl+O are inline-only\n\
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
            id: run_id,
            objective: "Question".into(),
            images: Vec::new(),
            config: None,
            agents: vec![AgentView {
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 2,
            finished_sequence: None,
            tools: Vec::new(),
        });
        let mut progress = Vec::new();
        linear
            .write_view(
                &mut progress,
                &view,
                Some("Run in progress... Ctrl+C cancels"),
            )
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
                 Agent: primary; state: working\n\
                 Notice: Run in progress... Ctrl+C cancels\n",
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
                "During a Run: Enter retains task drafts; Ctrl+C cancels; Session settings stay pinned\n",
                "Command: /help; List commands; during Run: available\n",
                "Command: /setup; Set up an account; during Run: locked\n",
                "Command: /paste; Paste clipboard into draft; during Run: view-only\n",
                "Command: /status; Session status; during Run: available\n",
                "Command: /new; New Session; during Run: locked\n",
                "Command: /clear; New Session; during Run: locked\n",
                "Command: /resume [session-id]; Resume Session; during Run: locked\n",
                "Command: /fork <session-id>; Fork Session; during Run: locked\n",
                "Command: /rename <title>; Rename Session; during Run: locked\n",
                "Command: /compact; Compact context; during Run: locked\n",
                "Command: /agents <single|auto|team> [max-active-children]; Agent details; during Run: view-only\n",
                "Command: /provider <provider>; Choose Provider; during Run: view-only\n",
                "Command: /model <model-id> [effort|default]; Choose model; during Run: view-only\n",
                "Command: /permissions; Show permissions; during Run: available\n",
                "Command: /quit; Exit Session; during Run: view-only\n",
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

        let model = AgentInspectorModel::from_session(&view, 0, 120, true);
        let agent_id = view.runs[0].agents[0].id;
        let mut inspector = LinearState::new("screen-reader");
        let mut detail = Vec::new();
        inspector
            .write_agents(&mut detail, &model, false)
            .expect("linear agent details");
        assert_eq!(
            String::from_utf8(detail.clone()).expect("UTF-8 agent details"),
            format!(
                "Agents: 1 in 1 recent Runs; 0 older Runs\n\
                 Next Run: Auto {{ max_active_children: 3 }}\n\
                 Current Run: admission in progress; topology locked\n\
                 Agent 1/1: primary; Active\n\
                 Session: {}\n\
                 Run: {run_id}\n\
                 AgentRun: {agent_id}\n\
                 Objective: Question\n\
                 Agent choice: n next, p previous, number select, q close:\n",
                view.id
            )
        );
        assert!(!detail.contains(&b'\r'));
        assert!(!detail.contains(&0x1b));

        let mut refreshed = Vec::new();
        inspector
            .write_agents(&mut refreshed, &model, false)
            .expect("progress redraw after partial choice");
        assert!(refreshed.starts_with(b"\nAgents:"));
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
                    vec![crate::provider::Effort::Low]
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
                    efforts: &vec![Some(crate::provider::Effort::Low); items.len()],
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
                    efforts: &vec![Some(crate::provider::Effort::Low); items.len()],
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
                    efforts: &vec![Some(crate::provider::Effort::Low); items.len()],
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
                    efforts: &vec![Some(crate::provider::Effort::Low); items.len()],
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
