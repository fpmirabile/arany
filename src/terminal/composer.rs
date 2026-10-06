use super::{
    TerminalInput,
    commands::{
        CommandChoices, CommandParseError, CommandPreview, Submission, command_completions,
        command_preview, completion_suffix, parse_submission,
    },
};
use crate::provider::{ImageAttachment, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, ModelEntry};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub(super) const MAX_DRAFT_BYTES: usize = 8 * 1024;

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

#[derive(Default)]
pub struct Composer {
    text: String,
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
    approval_mode: Option<crate::tools::ApprovalMode>,
}

impl Composer {
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
            return format!("Insert ${name} into your task");
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

    pub fn approval_mode(&self) -> Option<crate::tools::ApprovalMode> {
        self.approval_mode
    }

    pub fn set_runtime_skills(&mut self, names: &[String]) {
        self.skills = names
            .iter()
            .take(crate::tools::MAX_SKILLS)
            .filter(|name| crate::tools::types::valid_name(name))
            .map(|name| format!("${name}"))
            .collect();
        self.skills.sort();
        self.skills.dedup();
        self.completion_index = 0;
    }

    pub fn set_approval_mode(&mut self, mode: Option<crate::tools::ApprovalMode>) {
        self.approval_mode = mode;
    }

    pub fn file_query(&self) -> Option<String> {
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
            (
                names
                    .iter()
                    .map(|name| match name.strip_prefix('$') {
                        Some(name) => format!("Skill /{name}"),
                        None => format!("Cmd /{name}"),
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
        match parse_submission(&self.text) {
            Err(CommandParseError::Empty) if !self.images.is_empty() => {
                Ok(Submission::Objective(&self.text))
            }
            result => result,
        }
    }

    pub fn insert_paste(&mut self, text: &str) -> Result<ComposerEdit, &'static str> {
        if text.len() > MAX_DRAFT_BYTES {
            return Ok(ComposerEdit::AtCapacity);
        }
        if text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err("paste contains unsupported control characters");
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.text.len() + text.len() > MAX_DRAFT_BYTES {
            return Ok(ComposerEdit::AtCapacity);
        }
        if text.is_empty() {
            return Ok(ComposerEdit::Unchanged);
        }
        self.text.insert_str(self.cursor, &text);
        let target = self.cursor + text.len();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(start, grapheme)| start + grapheme.len())
            .find(|end| *end >= target)
            .unwrap_or(self.text.len());
        self.vertical_grapheme_column = None;
        self.completion_index = 0;
        self.completion_hidden = false;
        Ok(ComposerEdit::Changed)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }

    pub fn clear(&mut self) {
        self.skills_tab = false;
        self.text.clear();
        self.images.clear();
        self.cursor = 0;
        self.vertical_grapheme_column = None;
        self.completion_index = 0;
        self.completion_hidden = false;
    }

    pub fn take(&mut self) -> String {
        self.skills_tab = false;
        self.cursor = 0;
        self.vertical_grapheme_column = None;
        self.completion_index = 0;
        self.completion_hidden = false;
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
                    "Tab/Enter: {} · Up/Down choose · Esc close",
                    if names[selected].starts_with('$') {
                        format!("{} (Skill)", names[selected])
                    } else {
                        format!("/{}", names[selected])
                    }
                ),
            });
        }
        if self.cursor == self.text.len() {
            command_preview(&self.text, &self.choices)
        } else {
            None
        }
    }

    pub(super) fn completion_menu(&self) -> Option<(Vec<&str>, usize)> {
        if self.completion_hidden || self.cursor != self.text.len() {
            return None;
        }
        let prefix = self.text.strip_prefix('/')?;
        let mut names = command_completions(&self.text);
        if names.contains(&prefix) {
            return None;
        }
        if !prefix.is_empty() && !crate::tools::types::valid_name(prefix) {
            return None;
        }
        if self.completion_layout == CompletionLayout::Tabs && self.skills_tab {
            names.clear();
        }
        if self.completion_layout == CompletionLayout::Combined || self.skills_tab {
            names.extend(
                self.skills
                    .iter()
                    .filter(|name| name[1..].starts_with(prefix))
                    .map(String::as_str),
            );
        }
        if names.is_empty() && self.completion_layout == CompletionLayout::Combined {
            return None;
        }
        let selected = self.completion_index.min(names.len().saturating_sub(1));
        Some((names, selected))
    }

    pub fn apply(&mut self, input: TerminalInput) -> ComposerEdit {
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
                return if input == TerminalInput::Submit && !self.skills_tab && self.text != "/" {
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
                    let mut completed = if name.starts_with('$') {
                        format!("{name} ")
                    } else {
                        format!("/{name}")
                    };
                    if !name.starts_with('$')
                        && let Some(suffix) = completion_suffix(&completed, &self.choices)
                    {
                        completed.push_str(&suffix);
                    }
                    if completed.len() > MAX_DRAFT_BYTES {
                        return ComposerEdit::AtCapacity;
                    }
                    self.text = completed;
                    self.cursor = self.text.len();
                    self.vertical_grapheme_column = None;
                    self.completion_index = 0;
                    return ComposerEdit::Changed;
                }
                TerminalInput::Escape => {
                    self.completion_hidden = true;
                    return ComposerEdit::Changed;
                }
                _ => {}
            }
        }
        let edit = self.apply_edit(input);
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
            TerminalInput::Tab if self.cursor == self.text.len() => {
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
                if self.text.len() == MAX_DRAFT_BYTES {
                    return ComposerEdit::AtCapacity;
                }
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
                ComposerEdit::Changed
            }
            TerminalInput::Character(character) => {
                if self.text.len() + character.len_utf8() > MAX_DRAFT_BYTES {
                    return ComposerEdit::AtCapacity;
                }
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

    fn remove_range(&mut self, start: usize, end: usize) {
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
        let image = crate::provider::test_image();
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
            composer.insert_paste(&"x".repeat(MAX_DRAFT_BYTES)),
            Ok(ComposerEdit::Changed)
        );
        composer.apply(TerminalInput::Left);
        let cursor = composer.cursor_byte_offset();
        assert_eq!(composer.insert_paste("y"), Ok(ComposerEdit::AtCapacity));
        assert_eq!(composer.cursor_byte_offset(), cursor);
        assert_eq!(composer.text(), "x".repeat(MAX_DRAFT_BYTES));
        composer.clear();
        assert_eq!(
            composer.insert_paste(&"\r\n".repeat(MAX_DRAFT_BYTES / 2 + 1)),
            Ok(ComposerEdit::AtCapacity)
        );
        assert!(composer.text().is_empty());
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
                composer.set_approval_mode(Some(crate::tools::ApprovalMode::Request));
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
    fn slash_skills_are_distinct_bounded_task_drafts_not_local_commands() {
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
            ["Cmd /resume", "Cmd /rename"]
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
            assert_eq!(composer.apply(accept), ComposerEdit::Changed);
            assert_eq!(composer.text(), "$review ");
            assert_eq!(composer.cursor_byte_offset(), composer.text().len());
            assert_eq!(composer.submission(), Ok(Submission::Objective("$review ")));
            assert_eq!(
                composer.apply(TerminalInput::Submit),
                ComposerEdit::Unchanged
            );
        }
        composer.clear();
        composer.insert_paste("/he").unwrap();
        assert_eq!(
            composer.completion_rows().unwrap().0,
            ["Cmd /help", "Skill /help"]
        );
        composer.apply(TerminalInput::Down);
        composer.apply(TerminalInput::Submit);
        assert_eq!(composer.text(), "$help ");
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
        assert_eq!(composer.skills.len(), crate::tools::MAX_SKILLS);
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
                assert_eq!(composer.apply(accept), ComposerEdit::Changed);
                assert_eq!(composer.text(), completed);
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
                assert_eq!(composer.apply(accept), ComposerEdit::Changed);
                assert_eq!(
                    composer.text(),
                    format!(
                        "/{expected}{}",
                        if expected == "settings" { " " } else { "" }
                    )
                );
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
        assert_eq!(composer.apply(TerminalInput::Escape), ComposerEdit::Changed);
        assert!(composer.completion_menu().is_none());
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
                let mut defaults = crate::SessionDefaults {
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
