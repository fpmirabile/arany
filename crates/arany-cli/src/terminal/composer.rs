use super::{
    TerminalInput,
    commands::{
        CommandChoices, CommandParseError, CommandPreview, Submission, command_completions,
        command_preview, completion_suffix, parse_submission,
    },
};
use arany::{ImageAttachment, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, ModelEntry};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub const MAX_DRAFT_BYTES: usize = 8 * 1024;
pub(super) const MAX_PASTE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TextPaste {
    range: std::ops::Range<usize>,
    characters: usize,
    suffix: usize,
}

impl TextPaste {
    fn label(&self) -> String {
        let label = format!("[Pasted Content {} chars]", self.characters);
        if self.suffix == 1 {
            label
        } else {
            format!("{label} #{}", self.suffix)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComposerDraft {
    text: String,
    pastes: Vec<TextPaste>,
    images: Vec<ImageAttachment>,
    cursor: usize,
}

impl ComposerDraft {
    pub fn from_steering_note(mut note: String, offered: &[Self]) -> Self {
        let mut pastes = Vec::new();
        let mut end = note.len();
        for draft in offered.iter().rev() {
            if end == 0 {
                break;
            }
            let escaped = objective_escape_len(&draft.text, &draft.pastes);
            let text = &draft.text[escaped..];
            assert!(
                note[..end].ends_with(text),
                "withdrawn note matches offered drafts"
            );
            let offset = end - text.len();
            for paste in &draft.pastes {
                let mut paste = paste.clone();
                paste.range.start = paste.range.start - escaped + offset;
                paste.range.end = paste.range.end - escaped + offset;
                pastes.push(paste);
            }
            end = offset.saturating_sub(2);
        }
        assert_eq!(end, 0, "withdrawn note has retained provenance");
        pastes.sort_by_key(|paste| paste.range.start);
        if note.starts_with('/') && pastes.first().is_none_or(|paste| paste.range.start > 0) {
            note.insert(0, '/');
            for paste in &mut pastes {
                paste.range.start += 1;
                paste.range.end += 1;
            }
        }
        for index in 0..pastes.len() {
            if pastes[..index].iter().any(|paste| {
                paste.characters == pastes[index].characters && paste.suffix == pastes[index].suffix
            }) {
                pastes[index].suffix = pastes[..index]
                    .iter()
                    .filter(|paste| paste.characters == pastes[index].characters)
                    .map(|paste| paste.suffix)
                    .max()
                    .unwrap_or(0)
                    + 1;
            }
        }
        Self {
            cursor: note.len(),
            text: note,
            pastes,
            images: Vec::new(),
        }
    }

    pub fn images(&self) -> &[ImageAttachment] {
        &self.images
    }
    pub fn editable_text(&self) -> String {
        editable_text(&self.text, &self.pastes)
    }
}

fn editable_bytes(text: &str, pastes: &[TextPaste]) -> usize {
    text.len() - pastes.iter().map(|paste| paste.range.len()).sum::<usize>()
}

fn objective_escape_len(text: &str, pastes: &[TextPaste]) -> usize {
    usize::from(text.starts_with("//") && pastes.first().is_none_or(|paste| paste.range.start >= 2))
}

fn editable_text(text: &str, pastes: &[TextPaste]) -> String {
    let mut result = String::new();
    let mut start = 0;
    for paste in pastes {
        result.push_str(&text[start..paste.range.start]);
        result.push(' ');
        start = paste.range.end;
    }
    result.push_str(&text[start..]);
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerEdit {
    Changed,
    Unchanged,
    AtCapacity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionLayout {
    #[default]
    Tabs,
    Combined,
}

impl CompletionLayout {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tabs => "tabs",
            Self::Combined => "combined",
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Tabs => Self::Combined,
            Self::Combined => Self::Tabs,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewlineShortcut {
    #[default]
    ShiftEnter,
    AltEnter,
    CtrlEnter,
}

impl NewlineShortcut {
    pub const ALL: [Self; 3] = [Self::ShiftEnter, Self::AltEnter, Self::CtrlEnter];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ShiftEnter => "Shift+Enter",
            Self::AltEnter => "Alt+Enter",
            Self::CtrlEnter => "Ctrl+Enter",
        }
    }
}

#[derive(Default)]
pub struct Composer {
    text: String,
    pastes: Vec<TextPaste>,
    retained: Option<ComposerDraft>,
    pending_draft: Option<ComposerDraft>,
    detail_scroll: Option<usize>,
    continuation: Option<usize>,
    newline_shortcut: NewlineShortcut,
    shortcut_received: bool,
    text_only: bool,
    images: Vec<ImageAttachment>,
    cursor: usize,
    vertical_grapheme_column: Option<usize>,
    choices: CommandChoices,
    completion_index: usize,
    completion_hidden: bool,
    completion_layout: CompletionLayout,
    skills_tab: bool,
    file_query: Option<String>,
    file_candidates: Vec<String>,
    skills: Vec<String>,
    approval_mode: Option<arany::ApprovalMode>,
    recalled_draft: Option<ComposerDraft>,
    recalled_pending: bool,
}

impl Composer {
    pub fn newline_shortcut(&self) -> NewlineShortcut {
        self.newline_shortcut
    }

    pub fn set_newline_shortcut(&mut self, shortcut: NewlineShortcut) {
        if self.newline_shortcut != shortcut {
            self.shortcut_received = false;
        }
        self.newline_shortcut = shortcut;
    }

    pub(super) fn newline_hint(&self) -> &'static str {
        if self.shortcut_received {
            self.newline_shortcut.label()
        } else {
            "\\ then Enter"
        }
    }

    pub fn disarm_continuation(&mut self) {
        self.continuation = None;
    }

    pub fn continue_line(&mut self) -> bool {
        if self.continuation.take() != Some(self.cursor)
            || self.cursor == 0
            || self.text.as_bytes()[self.cursor - 1] != b'\\'
            || self
                .pastes
                .iter()
                .any(|paste| paste.range.contains(&(self.cursor - 1)))
        {
            return false;
        }
        self.text.replace_range(self.cursor - 1..self.cursor, "\n");
        self.vertical_grapheme_column = None;
        true
    }

    pub fn observe_input(&mut self, input: TerminalInput) {
        if !matches!(
            input,
            TerminalInput::Submit | TerminalInput::Resize | TerminalInput::PointerMove { .. }
        ) {
            self.disarm_continuation();
        }
    }

    pub fn chat_input(&mut self, input: TerminalInput) -> TerminalInput {
        self.observe_input(input);
        if let TerminalInput::ModifiedEnter(shortcut) = input {
            if shortcut == self.newline_shortcut {
                self.shortcut_received = true;
            }
            if shortcut == NewlineShortcut::ShiftEnter || shortcut == self.newline_shortcut {
                return TerminalInput::Newline;
            }
        }
        input
    }

    pub(super) fn text_only() -> Self {
        Self {
            text_only: true,
            ..Self::default()
        }
    }

    pub fn restore_prompt(&mut self, text: &str) -> Result<ComposerEdit, &'static str> {
        let literal = (!self.text_only && text.starts_with('/')).then(|| format!("/{text}"));
        self.insert_literal(literal.as_deref().unwrap_or(text))
    }

    pub fn insert_literal(&mut self, text: &str) -> Result<ComposerEdit, &'static str> {
        self.insert_text(text, false)
    }

    pub fn input_capacity(&self) -> usize {
        MAX_DRAFT_BYTES
            .saturating_sub(editable_bytes(&self.text, &self.pastes))
            .min(MAX_PASTE_BYTES.saturating_sub(self.text.len()))
    }
    pub fn remember_and_clear(&mut self) -> bool {
        if self.recalled_pending {
            return false;
        }
        self.recalled_draft = Some(self.take_draft());
        true
    }

    pub fn take_draft(&mut self) -> ComposerDraft {
        let draft = ComposerDraft {
            text: std::mem::take(&mut self.text),
            pastes: std::mem::take(&mut self.pastes),
            images: std::mem::take(&mut self.images),
            cursor: self.cursor,
        };
        self.clear();
        draft
    }

    pub fn restore_draft(&mut self, draft: ComposerDraft) -> bool {
        self.disarm_continuation();
        let separator = usize::from(!self.text.is_empty() && !draft.text.is_empty());
        let limit = if self.text_only {
            MAX_DRAFT_BYTES
        } else {
            MAX_PASTE_BYTES
        };
        if self.text.len() + draft.text.len() + separator > limit
            || editable_bytes(&self.text, &self.pastes)
                + editable_bytes(&draft.text, &draft.pastes)
                + separator
                > MAX_DRAFT_BYTES
            || self.images.len() + draft.images.len() > MAX_MESSAGE_IMAGES
            || self
                .images
                .iter()
                .chain(&draft.images)
                .map(ImageAttachment::byte_len)
                .sum::<usize>()
                > MAX_IMAGE_BYTES
        {
            if self.retained.is_none() {
                self.retained = Some(draft);
            } else if self.recalled_pending {
                self.pending_draft = Some(draft);
            } else {
                self.recalled_draft = Some(draft);
                self.recalled_pending = true;
            }
            return false;
        }
        let offset = draft.text.len() + separator;
        for paste in &mut self.pastes {
            paste.range.start += offset;
            paste.range.end += offset;
        }
        let mut text = draft.text;
        if separator != 0 {
            text.push('\n');
        }
        text.push_str(&self.text);
        let mut pastes = draft.pastes;
        for mut paste in self.pastes.drain(..) {
            if pastes
                .iter()
                .any(|old| old.characters == paste.characters && old.suffix == paste.suffix)
            {
                paste.suffix = pastes
                    .iter()
                    .filter(|old| old.characters == paste.characters)
                    .map(|old| old.suffix)
                    .max()
                    .unwrap_or(0)
                    + 1;
            }
            pastes.push(paste);
        }
        let mut images = draft.images;
        images.append(&mut self.images);
        self.cursor = if self.text.is_empty() {
            draft.cursor
        } else {
            self.cursor + offset
        };
        self.text = text;
        self.pastes = pastes;
        self.images = images;
        true
    }

    pub fn has_text_pastes(&self) -> bool {
        !self.pastes.is_empty()
    }

    pub fn editable_text(&self) -> String {
        editable_text(&self.text, &self.pastes)
    }

    pub(super) fn display_text(&self) -> (String, usize) {
        let mut text = String::new();
        let mut start = 0;
        let mut cursor = self.cursor;
        for paste in &self.pastes {
            text.push_str(&self.text[start..paste.range.start]);
            let label = paste.label();
            if self.cursor >= paste.range.end {
                cursor = cursor - paste.range.len() + label.len();
            }
            text.push_str(&label);
            start = paste.range.end;
        }
        text.push_str(&self.text[start..]);
        (text, cursor)
    }

    pub(super) fn paste_detail(&self) -> Option<(String, &str, usize)> {
        let scroll = self.detail_scroll?;
        let (label, text) = self.selected_paste()?;
        Some((label, text, scroll))
    }

    pub(super) fn clamp_detail_scroll(&mut self, maximum: usize) {
        if let Some(scroll) = &mut self.detail_scroll {
            *scroll = (*scroll).min(maximum);
        }
    }

    pub fn inspect_input(&mut self, input: TerminalInput) -> bool {
        if let Some(scroll) = self.detail_scroll.as_mut() {
            match input {
                TerminalInput::Escape | TerminalInput::Submit => self.detail_scroll = None,
                TerminalInput::Up => *scroll = scroll.saturating_sub(1),
                TerminalInput::Down => *scroll = scroll.saturating_add(1).min(MAX_PASTE_BYTES * 10),
                TerminalInput::PageUp => *scroll = scroll.saturating_sub(10),
                TerminalInput::PageDown => {
                    *scroll = scroll.saturating_add(10).min(MAX_PASTE_BYTES * 10)
                }
                TerminalInput::Home => *scroll = 0,
                TerminalInput::End => *scroll = MAX_PASTE_BYTES * 10,
                TerminalInput::Interrupt => {
                    self.detail_scroll = None;
                    return false;
                }
                TerminalInput::Resize | TerminalInput::Suspend | TerminalInput::Shutdown(_) => {
                    return false;
                }
                _ => {}
            }
            return true;
        }
        if input == TerminalInput::Tab && self.selected_paste().is_some() {
            self.detail_scroll = Some(0);
            return true;
        }
        false
    }

    pub fn selected_paste(&self) -> Option<(String, &str)> {
        self.pastes
            .iter()
            .find(|paste| paste.range.start == self.cursor)
            .map(|paste| (paste.label(), &self.text[paste.range.clone()]))
    }

    pub fn completion_layout(&self) -> CompletionLayout {
        self.completion_layout
    }

    pub fn set_completion_layout(&mut self, layout: CompletionLayout) {
        self.completion_layout = layout;
        self.skills_tab = false;
        self.completion_index = 0;
        self.completion_hidden = false;
    }

    pub(super) fn completion_header(&self) -> Option<&'static str> {
        self.completion_menu()?;
        Some(match (self.completion_layout, self.skills_tab) {
            (CompletionLayout::Combined, _) => "Commands + Skills",
            (CompletionLayout::Tabs, false) => "[Commands]  Skills · Left/Right",
            (CompletionLayout::Tabs, true) => "Commands  [Skills] · Left/Right",
        })
    }

    pub(super) fn completion_skills_selected(&self) -> bool {
        self.skills_tab
    }

    pub(super) fn completion_description(&self, row: &str) -> String {
        if let Some(name) = row.strip_prefix("Skill /") {
            return format!("Invoke /{name}");
        }
        let name = row.strip_prefix("Cmd /").unwrap_or(row);
        (0..super::commands::help_len())
            .filter_map(super::commands::help_entry)
            .find(|entry| entry.name == name)
            .map_or_else(String::new, |entry| entry.description.to_owned())
    }

    pub(super) fn empty_completion_notice(&self) -> &'static str {
        if self.skills_tab && self.skills.is_empty() {
            "No Skills · use /permissions for .agents/skills"
        } else {
            "No matches · edit the query or switch tabs"
        }
    }

    pub fn approval_mode(&self) -> Option<arany::ApprovalMode> {
        self.approval_mode
    }

    pub fn set_runtime_skills(&mut self, names: &[String]) {
        self.skills = names
            .iter()
            .take(arany::MAX_SKILLS)
            .filter(|name| arany::valid_tool_name(name))
            .cloned()
            .collect();
        self.skills.sort();
        self.skills.dedup();
        self.completion_index = 0;
    }

    pub fn set_approval_mode(&mut self, mode: Option<arany::ApprovalMode>) {
        self.approval_mode = mode;
    }

    pub fn file_query(&self) -> Option<String> {
        if self.text_only || !self.pastes.is_empty() {
            return None;
        }
        if self.cursor < self.text.len()
            && !self.text[self.cursor..].chars().next()?.is_whitespace()
        {
            return None;
        }
        if self.completion_hidden || self.text.starts_with('/') {
            return None;
        }
        let start = self.text[..self.cursor].rfind('@')?;
        if start > 0 && !self.text[..start].chars().next_back()?.is_whitespace() {
            return None;
        }
        let query = &self.text[start + 1..self.cursor];
        if let Some(quoted) = query.strip_prefix('"') {
            if quoted.contains(['"', '\\']) {
                return None;
            }
            return Some(quoted.to_owned());
        }
        (!query.chars().any(char::is_whitespace)).then(|| query.to_owned())
    }

    pub fn set_file_candidates(&mut self, query: Option<String>, items: Vec<String>) {
        if self.file_query != query {
            self.completion_index = 0;
        }
        self.file_query = query;
        self.file_candidates = items;
    }

    pub fn file_query_changed(&self) -> bool {
        self.file_query != self.file_query()
    }

    fn file_menu(&self) -> Option<(&[String], usize)> {
        if self.file_candidates.is_empty() || self.file_query.as_ref() != self.file_query().as_ref()
        {
            return None;
        }
        Some((
            &self.file_candidates,
            self.completion_index.min(self.file_candidates.len() - 1),
        ))
    }

    pub(super) fn completion_rows(&self) -> Option<(Vec<String>, usize)> {
        if let Some((items, selected)) = self.file_menu() {
            return Some((
                items.iter().map(|item| format!("@{item}")).collect(),
                selected,
            ));
        }
        self.completion_menu().map(|(names, selected)| {
            let commands = command_completions("/");
            (
                names
                    .iter()
                    .map(|name| {
                        if self.skills.iter().any(|skill| skill == name) && !commands.contains(name)
                        {
                            format!("Skill /{name}")
                        } else {
                            format!("Cmd /{name}")
                        }
                    })
                    .collect(),
                selected,
            )
        })
    }
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn character_count(&self) -> usize {
        self.text.graphemes(true).count()
    }

    pub fn cursor_byte_offset(&self) -> usize {
        self.cursor
    }

    pub fn images(&self) -> &[ImageAttachment] {
        &self.images
    }

    pub fn attach_image(&mut self, image: ImageAttachment) -> Result<(), &'static str> {
        self.disarm_continuation();
        if self.text_only {
            return Err("Child messages are text only; draft retained");
        }
        if self.retained.is_some() || self.pending_draft.is_some() {
            return Err("Two drafts retained; submit or clear this draft, then press Up");
        }
        if self.images.len() == MAX_MESSAGE_IMAGES {
            return Err("draft already has four images; submit or clear it first");
        }
        if self
            .images
            .iter()
            .map(ImageAttachment::byte_len)
            .sum::<usize>()
            + image.byte_len()
            > MAX_IMAGE_BYTES
        {
            return Err("draft images are too large; remove one or use smaller PNGs");
        }
        self.images.push(image);
        Ok(())
    }

    pub fn take_images(&mut self) -> Vec<ImageAttachment> {
        std::mem::take(&mut self.images)
    }

    pub fn submission(&self) -> Result<Submission<'_>, CommandParseError> {
        if self.has_text_pastes() {
            let escaped = objective_escape_len(&self.text, &self.pastes);
            if escaped > 0 {
                return Ok(Submission::Objective(&self.text[escaped..]));
            }
            let editable = self.editable_text();
            if editable.trim_start().starts_with('/') {
                return Err(CommandParseError::Attachments);
            }
            return Ok(Submission::Objective(&self.text));
        }
        self.parse_submission(&self.text)
    }

    pub fn parse_submission<'a>(&self, text: &'a str) -> Result<Submission<'a>, CommandParseError> {
        if text.len()
            > if self.text_only {
                MAX_DRAFT_BYTES
            } else {
                MAX_PASTE_BYTES
            }
        {
            return Err(CommandParseError::TooLong);
        }
        if !text.starts_with('/') && !text.trim().is_empty() {
            return Ok(Submission::Objective(text));
        }
        if self.text_only {
            return if text.trim().is_empty() {
                Err(CommandParseError::Empty)
            } else {
                Ok(Submission::Objective(text))
            };
        }
        match parse_submission(text) {
            Err(CommandParseError::Empty) if !self.images.is_empty() => {
                Ok(Submission::Objective(text))
            }
            Err(error @ CommandParseError::Unknown { .. }) => {
                super::commands::skill_invocation(text, &self.skills)
                    .map(|(name, task)| Submission::Skill { name, task })
                    .ok_or(error)
            }
            result => result,
        }
    }

    pub fn insert_paste(&mut self, text: &str) -> Result<ComposerEdit, &'static str> {
        self.insert_text(text, !self.text_only)
    }

    fn insert_text(&mut self, text: &str, collapse: bool) -> Result<ComposerEdit, &'static str> {
        self.disarm_continuation();
        if self.retained.is_some() || self.pending_draft.is_some() {
            return Err("Two drafts retained; submit or clear this draft, then press Up");
        }
        let limit = if collapse {
            MAX_PASTE_BYTES
        } else {
            MAX_DRAFT_BYTES
        };
        if self.text.len() + text.len() > limit {
            return Ok(ComposerEdit::AtCapacity);
        }
        if text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err("paste contains unsupported control characters");
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text.is_empty() {
            return Ok(ComposerEdit::Unchanged);
        }
        let characters = text.chars().count();
        let chip = collapse && characters > 1000;
        if !chip && !self.can_type(text.len()) {
            return Ok(ComposerEdit::AtCapacity);
        }
        for paste in &mut self.pastes {
            if paste.range.start >= self.cursor {
                paste.range.start += text.len();
                paste.range.end += text.len();
            }
        }
        if chip {
            let suffix = self
                .pastes
                .iter()
                .filter(|paste| paste.characters == characters)
                .map(|paste| paste.suffix)
                .max()
                .unwrap_or(0)
                + 1;
            self.pastes.push(TextPaste {
                range: self.cursor..self.cursor + text.len(),
                characters,
                suffix,
            });
            self.pastes.sort_by_key(|paste| paste.range.start);
        }
        self.text.insert_str(self.cursor, &text);
        let target = self.cursor + text.len();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(start, grapheme)| start + grapheme.len())
            .find(|end| *end >= target)
            .unwrap_or(self.text.len());
        if let Some(paste) = self
            .pastes
            .iter()
            .find(|paste| paste.range.start < self.cursor && self.cursor < paste.range.end)
        {
            self.cursor = paste.range.end;
        }
        self.vertical_grapheme_column = None;
        self.completion_index = 0;
        self.completion_hidden = false;
        Ok(ComposerEdit::Changed)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }

    pub fn clear(&mut self) {
        self.disarm_continuation();
        self.skills_tab = false;
        self.text.clear();
        self.pastes.clear();
        self.detail_scroll = None;
        self.images.clear();
        self.cursor = 0;
        self.vertical_grapheme_column = None;
        self.completion_index = 0;
        self.completion_hidden = false;
    }

    pub fn take(&mut self) -> String {
        self.disarm_continuation();
        self.skills_tab = false;
        self.cursor = 0;
        self.vertical_grapheme_column = None;
        self.completion_index = 0;
        self.completion_hidden = false;
        self.pastes.clear();
        std::mem::take(&mut self.text)
    }

    pub fn set_completion_selection(
        &mut self,
        profile: Option<&str>,
        model: Option<&str>,
        account_id: Option<Uuid>,
    ) {
        self.choices.set_selection(profile, model, account_id);
    }

    pub fn set_model_catalog(&mut self, profile: &str, items: &[ModelEntry], exact_custom: bool) {
        self.choices.set_catalog(profile, items, exact_custom);
    }

    pub fn cached_model_catalog(&self) -> Option<(Vec<ModelEntry>, bool)> {
        self.choices.cached_catalog()
    }

    pub(super) fn command_preview(&self) -> Option<CommandPreview> {
        if self.text_only || !self.pastes.is_empty() {
            return None;
        }
        if let Some((items, selected)) = self.file_menu() {
            return Some(CommandPreview {
                ghost: String::new(),
                ghost_is_placeholder: false,
                status: format!(
                    "{}: @{} · Up/Down choose · Esc close",
                    if items[selected].ends_with('/') {
                        "Enter reference · Tab browse"
                    } else {
                        "Tab/Enter"
                    },
                    items[selected],
                ),
            });
        }
        if let Some((names, selected)) = self.completion_menu() {
            if names.is_empty() {
                return Some(CommandPreview {
                    ghost: String::new(),
                    ghost_is_placeholder: false,
                    status: self.empty_completion_notice().to_owned(),
                });
            }
            return Some(CommandPreview {
                ghost: String::new(),
                ghost_is_placeholder: false,
                status: format!(
                    "Tab completes · Enter invokes /{} · Up/Down choose · Esc close",
                    names[selected]
                ),
            });
        }
        if self.cursor == self.text.len() {
            command_preview(&self.text, &self.choices)
        } else {
            None
        }
    }

    #[must_use]
    pub fn recall_pending(&self) -> bool {
        self.recalled_draft.is_some() || self.retained.is_some() || self.pending_draft.is_some()
    }

    pub fn has_open_menu(&self) -> bool {
        self.detail_scroll.is_some()
            || self.file_menu().is_some()
            || self.completion_menu().is_some()
    }

    fn skill_matches<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let commands = command_completions("/");
        self.skills
            .iter()
            .map(String::as_str)
            .filter(move |name| name.starts_with(prefix) && !commands.contains(name))
    }

    pub(super) fn completion_menu(&self) -> Option<(Vec<&str>, usize)> {
        if self.text_only || !self.pastes.is_empty() {
            return None;
        }
        if self.completion_hidden || self.cursor != self.text.len() {
            return None;
        }
        let prefix = self.text.strip_prefix('/')?;
        let mut names = command_completions(&self.text);
        if names.contains(&prefix) {
            return None;
        }
        if !prefix.is_empty() && !arany::valid_tool_name(prefix) {
            return None;
        }
        if self.completion_layout == CompletionLayout::Tabs && self.skills_tab {
            names.clear();
        }
        if self.completion_layout == CompletionLayout::Combined || self.skills_tab {
            names.extend(self.skill_matches(prefix));
        }
        if names.is_empty() && self.completion_layout == CompletionLayout::Combined {
            return None;
        }
        let selected = self.completion_index.min(names.len().saturating_sub(1));
        Some((names, selected))
    }

    pub fn apply(&mut self, input: TerminalInput) -> ComposerEdit {
        let input = self.chat_input(input);
        if input == TerminalInput::Submit && self.continue_line() {
            return ComposerEdit::Changed;
        }
        if input == TerminalInput::Up
            && self.is_empty()
            && let Some(draft) = self
                .retained
                .take()
                .or_else(|| self.pending_draft.take())
                .or_else(|| self.recalled_draft.take())
        {
            if self.recalled_draft.is_none() {
                self.recalled_pending = false;
            }
            self.restore_draft(draft);
            return ComposerEdit::Changed;
        }
        if let Some((items, selected)) = self.file_menu() {
            let count = items.len();
            match input {
                TerminalInput::Up => {
                    self.completion_index = (selected + count - 1) % count;
                    return ComposerEdit::Changed;
                }
                TerminalInput::Down => {
                    self.completion_index = (selected + 1) % count;
                    return ComposerEdit::Changed;
                }
                TerminalInput::Escape => {
                    self.completion_hidden = true;
                    return ComposerEdit::Changed;
                }
                TerminalInput::Tab | TerminalInput::Submit => {
                    let path = &items[selected];
                    let directory = path.ends_with('/');
                    let browse = directory && input == TerminalInput::Tab;
                    let replacement = if browse && !path.chars().any(char::is_whitespace) {
                        format!("@{path}")
                    } else if browse {
                        format!("@\"{path}")
                    } else {
                        format!(
                            "@{} ",
                            serde_json::to_string(path).expect("file name is serializable")
                        )
                    };
                    let start = self.text[..self.cursor]
                        .rfind('@')
                        .expect("file menu query");
                    if self.text.len() - (self.cursor - start) + replacement.len() > MAX_DRAFT_BYTES
                    {
                        return ComposerEdit::AtCapacity;
                    }
                    self.text.replace_range(start..self.cursor, &replacement);
                    self.cursor = start + replacement.len();
                    self.vertical_grapheme_column = None;
                    self.completion_index = 0;
                    return ComposerEdit::Changed;
                }
                _ => {}
            }
        }
        if let Some((names, selected)) = self.completion_menu() {
            if matches!(input, TerminalInput::Left | TerminalInput::Right)
                && self.completion_layout == CompletionLayout::Tabs
            {
                self.skills_tab = !self.skills_tab;
                self.completion_index = 0;
                return ComposerEdit::Changed;
            }
            if names.is_empty()
                && matches!(
                    input,
                    TerminalInput::Tab
                        | TerminalInput::Submit
                        | TerminalInput::Up
                        | TerminalInput::Down
                )
            {
                let submit = input == TerminalInput::Submit && !self.skills_tab && self.text != "/";
                if submit {
                    let skill = {
                        let mut matches = self.skill_matches(&self.text[1..]);
                        match (matches.next(), matches.next()) {
                            (Some(name), None) => Some(name.to_owned()),
                            _ => None,
                        }
                    };
                    if let Some(name) = skill {
                        self.text = format!("/{name}");
                        self.cursor = self.text.len();
                        self.completion_index = 0;
                        self.completion_hidden = true;
                    }
                }
                return if submit {
                    ComposerEdit::Unchanged
                } else {
                    ComposerEdit::Changed
                };
            }
            match input {
                TerminalInput::Up => {
                    self.completion_index = (selected + names.len() - 1) % names.len();
                    return ComposerEdit::Changed;
                }
                TerminalInput::Down => {
                    self.completion_index = (selected + 1) % names.len();
                    return ComposerEdit::Changed;
                }
                TerminalInput::Tab | TerminalInput::Submit => {
                    let name = names[selected];
                    let mut completed = format!("/{name}");
                    if input == TerminalInput::Tab {
                        if let Some(suffix) = completion_suffix(&completed, &self.choices) {
                            completed.push_str(&suffix);
                        } else if self.skills.iter().any(|skill| skill == name) {
                            completed.push(' ');
                        }
                    }
                    if completed.len() > MAX_DRAFT_BYTES {
                        return ComposerEdit::AtCapacity;
                    }
                    self.text = completed;
                    self.cursor = self.text.len();
                    self.vertical_grapheme_column = None;
                    self.completion_index = 0;
                    self.completion_hidden = input == TerminalInput::Submit;
                    return if input == TerminalInput::Submit {
                        ComposerEdit::Unchanged
                    } else {
                        ComposerEdit::Changed
                    };
                }
                TerminalInput::Escape => {
                    self.completion_hidden = true;
                    return ComposerEdit::Changed;
                }
                _ => {}
            }
        }
        if (self.retained.is_some() || self.pending_draft.is_some())
            && matches!(
                input,
                TerminalInput::Character(_) | TerminalInput::Newline | TerminalInput::Tab
            )
        {
            return ComposerEdit::AtCapacity;
        }
        let edit = self.apply_edit(input);
        if let Some(paste) = self
            .pastes
            .iter()
            .find(|paste| paste.range.start < self.cursor && self.cursor < paste.range.end)
        {
            self.cursor = if matches!(
                input,
                TerminalInput::Left | TerminalInput::WordLeft | TerminalInput::Up
            ) {
                paste.range.start
            } else {
                paste.range.end
            };
        }
        if input == TerminalInput::Character('\\') && edit == ComposerEdit::Changed {
            self.continuation = Some(self.cursor);
        }
        if edit == ComposerEdit::Changed {
            self.completion_index = 0;
            self.completion_hidden = false;
        }
        edit
    }

    fn apply_edit(&mut self, input: TerminalInput) -> ComposerEdit {
        if !matches!(input, TerminalInput::Up | TerminalInput::Down) {
            self.vertical_grapheme_column = None;
        }
        match input {
            TerminalInput::Tab
                if !self.text_only && self.pastes.is_empty() && self.cursor == self.text.len() =>
            {
                let Some(suffix) = completion_suffix(&self.text, &self.choices) else {
                    return ComposerEdit::Unchanged;
                };
                if self.text.len() + suffix.len() > MAX_DRAFT_BYTES {
                    return ComposerEdit::AtCapacity;
                }
                self.text.push_str(&suffix);
                self.cursor = self.text.len();
                ComposerEdit::Changed
            }
            TerminalInput::Character(character) if character.is_control() => {
                ComposerEdit::Unchanged
            }
            TerminalInput::Newline => {
                if !self.can_type(1) {
                    return ComposerEdit::AtCapacity;
                }
                self.shift_pastes(1);
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
                ComposerEdit::Changed
            }
            TerminalInput::Character(character) => {
                if !self.can_type(character.len_utf8()) {
                    return ComposerEdit::AtCapacity;
                }
                self.shift_pastes(character.len_utf8());
                if self.cursor == self.text.len() {
                    self.text.push(character);
                    self.cursor = self.text.len();
                    return ComposerEdit::Changed;
                }
                self.text.insert(self.cursor, character);
                let target = self.cursor + character.len_utf8();
                self.cursor = self
                    .text
                    .grapheme_indices(true)
                    .map(|(start, grapheme)| start + grapheme.len())
                    .find(|end| *end >= target)
                    .unwrap_or(self.text.len());
                ComposerEdit::Changed
            }
            TerminalInput::Backspace if self.cursor > 0 => {
                let start = previous_boundary(&self.text, self.cursor);
                self.remove_range(start, self.cursor);
                ComposerEdit::Changed
            }
            TerminalInput::Delete if self.cursor < self.text.len() => {
                let end = next_boundary(&self.text, self.cursor);
                self.remove_range(self.cursor, end);
                ComposerEdit::Changed
            }
            TerminalInput::Left if self.cursor > 0 => {
                self.cursor = previous_boundary(&self.text, self.cursor);
                ComposerEdit::Changed
            }
            TerminalInput::Right if self.cursor < self.text.len() => {
                self.cursor = next_boundary(&self.text, self.cursor);
                ComposerEdit::Changed
            }
            TerminalInput::WordLeft if self.cursor > 0 => {
                self.cursor = previous_word_boundary(&self.text, self.cursor);
                ComposerEdit::Changed
            }
            TerminalInput::WordRight if self.cursor < self.text.len() => {
                self.cursor = next_word_boundary(&self.text, self.cursor);
                ComposerEdit::Changed
            }
            TerminalInput::BackspaceWord if self.cursor > 0 => {
                let start = previous_word_boundary(&self.text, self.cursor);
                self.remove_range(start, self.cursor);
                ComposerEdit::Changed
            }
            TerminalInput::Up => self.move_vertical(true),
            TerminalInput::Down => self.move_vertical(false),
            TerminalInput::Home if self.cursor > 0 => {
                self.cursor = 0;
                ComposerEdit::Changed
            }
            TerminalInput::End if self.cursor < self.text.len() => {
                self.cursor = self.text.len();
                ComposerEdit::Changed
            }
            _ => ComposerEdit::Unchanged,
        }
    }

    fn can_type(&self, bytes: usize) -> bool {
        bytes <= self.input_capacity()
    }

    fn shift_pastes(&mut self, bytes: usize) {
        for paste in &mut self.pastes {
            if paste.range.start >= self.cursor {
                paste.range.start += bytes;
                paste.range.end += bytes;
            }
        }
    }

    fn remove_range(&mut self, mut start: usize, mut end: usize) {
        for paste in &self.pastes {
            if paste.range.start < end && paste.range.end > start {
                start = start.min(paste.range.start);
                end = end.max(paste.range.end);
            }
        }
        self.pastes.retain_mut(|paste| {
            if paste.range.start >= start && paste.range.end <= end {
                return false;
            }
            if paste.range.start >= end {
                paste.range.start -= end - start;
                paste.range.end -= end - start;
            }
            true
        });
        self.text.replace_range(start..end, "");
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .find(|offset| *offset >= start)
            .unwrap_or(self.text.len());
    }

    fn move_vertical(&mut self, up: bool) -> ComposerEdit {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        let target = if up {
            if start == 0 {
                return ComposerEdit::Unchanged;
            }
            let end = start - 1;
            let start = self.text[..end].rfind('\n').map_or(0, |index| index + 1);
            start..end
        } else {
            if end == self.text.len() {
                return ComposerEdit::Unchanged;
            }
            let start = end + 1;
            let end = self.text[start..]
                .find('\n')
                .map_or(self.text.len(), |index| start + index);
            start..end
        };
        let column = *self
            .vertical_grapheme_column
            .get_or_insert_with(|| self.text[start..self.cursor].graphemes(true).count());
        self.cursor = self.text[target.clone()]
            .grapheme_indices(true)
            .nth(column)
            .map_or(target.end, |(index, _)| target.start + index);
        ComposerEdit::Changed
    }
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text.grapheme_indices(true)
        .take_while(|(start, _)| *start < cursor)
        .last()
        .map_or(0, |(start, _)| start)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text.grapheme_indices(true)
        .find(|(start, _)| *start > cursor)
        .map_or(text.len(), |(start, _)| start)
}

fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .rev()
        .skip_while(|(_, grapheme)| grapheme.chars().next().is_some_and(char::is_whitespace))
        .take_while(|(_, grapheme)| !grapheme.chars().next().is_some_and(char::is_whitespace))
        .last()
        .map_or(0, |(offset, _)| offset)
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .grapheme_indices(true)
        .skip_while(|(_, grapheme)| !grapheme.chars().next().is_some_and(char::is_whitespace))
        .find(|(_, grapheme)| !grapheme.chars().next().is_some_and(char::is_whitespace))
        .map_or(text.len(), |(offset, _)| cursor + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_gestures_preserve_provenance_caret_capacity_and_aliases() {
        for text in ["a", "e\u{301}🙂", "\\"] {
            let mut composer = Composer::default();
            composer.insert_paste(text).unwrap();
            composer.apply(TerminalInput::Character('\\'));
            composer.apply(TerminalInput::Resize);
            assert_eq!(composer.apply(TerminalInput::Submit), ComposerEdit::Changed);
            assert_eq!(composer.text(), format!("{text}\n"));
            composer.apply(TerminalInput::Character('b'));
            assert_eq!(
                composer.apply(TerminalInput::Submit),
                ComposerEdit::Unchanged
            );
            assert_eq!(composer.text(), format!("{text}\nb"));
        }
        let mut composer = Composer::default();
        composer.insert_paste("a🙂b").unwrap();
        composer.apply(TerminalInput::Left);
        composer.apply(TerminalInput::Character('\\'));
        assert_eq!(composer.apply(TerminalInput::Submit), ComposerEdit::Changed);
        assert_eq!(composer.text(), "a🙂\nb");
        assert_eq!(composer.cursor_byte_offset(), "a🙂\n".len());
        for action in [
            TerminalInput::Left,
            TerminalInput::Backspace,
            TerminalInput::Tab,
            TerminalInput::Escape,
            TerminalInput::AgentNext,
            TerminalInput::ClipboardPaste,
            TerminalInput::LineRejected,
            TerminalInput::LineContinued,
            TerminalInput::IgnoredKey,
        ] {
            composer.clear();
            composer.apply(TerminalInput::Character('\\'));
            composer.observe_input(action);
            assert_eq!(
                composer.apply(TerminalInput::Submit),
                ComposerEdit::Unchanged,
                "{action:?}"
            );
            assert_eq!(composer.text(), "\\");
        }
        for paste in ["", "pasted\\"] {
            composer.clear();
            composer.apply(TerminalInput::Character('\\'));
            composer.insert_paste(paste).unwrap();
            assert_eq!(
                composer.apply(TerminalInput::Submit),
                ComposerEdit::Unchanged
            );
        }
        composer.remember_and_clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(
            composer.apply(TerminalInput::Submit),
            ComposerEdit::Unchanged
        );
        composer.clear();
        composer.apply(TerminalInput::Character('\\'));
        composer.apply(TerminalInput::Character('\\'));
        assert_eq!(composer.apply(TerminalInput::Submit), ComposerEdit::Changed);
        assert_eq!(composer.text(), "\\\n");
        composer.clear();
        composer
            .insert_literal(&"x".repeat(MAX_DRAFT_BYTES - 1))
            .unwrap();
        composer.apply(TerminalInput::Character('\\'));
        assert_eq!(composer.apply(TerminalInput::Submit), ComposerEdit::Changed);
        assert_eq!(composer.text().len(), MAX_DRAFT_BYTES);
        assert!(composer.text().ends_with('\n'));
        for paste_bytes in [MAX_DRAFT_BYTES + 1, MAX_PASTE_BYTES - 1] {
            composer.clear();
            let payload = format!("{}\\", "p".repeat(paste_bytes - 1));
            composer.insert_paste(&payload).unwrap();
            composer
                .attach_image(arany::test_support::test_image())
                .unwrap();
            assert_eq!(
                composer.apply(TerminalInput::Submit),
                ComposerEdit::Unchanged
            );
            assert_eq!(
                composer.apply(TerminalInput::Character('\\')),
                ComposerEdit::Changed
            );
            let pastes = composer.pastes.clone();
            assert_eq!(composer.apply(TerminalInput::Submit), ComposerEdit::Changed);
            assert_eq!(composer.text(), format!("{payload}\n"));
            assert_eq!(composer.pastes, pastes);
            assert_eq!(composer.images().len(), 1);
            composer.apply(TerminalInput::Left);
            composer.apply(TerminalInput::Home);
            assert_eq!(
                composer.apply(TerminalInput::ModifiedEnter(NewlineShortcut::ShiftEnter)),
                if paste_bytes == MAX_PASTE_BYTES - 1 {
                    ComposerEdit::AtCapacity
                } else {
                    ComposerEdit::Changed
                }
            );
            let paste = &composer.pastes[0];
            assert_eq!(&composer.text()[paste.range.clone()], payload);
            assert_eq!(composer.images().len(), 1);
        }
        composer.clear();
        composer.apply(TerminalInput::Character('\\'));
        let mut restored = Composer::default();
        restored
            .attach_image(arany::test_support::test_image())
            .unwrap();
        assert!(composer.restore_draft(restored.take_draft()));
        assert_eq!(
            composer.apply(TerminalInput::Submit),
            ComposerEdit::Unchanged
        );
        assert_eq!(composer.text(), "\\");
        assert_eq!(composer.images().len(), 1);
        composer.clear();
        let payload = format!("\u{301}{}\\", "p".repeat(1001));
        composer.insert_paste(&payload).unwrap();
        composer.apply(TerminalInput::Home);
        composer.apply(TerminalInput::Character('\\'));
        assert_eq!(
            composer.apply(TerminalInput::Submit),
            ComposerEdit::Unchanged
        );
        assert_eq!(composer.text(), format!("\\{payload}"));
        assert_eq!(&composer.text()[composer.pastes[0].range.clone()], payload);
        for selected in NewlineShortcut::ALL {
            let mut composer = Composer::default();
            composer.set_newline_shortcut(selected);
            assert_eq!(composer.newline_hint(), "\\ then Enter");
            for received in NewlineShortcut::ALL {
                composer.clear();
                composer.apply(TerminalInput::Character('\\'));
                let enabled = received == selected || received == NewlineShortcut::ShiftEnter;
                assert_eq!(
                    composer.apply(TerminalInput::ModifiedEnter(received)),
                    if enabled {
                        ComposerEdit::Changed
                    } else {
                        ComposerEdit::Unchanged
                    }
                );
                assert_eq!(composer.text(), if enabled { "\\\n" } else { "\\" });
                assert_eq!(
                    composer.apply(TerminalInput::Submit),
                    ComposerEdit::Unchanged
                );
            }
            composer.clear();
            composer.apply(TerminalInput::Newline);
            assert_eq!(composer.text(), "\n");
            assert_eq!(composer.newline_hint(), selected.label());
        }
    }

    #[test]
    fn rewind_restored_prompts_remain_objective_data_and_clear_is_recallable() {
        for prompt in [
            "ordinary prompt",
            "/rewind",
            "/permissions",
            "//literal",
            "first\nsecond",
        ] {
            let mut composer = Composer::default();
            assert_eq!(composer.restore_prompt(prompt), Ok(ComposerEdit::Changed));
            assert_eq!(composer.submission(), Ok(Submission::Objective(prompt)));
            let draft = composer.text().to_owned();
            composer.remember_and_clear();
            assert!(composer.is_empty());
            composer.apply(TerminalInput::Up);
            assert_eq!(composer.text(), draft);
            assert_eq!(composer.submission(), Ok(Submission::Objective(prompt)));
        }
        let mut composer = Composer::default();
        assert_eq!(
            composer.restore_prompt(&format!("/{}", "x".repeat(MAX_DRAFT_BYTES - 1))),
            Ok(ComposerEdit::AtCapacity)
        );
        assert!(composer.is_empty());
    }

    #[test]
    fn composer_navigation_editing_and_capacity_keep_whole_graphemes() {
        let mut composer = Composer::default();
        for character in "a🙂e\u{301}中".chars() {
            assert_eq!(
                composer.apply(TerminalInput::Character(character)),
                ComposerEdit::Changed
            );
        }
        assert_eq!(composer.text(), "a🙂e\u{301}中");
        composer.apply(TerminalInput::Left);
        composer.apply(TerminalInput::Backspace);
        assert_eq!(composer.text(), "a🙂中");
        assert_eq!(composer.cursor_byte_offset(), "a🙂".len());
        composer.apply(TerminalInput::Home);
        composer.apply(TerminalInput::Delete);
        assert_eq!(composer.text(), "🙂中");
        composer.apply(TerminalInput::Right);
        composer.apply(TerminalInput::Character('x'));
        assert_eq!(composer.text(), "🙂x中");
        composer.apply(TerminalInput::End);
        assert_eq!(composer.cursor_byte_offset(), composer.text().len());
        assert_eq!(composer.take(), "🙂x中");
        assert!(composer.is_empty());
        assert_eq!(composer.cursor_byte_offset(), 0);

        for (text, joined, cursor, remaining) in [
            ("a\n\u{301}b", "a\u{301}b", "a\u{301}".len(), "b"),
            ("🇦x🇧", "🇦🇧", "🇦🇧".len(), ""),
            ("🇦x🇧🇨🇩", "🇦🇧🇨🇩", "🇦🇧".len(), "🇨🇩"),
            ("\u{1100}x\u{1161}", "\u{1100}\u{1161}", 6, ""),
            ("👩\n\u{200d}💻", "👩\u{200d}💻", "👩\u{200d}💻".len(), ""),
        ] {
            for key in [TerminalInput::Backspace, TerminalInput::Delete] {
                composer.clear();
                for character in text.chars() {
                    composer.apply(if character == '\n' {
                        TerminalInput::Newline
                    } else {
                        TerminalInput::Character(character)
                    });
                }
                composer.apply(TerminalInput::Home);
                composer.apply(TerminalInput::Right);
                if key == TerminalInput::Backspace {
                    composer.apply(TerminalInput::Right);
                }
                assert_eq!(composer.apply(key), ComposerEdit::Changed);
                assert_eq!(composer.text(), joined, "{key:?}: {text:?}");
                assert_eq!(composer.cursor_byte_offset(), cursor, "{key:?}: {text:?}");
                assert_eq!(
                    composer.apply(TerminalInput::Backspace),
                    ComposerEdit::Changed
                );
                assert_eq!(composer.text(), remaining, "{key:?}: follow-up deletion");
                assert_eq!(composer.cursor_byte_offset(), 0);
            }
        }

        for (text, cursor, left, right) in [
            ("", 0, 0, 0),
            ("   ", 2, 0, 3),
            ("alpha beta", 0, 0, 6),
            ("alpha beta", 3, 0, 6),
            ("alpha beta", 6, 0, 10),
            ("alpha beta", 10, 6, 10),
            ("alpha  beta  ", 6, 0, 7),
            ("/model gpt-5.4", 14, 7, 14),
            ("src/cli.rs next", 4, 0, 11),
            ("e\u{301}\u{a0}👩\u{200d}💻\n中", 16, 5, 17),
            ("e\u{301}\u{a0}👩\u{200d}💻\n中", 17, 5, 20),
            (" \u{301}next", 3, 0, 7),
        ] {
            for (input, target) in [
                (TerminalInput::WordLeft, left),
                (TerminalInput::WordRight, right),
                (TerminalInput::BackspaceWord, left),
            ] {
                composer.clear();
                for character in text.chars() {
                    composer.apply(if character == '\n' {
                        TerminalInput::Newline
                    } else {
                        TerminalInput::Character(character)
                    });
                }
                composer.apply(TerminalInput::Home);
                while composer.cursor_byte_offset() < cursor {
                    composer.apply(TerminalInput::Right);
                }
                assert_eq!(composer.cursor_byte_offset(), cursor, "fixture: {text:?}");
                assert_eq!(
                    composer.apply(input),
                    if target == cursor {
                        ComposerEdit::Unchanged
                    } else {
                        ComposerEdit::Changed
                    },
                    "{input:?}: {text:?} at {cursor}"
                );
                let expected = if input == TerminalInput::BackspaceWord {
                    format!("{}{}", &text[..left], &text[cursor..])
                } else {
                    text.to_owned()
                };
                assert_eq!(composer.text(), expected, "{input:?}: {text:?}");
                assert_eq!(composer.cursor_byte_offset(), target, "{input:?}: {text:?}");
            }
        }

        composer.clear();
        for character in "a x\n\u{301}b".chars() {
            composer.apply(if character == '\n' {
                TerminalInput::Newline
            } else {
                TerminalInput::Character(character)
            });
        }
        composer.apply(TerminalInput::Home);
        composer.apply(TerminalInput::Right);
        composer.apply(TerminalInput::Right);
        composer.apply(TerminalInput::Right);
        composer.apply(TerminalInput::Right);
        assert_eq!(composer.cursor_byte_offset(), 4);
        assert_eq!(
            composer.apply(TerminalInput::BackspaceWord),
            ComposerEdit::Changed
        );
        assert_eq!(composer.text(), "a \u{301}b");
        assert_eq!(composer.cursor_byte_offset(), 4);
        assert_eq!(
            composer.apply(TerminalInput::Backspace),
            ComposerEdit::Changed
        );
        assert_eq!(composer.text(), "ab");
        assert_eq!(composer.cursor_byte_offset(), 1);

        composer.text = "x".repeat(MAX_DRAFT_BYTES);
        composer.cursor = MAX_DRAFT_BYTES;
        assert_eq!(
            composer.apply(TerminalInput::Character('!')),
            ComposerEdit::AtCapacity
        );
        assert_eq!(composer.text().len(), MAX_DRAFT_BYTES);
        assert_eq!(
            composer.apply(TerminalInput::Newline),
            ComposerEdit::AtCapacity
        );
        assert_eq!(
            composer.apply(TerminalInput::Character('\n')),
            ComposerEdit::Unchanged
        );
        composer.clear();
        composer.insert_paste("alpha\n中").unwrap();
        composer.apply(TerminalInput::Left);
        let caret = composer.cursor_byte_offset();
        let image = arany::test_support::test_image();
        let mut child = Composer::text_only();
        child.restore_prompt("/literal").unwrap();
        assert!(child.attach_image(image.clone()).is_err());
        assert_eq!(child.text(), "/literal");
        assert!(child.images().is_empty());
        for _ in 0..MAX_MESSAGE_IMAGES {
            composer.attach_image(image.clone()).unwrap();
        }
        assert!(composer.attach_image(image.clone()).is_err());
        assert_eq!(composer.images().len(), MAX_MESSAGE_IMAGES);
        assert_eq!(composer.text(), "alpha\n中");
        assert_eq!(composer.cursor_byte_offset(), caret);
        assert_eq!(composer.take(), "alpha\n中");
        assert!(!composer.is_empty(), "image-only draft remains actionable");
        assert_eq!(composer.submission(), Ok(Submission::Objective("")));
        composer.insert_paste("/help").unwrap();
        assert_eq!(
            composer.submission(),
            Ok(Submission::Command {
                command: super::super::commands::InteractiveCommand::Help,
                argument: None
            })
        );
        assert_eq!(composer.take(), "/help");
        assert_eq!(
            composer.take_images(),
            vec![image.clone(); MAX_MESSAGE_IMAGES]
        );
        assert!(composer.is_empty());
        composer.attach_image(image).unwrap();
        composer.clear();
        assert!(
            composer.is_empty(),
            "explicit clear discards text and images"
        );
    }

    #[test]
    fn multiline_editing_preserves_graphemes_and_vertical_column() {
        let mut composer = Composer::default();
        for character in "ab🙂".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        composer.apply(TerminalInput::Newline);
        composer.apply(TerminalInput::Character('中'));
        composer.apply(TerminalInput::Newline);
        for character in "xy🙂".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert_eq!(composer.text(), "ab🙂\n中\nxy🙂");
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.cursor_byte_offset(), "ab🙂\n中".len());
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.cursor_byte_offset(), "ab🙂".len());
        composer.apply(TerminalInput::Down);
        assert_eq!(composer.cursor_byte_offset(), "ab🙂\n中".len());
        composer.apply(TerminalInput::Down);
        assert_eq!(composer.cursor_byte_offset(), composer.text().len());
        composer.apply(TerminalInput::Left);
        assert_eq!(composer.cursor_byte_offset(), "ab🙂\n中\nxy".len());
        composer.apply(TerminalInput::Up);
        composer.apply(TerminalInput::Backspace);
        assert_eq!(composer.text(), "ab🙂\n\nxy🙂");
        composer.clear();
        for _ in 0..5 {
            composer.apply(TerminalInput::Newline);
        }
        assert_eq!(composer.text(), "\n".repeat(5));

        for (before, cursor, paste, after, caret) in [
            ("ab", 1, "first\r\nsecond\r\t", "afirst\nsecond\n\tb", 15),
            ("ab", 1, "\u{301}", "a\u{301}b", 3),
            ("\u{301}b", 0, "a", "a\u{301}b", 3),
            ("🇧", 0, "🇦", "🇦🇧", 8),
            ("👩👩", 4, "\u{200d}", "👩\u{200d}👩", 11),
            ("", 0, "/exit\n/setup", "/exit\n/setup", 12),
        ] {
            composer.clear();
            composer.insert_paste(before).unwrap();
            composer.cursor = cursor;
            assert_eq!(composer.insert_paste(paste), Ok(ComposerEdit::Changed));
            assert_eq!(composer.text(), after, "paste normalization or insertion");
            assert_eq!(composer.cursor_byte_offset(), caret, "paste grapheme caret");
            assert!(composer.completion_menu().is_none());
        }
        for paste in ["\0", "\x1b[31m", "\x03", "\u{85}"] {
            let before = composer.text().to_owned();
            let cursor = composer.cursor_byte_offset();
            assert!(composer.insert_paste(paste).is_err());
            assert_eq!(composer.text(), before);
            assert_eq!(composer.cursor_byte_offset(), cursor);
        }
        assert_eq!(composer.insert_paste(""), Ok(ComposerEdit::Unchanged));
        composer.clear();
        assert_eq!(
            composer.insert_paste(&"x".repeat(MAX_PASTE_BYTES)),
            Ok(ComposerEdit::Changed)
        );
        composer.apply(TerminalInput::Left);
        let cursor = composer.cursor_byte_offset();
        assert_eq!(composer.insert_paste("y"), Ok(ComposerEdit::AtCapacity));
        assert_eq!(composer.cursor_byte_offset(), cursor);
        assert_eq!(composer.text(), "x".repeat(MAX_PASTE_BYTES));
        composer.clear();
        assert_eq!(
            composer.insert_paste(&"\r\n".repeat(MAX_PASTE_BYTES / 2 + 1)),
            Ok(ComposerEdit::AtCapacity)
        );
        assert!(composer.text().is_empty());
        composer.insert_paste("prefix").unwrap();
        assert_eq!(
            composer.insert_paste(&"\r\n".repeat(MAX_PASTE_BYTES / 2)),
            Ok(ComposerEdit::AtCapacity)
        );
        assert_eq!(composer.text(), "prefix");
    }

    #[test]
    fn withdrawn_notes_preserve_typed_and_pasted_provenance_after_prior_delivery() {
        let mut composer = Composer::default();
        composer.insert_literal("already delivered").unwrap();
        let mut offered = vec![composer.take_draft()];
        let typed = format!("{} @src/lib.rs", "typed ".repeat(200));
        composer.insert_literal(&typed).unwrap();
        offered.push(composer.take_draft());
        let restored = ComposerDraft::from_steering_note(typed.clone(), &offered);
        assert_eq!(restored.editable_text(), typed);
        assert!(restored.pastes.is_empty());
        let pasted = format!("@private.txt {}", "paste ".repeat(200));
        composer.insert_paste(&pasted).unwrap();
        composer.insert_literal(" @visible.txt").unwrap();
        let second = composer.text().to_owned();
        offered.push(composer.take_draft());
        let combined = format!("{typed}\n\n{second}");
        let restored = ComposerDraft::from_steering_note(combined.clone(), &offered);
        assert_eq!(restored.text, combined);
        assert_eq!(
            arany::mention_paths(&restored.editable_text()).unwrap(),
            vec![
                std::path::PathBuf::from("src/lib.rs"),
                std::path::PathBuf::from("visible.txt")
            ]
        );
        assert_eq!(restored.pastes.len(), 1);
        offered.push(offered.last().unwrap().clone());
        let duplicated =
            ComposerDraft::from_steering_note(format!("{combined}\n\n{second}"), &offered);
        assert_eq!(duplicated.pastes.len(), 2);
        assert_eq!(duplicated.pastes[0].suffix, 1);
        assert_eq!(duplicated.pastes[1].suffix, 2);
        assert!(composer.restore_draft(restored));
        assert!(composer.has_text_pastes());
        composer.clear();
        composer.insert_literal("//literal").unwrap();
        offered.push(composer.take_draft());
        let restored = ComposerDraft::from_steering_note("/literal".into(), &offered);
        assert_eq!(restored.editable_text(), "//literal");
        assert!(restored.pastes.is_empty());
        assert!(composer.restore_draft(restored));
        assert_eq!(composer.submission(), Ok(Submission::Objective("/literal")));
        composer.clear();
        composer.insert_literal("//literal ").unwrap();
        composer.insert_paste(&pasted).unwrap();
        composer.insert_literal(" @visible.txt").unwrap();
        let escaped = composer.take_draft();
        let objective = escaped.text[1..].to_owned();
        offered.push(escaped);
        for note in [objective.clone(), format!("/literal\n\n{objective}")] {
            let restored = ComposerDraft::from_steering_note(note.clone(), &offered);
            assert_eq!(restored.pastes.len(), 1);
            assert_eq!(restored.text[restored.pastes[0].range.clone()], pasted);
            assert_eq!(
                arany::mention_paths(&restored.editable_text()).unwrap(),
                vec![std::path::PathBuf::from("visible.txt")]
            );
            assert!(composer.restore_draft(restored));
            assert_eq!(composer.submission(), Ok(Submission::Objective(&note)));
            composer.clear();
        }
    }

    #[test]
    fn literal_restoration_keeps_long_notes_editable_and_merge_limits_preserve_both() {
        let note = format!("{} @src/lib.rs", "typed ".repeat(300));
        let mut composer = Composer::default();
        composer.insert_literal(&note).unwrap();
        assert_eq!(composer.editable_text(), note);
        assert!(!composer.has_text_pastes());
        assert_eq!(
            arany::mention_paths(&composer.editable_text()).unwrap(),
            vec![std::path::PathBuf::from("src/lib.rs")]
        );

        for newer in [2191, 2192] {
            let mut composer = Composer::default();
            composer.insert_literal(&"a".repeat(6000)).unwrap();
            let held = composer.take_draft();
            composer.insert_literal(&"b".repeat(newer)).unwrap();
            if newer == 2191 {
                assert!(composer.restore_draft(held));
                assert_eq!(composer.text().len(), MAX_DRAFT_BYTES);
                assert_eq!(composer.input_capacity(), 0);
            } else {
                assert!(!composer.restore_draft(held));
                assert_eq!(composer.text(), "b".repeat(newer));
                composer.clear();
                composer.apply(TerminalInput::Up);
                assert_eq!(composer.text(), "a".repeat(6000));
                assert!(!composer.has_text_pastes());
            }
        }
    }

    #[test]
    fn refused_restoration_preserves_every_waiting_payload_and_merge_labels() {
        let mut composer = Composer::default();
        composer.insert_paste(&"first ".repeat(200)).unwrap();
        let first = composer.take_draft();
        composer.insert_paste(&"later ".repeat(200)).unwrap();
        assert!(composer.restore_draft(first));
        assert_eq!(
            composer.text(),
            format!("{}\n{}", "first ".repeat(200), "later ".repeat(200))
        );
        assert_eq!(composer.pastes[0].label(), "[Pasted Content 1200 chars]");
        assert_eq!(composer.pastes[1].label(), "[Pasted Content 1200 chars] #2");
        composer.clear();
        composer.insert_paste(&"a".repeat(MAX_PASTE_BYTES)).unwrap();
        let held = composer.take_draft();
        composer.insert_paste(&"b".repeat(MAX_PASTE_BYTES)).unwrap();
        assert!(!composer.restore_draft(held));
        let mut note = Composer::default();
        note.insert_paste("waiting steering note").unwrap();
        assert!(!composer.restore_draft(note.take_draft()));
        let mut submitted = Composer::default();
        submitted.insert_paste(&"original ".repeat(1100)).unwrap();
        assert!(!composer.restore_draft(submitted.take_draft()));
        assert!(
            !composer.remember_and_clear(),
            "a waiting note is not replaceable recall"
        );
        assert_eq!(composer.text(), "b".repeat(MAX_PASTE_BYTES));
        composer.clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.text(), "a".repeat(MAX_PASTE_BYTES));
        composer.clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.text(), "original ".repeat(1100));
        composer.clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.text(), "waiting steering note");
    }

    #[test]
    fn small_pastes_share_the_typed_limit_while_chips_use_the_expanded_limit() {
        let mut composer = Composer::default();
        for _ in 0..8 {
            assert_eq!(
                composer.insert_paste(&"a".repeat(1000)),
                Ok(ComposerEdit::Changed)
            );
        }
        assert_eq!(
            composer.insert_paste(&"é".repeat(96)),
            Ok(ComposerEdit::Changed)
        );
        let editable = composer.text().to_owned();
        assert_eq!(editable.len(), MAX_DRAFT_BYTES);
        for text in ["a", "é", "\r\n"] {
            assert_eq!(composer.insert_paste(text), Ok(ComposerEdit::AtCapacity));
            assert_eq!(composer.text(), editable);
        }
        composer.apply(TerminalInput::Home);
        assert_eq!(
            composer.insert_paste(&"@hidden ".repeat(200)),
            Ok(ComposerEdit::Changed)
        );
        assert!(composer.has_text_pastes());
        assert!(
            arany::mention_paths(&composer.editable_text())
                .unwrap()
                .is_empty()
        );
        let draft = composer.take_draft();
        assert!(composer.restore_draft(draft.clone()));
        assert_eq!(composer.insert_paste("@file"), Ok(ComposerEdit::AtCapacity));
        assert_eq!(
            composer.apply(TerminalInput::Character('a')),
            ComposerEdit::AtCapacity
        );
        assert_eq!(composer.take_draft(), draft);
    }

    #[test]
    fn text_attachments_keep_exact_bytes_atomic_identity_and_literal_syntax() {
        for input in [
            Some(TerminalInput::Character('!')),
            Some(TerminalInput::Newline),
            None,
        ] {
            let mut composer = Composer::default();
            let payload = format!("\u{301}{} @private.txt", "paste ".repeat(200));
            composer.insert_paste(&payload).unwrap();
            composer.apply(TerminalInput::Home);
            composer.insert_paste("a").unwrap();
            assert_eq!(composer.cursor_byte_offset(), composer.text().len());
            let suffix = match input {
                Some(input) => {
                    assert_eq!(composer.apply(input), ComposerEdit::Changed);
                    if input == TerminalInput::Newline {
                        "\n"
                    } else {
                        "!"
                    }
                }
                None => {
                    composer.insert_paste("tail").unwrap();
                    "tail"
                }
            };
            assert_eq!(composer.text(), format!("a{payload}{suffix}"));
            assert_eq!(composer.text[composer.pastes[0].range.clone()], payload);
            assert_eq!(composer.editable_text(), format!("a {suffix}"));
            assert!(
                arany::mention_paths(&composer.editable_text())
                    .unwrap()
                    .is_empty()
            );
        }
        for text_only in [false, true] {
            for (input, characters, collapsed) in [
                ("a".repeat(1000), 1000, false),
                ("中".repeat(1001), 1001, true),
                ("e\u{301}".repeat(501), 1002, true),
                ("a\r\n".repeat(500), 1000, false),
                ("a\r".repeat(501), 1002, true),
            ] {
                let mut composer = if text_only {
                    Composer::text_only()
                } else {
                    Composer::default()
                };
                let collapsed = collapsed && !text_only;
                composer.insert_paste(&input).unwrap();
                let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
                assert_eq!(composer.text(), normalized);
                assert_eq!(composer.has_text_pastes(), collapsed);
                if !collapsed {
                    assert_eq!(composer.editable_text(), normalized);
                }
                if collapsed {
                    assert_eq!(
                        composer.display_text().0,
                        format!("[Pasted Content {characters} chars]")
                    );
                    composer.apply(TerminalInput::Left);
                    assert_eq!(composer.selected_paste().unwrap().1, normalized);
                    let cursor = composer.cursor_byte_offset();
                    assert!(composer.inspect_input(TerminalInput::Tab));
                    assert!(composer.inspect_input(TerminalInput::Character('x')));
                    assert!(composer.inspect_input(TerminalInput::Escape));
                    assert_eq!(composer.cursor_byte_offset(), cursor);
                    assert_eq!(composer.text(), normalized);
                    composer.apply(TerminalInput::Delete);
                    assert!(composer.is_empty());
                }
            }
        }
        let mut composer = Composer::default();
        composer.insert_paste("before ").unwrap();
        let payload = format!("/exit @private.txt {}", "text ".repeat(1840));
        composer.insert_paste(&payload).unwrap();
        composer
            .insert_paste(" after [Pasted Content 1001 chars]")
            .unwrap();
        assert_eq!(
            composer.editable_text(),
            "before   after [Pasted Content 1001 chars]"
        );
        assert_eq!(
            composer.submission(),
            Ok(Submission::Objective(composer.text()))
        );
        let exact = composer.text().to_owned();
        composer.remember_and_clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.text(), exact);
        assert!(composer.has_text_pastes());
        composer.apply(TerminalInput::Home);
        composer.insert_paste("/help ").unwrap();
        assert_eq!(composer.submission(), Err(CommandParseError::Attachments));

        for prefix in ["//help ", "///help ", "/help ", " //help ", "/", ""] {
            composer.clear();
            composer.insert_literal(prefix).unwrap();
            let payload = format!("//hidden @private.txt {}", "paste ".repeat(200));
            composer.insert_paste(&payload).unwrap();
            if prefix.starts_with("//") {
                assert_eq!(
                    composer.submission(),
                    Ok(Submission::Objective(&composer.text()[1..]))
                );
            } else if prefix.is_empty() {
                assert_eq!(composer.submission(), Ok(Submission::Objective(&payload)));
            } else {
                assert_eq!(composer.submission(), Err(CommandParseError::Attachments));
            }
            assert!(
                arany::mention_paths(&composer.editable_text())
                    .unwrap()
                    .is_empty()
            );
        }

        composer.clear();
        for _ in 0..20 {
            composer.insert_paste(&"a".repeat(1001)).unwrap();
        }
        assert_eq!(composer.pastes.len(), 20);
        assert!(
            composer
                .display_text()
                .0
                .ends_with("[Pasted Content 1001 chars] #20")
        );
        composer.apply(TerminalInput::Home);
        composer.apply(TerminalInput::Delete);
        composer.insert_paste(&"a".repeat(1001)).unwrap();
        assert_eq!(
            composer.pastes[0].label(),
            "[Pasted Content 1001 chars] #21"
        );
        composer.clear();
        composer.insert_paste(&"a".repeat(1001)).unwrap();
        assert_eq!(composer.pastes[0].label(), "[Pasted Content 1001 chars]");
        let mut child = Composer::text_only();
        assert_eq!(
            child.insert_paste(&"a".repeat(MAX_DRAFT_BYTES + 1)),
            Ok(ComposerEdit::AtCapacity)
        );
        assert!(child.is_empty());
    }

    #[test]
    fn file_completion_preserves_draft_and_quotes_selected_paths() {
        for (prefix, candidate, expected) in [
            ("Edit @REA", "README.md", "Edit @\"README.md\" "),
            (
                "Edit @docs/",
                "docs/hello world.md",
                "Edit @\"docs/hello world.md\" ",
            ),
            ("Edit @", "中 é.md", "Edit @\"中 é.md\" "),
            ("Edit @d", "docs/", "Edit @docs/"),
            (
                "Inspect @hello",
                "docs/hello world/",
                "Inspect @\"docs/hello world/",
            ),
            (
                "Edit @hwd",
                "docs/hello world.md",
                "Edit @\"docs/hello world.md\" ",
            ),
        ] {
            for accept in [TerminalInput::Tab, TerminalInput::Submit] {
                let mut composer = Composer::default();
                for character in prefix.chars() {
                    composer.apply(TerminalInput::Character(character));
                }
                let query = composer.file_query();
                assert!(query.is_some());
                composer.set_file_candidates(query, vec![candidate.into()]);
                let cursor = composer.cursor_byte_offset();
                composer.set_approval_mode(Some(arany::ApprovalMode::Request));
                assert_eq!(composer.text(), prefix);
                assert_eq!(composer.cursor_byte_offset(), cursor);
                assert_eq!(composer.apply(accept), ComposerEdit::Changed);
                let expected = if candidate.ends_with('/') && accept == TerminalInput::Submit {
                    format!(
                        "{}@{} ",
                        prefix.rsplit_once('@').unwrap().0,
                        serde_json::to_string(candidate).unwrap()
                    )
                } else {
                    expected.to_owned()
                };
                assert_eq!(composer.text(), expected);
                assert_eq!(composer.cursor_byte_offset(), expected.len());
                assert!(composer.file_menu().is_none());
            }
        }
        for text in [
            "user@example.com",
            "/model @",
            "Edit @@",
            "Edit @README.md done",
        ] {
            let mut composer = Composer::default();
            for character in text.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            assert!(composer.file_query().is_none(), "{text}");
        }
    }

    #[test]
    fn slash_skills_are_distinct_bounded_invocations_with_command_precedence() {
        let mut composer = Composer::default();
        composer.insert_paste("/").unwrap();
        let cursor = composer.cursor_byte_offset();
        assert!(
            composer
                .completion_header()
                .unwrap()
                .starts_with("[Commands]")
        );
        composer.apply(TerminalInput::Right);
        assert_eq!(composer.completion_rows().unwrap().0, Vec::<String>::new());
        assert!(
            composer
                .command_preview()
                .unwrap()
                .status
                .starts_with("No Skills")
        );
        composer.apply(TerminalInput::Submit);
        assert_eq!(composer.text(), "/");
        assert_eq!(composer.cursor_byte_offset(), cursor);
        composer.set_runtime_skills(&["review".into()]);
        composer.apply(TerminalInput::Character('r'));
        assert_eq!(composer.completion_rows().unwrap().0, ["Skill /review"]);
        composer.apply(TerminalInput::Left);
        assert_eq!(composer.text(), "/r");
        assert_eq!(
            composer.completion_rows().unwrap().0,
            ["Cmd /rewind", "Cmd /resume", "Cmd /rename"]
        );
        composer.apply(TerminalInput::Right);
        composer.apply(TerminalInput::Escape);
        composer.apply(TerminalInput::Left);
        assert_eq!(composer.cursor_byte_offset(), 1);
        assert!(composer.completion_menu().is_none());
        composer.clear();
        composer.set_completion_layout(CompletionLayout::Combined);
        composer.set_runtime_skills(&[
            "review".into(),
            "help".into(),
            "review".into(),
            "../unsafe".into(),
            "bad\u{1b}name".into(),
        ]);
        for accept in [TerminalInput::Tab, TerminalInput::Submit] {
            composer.clear();
            composer.insert_paste("/rev").unwrap();
            assert_eq!(
                composer.completion_rows().unwrap(),
                (vec!["Skill /review".into()], 0)
            );
            assert_eq!(
                composer.apply(accept),
                if accept == TerminalInput::Tab {
                    ComposerEdit::Changed
                } else {
                    ComposerEdit::Unchanged
                }
            );
            assert_eq!(
                composer.text(),
                if accept == TerminalInput::Tab {
                    "/review "
                } else {
                    "/review"
                }
            );
            assert_eq!(composer.cursor_byte_offset(), composer.text().len());
            assert_eq!(
                composer.submission(),
                Ok(Submission::Skill {
                    name: "review",
                    task: ""
                })
            );
            assert_eq!(
                composer.apply(TerminalInput::Submit),
                ComposerEdit::Unchanged
            );
        }
        composer.clear();
        composer.insert_paste("/he").unwrap();
        assert_eq!(composer.completion_rows().unwrap().0, ["Cmd /help"]);
        composer.apply(TerminalInput::Down);
        composer.apply(TerminalInput::Submit);
        assert_eq!(composer.text(), "/help");
        composer.clear();
        composer.insert_paste("/help").unwrap();
        assert!(composer.completion_menu().is_none());
        assert!(matches!(
            composer.submission(),
            Ok(Submission::Command { .. })
        ));
        composer.clear();
        composer.insert_paste("/revi").unwrap();
        composer.apply(TerminalInput::Escape);
        assert_eq!(composer.text(), "/revi");
        assert!(composer.completion_menu().is_none());
        composer.apply(TerminalInput::Backspace);
        assert!(composer.completion_menu().is_some());
        composer.apply(TerminalInput::Left);
        assert!(composer.completion_menu().is_none());
        composer.set_runtime_skills(&[]);
        composer.apply(TerminalInput::End);
        assert!(composer.completion_menu().is_none());
        composer.set_runtime_skills(&(0..80).map(|i| format!("skill-{i}")).collect::<Vec<_>>());
        assert_eq!(composer.skills.len(), arany::MAX_SKILLS);
        composer.clear();
        composer.insert_paste("/skill-1").unwrap();
        assert!(composer.completion_menu().is_some());
        composer.apply(TerminalInput::Newline);
        assert!(composer.completion_menu().is_none());
    }

    #[test]
    fn tab_completion_changes_only_real_draft_bytes() {
        let mut composer = Composer::default();
        composer.set_completion_layout(CompletionLayout::Combined);
        for (prefix, completed) in [("/resum", "/resume "), ("/prov", "/provider ")] {
            for accept in [TerminalInput::Tab, TerminalInput::Submit] {
                composer.clear();
                composer.insert_paste(prefix).expect("command prefix");
                let (names, selected) = composer
                    .completion_menu()
                    .expect("unique choice stays visible");
                assert_eq!(names[selected], completed.trim().trim_start_matches('/'));
                assert_eq!(
                    composer.apply(accept),
                    if accept == TerminalInput::Tab {
                        ComposerEdit::Changed
                    } else {
                        ComposerEdit::Unchanged
                    }
                );
                assert_eq!(
                    composer.text(),
                    if accept == TerminalInput::Tab {
                        completed
                    } else {
                        completed.trim()
                    }
                );
                assert!(composer.completion_menu().is_none());
                assert_eq!(
                    composer.apply(TerminalInput::Submit),
                    ComposerEdit::Unchanged
                );
            }
        }
        for (prefix, navigation, expected) in [
            ("/s", TerminalInput::Down, "status"),
            ("/s", TerminalInput::Up, "settings"),
            ("/", TerminalInput::Up, "exit"),
        ] {
            for accept in [TerminalInput::Tab, TerminalInput::Submit] {
                composer.clear();
                for character in prefix.chars() {
                    composer.apply(TerminalInput::Character(character));
                }
                let cursor = composer.cursor_byte_offset();
                assert_eq!(composer.apply(navigation), ComposerEdit::Changed);
                let (names, selected) = composer.completion_menu().expect("ambiguous choices");
                assert_eq!(names[selected], expected);
                assert_eq!(composer.text(), prefix);
                assert_eq!(composer.cursor_byte_offset(), cursor);
                assert_eq!(
                    composer.apply(accept),
                    if accept == TerminalInput::Tab {
                        ComposerEdit::Changed
                    } else {
                        ComposerEdit::Unchanged
                    }
                );
                assert_eq!(composer.text(), format!("/{expected}"));
                assert!(composer.completion_menu().is_none());
                assert_eq!(
                    composer.apply(TerminalInput::Submit),
                    ComposerEdit::Unchanged
                );
            }
        }
        composer.clear();
        for character in "/s".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        composer.apply(TerminalInput::Down);
        let cursor = composer.cursor_byte_offset();
        assert!(composer.has_open_menu());
        assert_eq!(composer.apply(TerminalInput::Escape), ComposerEdit::Changed);
        assert!(composer.completion_menu().is_none());
        assert!(!composer.has_open_menu());
        assert_eq!(
            composer.apply(TerminalInput::Escape),
            ComposerEdit::Unchanged,
            "Escape with no menu changes nothing"
        );
        assert_eq!(composer.text(), "/s");
        assert_eq!(composer.cursor_byte_offset(), cursor);
        assert_eq!(composer.apply(TerminalInput::Down), ComposerEdit::Unchanged);
        composer.apply(TerminalInput::Character('t'));
        composer.apply(TerminalInput::Backspace);
        let (names, selected) = composer
            .completion_menu()
            .expect("edited prefix reopens choices");
        assert_eq!(names, ["setup", "status", "settings"]);
        assert_eq!(selected, 0);
        composer.apply(TerminalInput::Left);
        assert!(composer.completion_menu().is_none());
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
        composer.apply(TerminalInput::End);
        assert!(composer.completion_menu().is_some());
        composer.apply(TerminalInput::Newline);
        assert!(composer.completion_menu().is_none());
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
        assert_eq!(composer.text(), "/s\n");
        assert_eq!(composer.apply(TerminalInput::Up), ComposerEdit::Changed);
        assert_eq!(composer.cursor_byte_offset(), 0);
        composer.clear();
        for character in "/model".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(composer.completion_menu().is_none());
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
        assert_eq!(composer.take(), "/model ");
        for character in "/prov".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
        assert_eq!(composer.text(), "/provider ");
        assert_eq!(completion_suffix(composer.text(), &composer.choices), None);
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
        assert_eq!(composer.text(), "/provider ");
        composer.apply(TerminalInput::Character('o'));
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
        assert_eq!(composer.text(), "/provider openai");
        composer.apply(TerminalInput::Home);
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
        assert_eq!(composer.text(), "/provider openai");

        composer.clear();
        composer.set_completion_selection(Some("openai"), Some("gpt-5.4"), None);
        composer.set_model_catalog(
            "openai",
            &[ModelEntry {
                id: "gpt-5.4".into(),
                runnable: true,
                efforts: vec![],
            }],
            false,
        );
        for character in "/model gpt-5".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
        assert_eq!(composer.text(), "/model gpt-5.4");
        composer.set_completion_selection(Some("anthropic"), None, None);
        composer.clear();
        for character in "/model gpt-5".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
        assert_eq!(composer.text(), "/model gpt-5");

        for profile in ["chatgpt", "openai", "anthropic"] {
            composer.clear();
            composer.set_completion_selection(Some(profile), Some("pending-model"), None);
            composer.set_model_catalog(
                profile,
                &[ModelEntry {
                    id: "pending-model".into(),
                    runnable: false,
                    efforts: vec![],
                }],
                false,
            );
            for character in "/model pending-".chars() {
                composer.apply(TerminalInput::Character(character));
            }
            assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
            assert_eq!(composer.take(), "/model pending-model");
            for character in "/model pending-model hig".chars() {
                composer.apply(TerminalInput::Character(character));
            }
            assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
            assert_eq!(composer.take(), "/model pending-model high");
            for character in "/model pending-model def".chars() {
                composer.apply(TerminalInput::Character(character));
            }
            assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Unchanged);
            assert_eq!(composer.text(), "/model pending-model def");
        }

        let first_account = uuid::Uuid::now_v7();
        let second_account = uuid::Uuid::now_v7();
        for profile in ["openai", "anthropic", "chatgpt"] {
            for (previous, next) in [
                (Some(first_account), Some(second_account)),
                (Some(first_account), None),
                (None, Some(first_account)),
            ] {
                let mut defaults = arany::SessionDefaults {
                    provider: Some(profile.into()),
                    model: Some("account-model".into()),
                    account_id: previous,
                    ..Default::default()
                };
                composer.clear();
                composer.set_completion_selection(
                    defaults.provider.as_deref(),
                    defaults.model.as_deref(),
                    defaults.account_id,
                );
                composer.set_model_catalog(
                    profile,
                    &[ModelEntry::exact_custom("account-model".into())],
                    false,
                );
                for character in "/model account-".chars() {
                    composer.apply(TerminalInput::Character(character));
                }
                composer.set_completion_selection(
                    defaults.provider.as_deref(),
                    defaults.model.as_deref(),
                    defaults.account_id,
                );
                assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
                assert_eq!(composer.take(), "/model account-model");
                defaults.model = Some("different-staged-model".into());
                composer.set_completion_selection(
                    defaults.provider.as_deref(),
                    defaults.model.as_deref(),
                    defaults.account_id,
                );
                for character in "/model account-".chars() {
                    composer.apply(TerminalInput::Character(character));
                }
                assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
                assert_eq!(composer.take(), "/model account-model");
                for character in "/model account-".chars() {
                    composer.apply(TerminalInput::Character(character));
                }
                let caret = composer.cursor_byte_offset();
                defaults.account_id = next;
                composer.set_completion_selection(
                    defaults.provider.as_deref(),
                    defaults.model.as_deref(),
                    defaults.account_id,
                );
                assert_eq!(composer.text(), "/model account-");
                assert_eq!(composer.cursor_byte_offset(), caret);
                assert_eq!(
                    composer.apply(TerminalInput::Tab),
                    ComposerEdit::Unchanged,
                    "{profile}: changing account source must discard its old catalog"
                );
                assert_eq!(composer.text(), "/model account-");
                assert_eq!(composer.cursor_byte_offset(), caret);
                assert!(
                    composer
                        .command_preview()
                        .expect("model preview")
                        .status
                        .contains("run /model")
                );
                composer.set_model_catalog(
                    profile,
                    &[ModelEntry::exact_custom("account-new-model".into())],
                    false,
                );
                assert_eq!(composer.apply(TerminalInput::Tab), ComposerEdit::Changed);
                assert_eq!(composer.take(), "/model account-new-model");
            }
        }
    }
}
