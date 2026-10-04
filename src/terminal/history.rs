use crate::presentation::{
    escape_terminal, escaped_terminal_width, image_message_text, safe_truncate,
};
use crate::session::{RunId, RunView, SessionId, SessionView};
use std::borrow::Cow;
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_FIND_BYTES: usize = 64;
const MAX_ROW_CELLS: usize = 4096;
const MAX_NOTICES: usize = 32;
const MAX_NOTICE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MessageKind {
    User,
    Assistant,
    Notice,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageSource {
    User(usize),
    Assistant(usize),
    Notice(u64),
}

struct Notice {
    id: u64,
    after: Option<MessageSource>,
    kind: MessageKind,
    text: String,
}

struct MessageIndex {
    source: MessageSource,
    kind: MessageKind,
    start: usize,
    rows: usize,
}

struct MessageRow {
    text: String,
    source_byte: usize,
}

#[derive(Clone)]
pub(super) struct HistoryRow {
    pub(super) text: String,
    pub(super) speaker: Option<MessageKind>,
    pub(super) matched: bool,
}

#[derive(Default)]
pub(super) struct History {
    session: Option<SessionId>,
    width: usize,
    height: usize,
    last_run: Option<RunId>,
    run_count: usize,
    last_answered: bool,
    messages: Vec<MessageIndex>,
    total_rows: usize,
    top: usize,
    follow_tail: bool,
    unseen_new_content: bool,
    find: Option<String>,
    find_failed: bool,
    matched_row: Option<usize>,
    notices: VecDeque<Notice>,
    next_notice: u64,
}

impl History {
    pub(super) fn sync(&mut self, view: &SessionView, width: usize, height: usize) {
        let width = width.min(MAX_ROW_CELLS);
        let last_run = view.runs.last().map(|run| run.id);
        let last_answered = view
            .runs
            .last()
            .is_some_and(|run| run.assistant_message.is_some());
        let new_session = self.session != Some(view.id);
        let width_changed = self.width != width;
        let rebuild = new_session
            || width_changed
            || self.run_count != view.runs.len()
            || self.last_run != last_run
            || self.last_answered != last_answered;
        let previous_last = self
            .run_count
            .checked_sub(1)
            .and_then(|index| view.runs.get(index).map(|run| (index, run)));
        let can_append = !new_session
            && !width_changed
            && self.run_count <= view.runs.len()
            && match previous_last {
                None => self.run_count == 0,
                Some((_, run)) => {
                    Some(run.id) == self.last_run
                        && (!self.last_answered || run.assistant_message.is_some())
                        && (view.runs.len() == self.run_count || run.assistant_message.is_some())
                }
            };
        let anchor = if !new_session && width_changed && !self.follow_tail {
            self.messages.iter().find_map(|message| {
                (message.start <= self.top && self.top < message.start + message.rows).then(|| {
                    let offset = self.top - message.start;
                    let rows = message_rows(
                        message.kind,
                        &message_text(view, message, &self.notices),
                        self.width,
                    );
                    (message.source, rows[offset].source_byte, offset == 0)
                })
            })
        } else {
            None
        };
        if rebuild {
            if new_session {
                self.follow_tail = true;
                self.unseen_new_content = false;
                self.find = None;
                self.matched_row = None;
                self.notices.clear();
            }
            let previous_rows = self.total_rows;
            if can_append {
                if let Some((index, run)) = previous_last
                    && !self.last_answered
                    && let Some(answer) = &run.assistant_message
                {
                    self.push(
                        MessageSource::Assistant(index),
                        MessageKind::Assistant,
                        answer,
                        width,
                    );
                }
                for (index, run) in view.runs.iter().enumerate().skip(self.run_count) {
                    self.push(
                        MessageSource::User(index),
                        MessageKind::User,
                        &image_message_text(&run.objective, &run.images),
                        width,
                    );
                    if let Some(answer) = &run.assistant_message {
                        self.push(
                            MessageSource::Assistant(index),
                            MessageKind::Assistant,
                            answer,
                            width,
                        );
                    }
                }
            } else {
                self.messages.clear();
                self.total_rows = 0;
                self.push_notices_after(None, width);
                for (index, (user_rows, answer_rows)) in
                    run_row_counts(&view.runs, width).into_iter().enumerate()
                {
                    let source = MessageSource::User(index);
                    self.push_count(source, MessageKind::User, user_rows);
                    self.push_notices_after(Some(source), width);
                    if let Some(answer_rows) = answer_rows {
                        let source = MessageSource::Assistant(index);
                        self.push_count(source, MessageKind::Assistant, answer_rows);
                        self.push_notices_after(Some(source), width);
                    }
                }
            }
            self.session = Some(view.id);
            self.width = width;
            self.last_run = last_run;
            self.run_count = view.runs.len();
            self.last_answered = last_answered;
            if !self.follow_tail && !width_changed && self.total_rows > previous_rows {
                self.unseen_new_content = true;
            }
            if let Some((source, byte, heading)) = anchor
                && let Some(message) = self
                    .messages
                    .iter()
                    .find(|message| message.source == source)
            {
                let rows = message_rows(
                    message.kind,
                    &message_text(view, message, &self.notices),
                    width,
                );
                let offset = if heading {
                    0
                } else {
                    rows.iter()
                        .rposition(|row| row.source_byte <= byte)
                        .unwrap_or(0)
                };
                self.top = message.start + offset;
            }
        }
        self.height = height;
        self.top = if self.follow_tail {
            self.tail()
        } else {
            self.top.min(self.tail())
        };
    }

    fn push(&mut self, source: MessageSource, kind: MessageKind, text: &str, width: usize) {
        let rows = message_row_count(text, width);
        self.push_count(source, kind, rows);
    }

    fn push_count(&mut self, source: MessageSource, kind: MessageKind, rows: usize) {
        self.messages.push(MessageIndex {
            source,
            kind,
            start: self.total_rows,
            rows,
        });
        self.total_rows = self.total_rows.saturating_add(rows);
    }

    fn push_notices_after(&mut self, after: Option<MessageSource>, width: usize) {
        for index in 0..self.notices.len() {
            let notice = &self.notices[index];
            if notice.after == after {
                let source = MessageSource::Notice(notice.id);
                let kind = notice.kind;
                let rows = message_row_count(&notice.text, width);
                self.push_count(source, kind, rows);
            }
        }
    }

    pub(super) fn record_notice(&mut self, text: &str) {
        let (kind, text) = match text.strip_prefix("Error:") {
            Some(body) => (MessageKind::Error, body.trim_start()),
            None => (MessageKind::Notice, text),
        };
        let shortened = (text.len() > MAX_NOTICE_BYTES).then(|| {
            let mut end = MAX_NOTICE_BYTES - '…'.len_utf8();
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &text[..end])
        });
        let text = shortened.as_deref().unwrap_or(text);
        let after = self.messages.iter().rev().find_map(|message| {
            (!matches!(message.source, MessageSource::Notice(_))).then_some(message.source)
        });
        if text.is_empty()
            || self
                .notices
                .back()
                .is_some_and(|last| last.after == after && last.kind == kind && last.text == text)
        {
            return;
        }
        if self.notices.len() == MAX_NOTICES {
            let oldest = self.notices.pop_front().expect("full notice history");
            if let Some(index) = self
                .messages
                .iter()
                .position(|message| message.source == MessageSource::Notice(oldest.id))
            {
                let removed = self.messages.remove(index);
                for message in &mut self.messages[index..] {
                    message.start -= removed.rows;
                }
                self.total_rows -= removed.rows;
                self.top -= self.top.saturating_sub(removed.start).min(removed.rows);
                self.matched_row = self.matched_row.and_then(|row| {
                    if row < removed.start {
                        Some(row)
                    } else if row >= removed.start + removed.rows {
                        Some(row - removed.rows)
                    } else {
                        None
                    }
                });
            }
        }
        let id = self.next_notice;
        self.next_notice += 1;
        self.push(MessageSource::Notice(id), kind, text, self.width);
        self.notices.push_back(Notice {
            id,
            after,
            kind,
            text: text.to_owned(),
        });
        if self.follow_tail {
            self.top = self.tail();
        } else {
            self.unseen_new_content = true;
        }
    }

    fn tail(&self) -> usize {
        self.total_rows.saturating_sub(self.height)
    }

    pub(super) fn handle(&mut self, input: super::TerminalInput, view: &SessionView) -> bool {
        use super::TerminalInput;
        if let Some(query) = &mut self.find {
            match input {
                TerminalInput::Character(character)
                    if query.len() + character.len_utf8() <= MAX_FIND_BYTES =>
                {
                    query.push(character);
                    self.find_failed = false;
                }
                TerminalInput::Backspace => {
                    if let Some((start, _)) = query.grapheme_indices(true).next_back() {
                        query.truncate(start);
                    }
                    self.find_failed = false;
                }
                TerminalInput::Submit => {
                    let query = self.find.take().expect("open find");
                    self.find_failed = !self.search(view, &query);
                    if self.find_failed {
                        self.find = Some(query);
                    }
                }
                TerminalInput::Escape => {
                    self.find = None;
                    self.find_failed = false;
                }
                TerminalInput::Interrupt => {
                    self.find = None;
                    return false;
                }
                TerminalInput::Resize | TerminalInput::Shutdown(_) | TerminalInput::Suspend => {
                    return false;
                }
                _ => {}
            }
            return true;
        }
        match input {
            TerminalInput::PageUp => {
                let top = self
                    .top
                    .saturating_sub(self.height.saturating_sub(1).max(1));
                if top < self.top {
                    self.top = top;
                    self.follow_tail = false;
                }
                true
            }
            TerminalInput::PageDown => {
                self.top = self
                    .top
                    .saturating_add(self.height.saturating_sub(1).max(1))
                    .min(self.tail());
                self.follow_tail = self.top == self.tail();
                if self.follow_tail {
                    self.unseen_new_content = false;
                }
                true
            }
            TerminalInput::HistoryLive => {
                self.follow_tail = true;
                self.unseen_new_content = false;
                self.top = self.tail();
                true
            }
            TerminalInput::HistoryFind => {
                self.find = Some(String::new());
                self.find_failed = false;
                true
            }
            _ => false,
        }
    }

    fn search(&mut self, view: &SessionView, query: &str) -> bool {
        if query.is_empty() {
            return false;
        }
        for message in self.messages.iter().rev() {
            let text = message_text(view, message, &self.notices);
            if let Some(byte) = text.find(query) {
                let before = message_row_count(&text[..byte], self.width);
                let row = message.start + before.saturating_sub(2);
                self.top = row.saturating_sub(1).min(self.tail());
                self.follow_tail = false;
                self.matched_row = Some(row);
                return true;
            }
        }
        false
    }

    pub(super) fn status(&self) -> Option<String> {
        if let Some(query) = &self.find {
            let (prefix, suffix) = match (self.find_failed, self.width) {
                (false, 0..=23) => ("F:", " Enter Esc"),
                (true, 0..=23) => ("0 ", " BS Esc"),
                (false, 24..=49) => ("Find: ", " Enter Esc"),
                (true, 24..=49) => ("No match ", " BS Esc"),
                (false, _) => ("Find: ", " · Enter search · Esc close"),
                (true, _) => ("Find: ", " · no match · edit or Esc"),
            };
            let query_width = self
                .width
                .saturating_sub(escaped_terminal_width(prefix) + escaped_terminal_width(suffix));
            let preview = find_preview(query, query_width);
            return Some(format!("{prefix}{preview}{suffix}"));
        }
        (!self.follow_tail).then(|| {
            if self.width < 24 {
                return if self.unseen_new_content {
                    "New · Ctrl+L".to_owned()
                } else {
                    "PgUp/Dn Ctrl+L".to_owned()
                };
            }
            if self.width >= 80 && !self.unseen_new_content {
                return "History · Ctrl+L live".to_owned();
            }
            if self.unseen_new_content {
                "New text · Ctrl+L live".to_owned()
            } else {
                "History · PgUp/PgDn · Ctrl+L live".to_owned()
            }
        })
    }

    pub(super) fn visible(&self, view: &SessionView) -> Vec<HistoryRow> {
        let mut visible = Vec::with_capacity(self.height);
        let end = self.top.saturating_add(self.height);
        for message in &self.messages {
            if message.start + message.rows <= self.top {
                continue;
            }
            if message.start >= end {
                break;
            }
            let rows = message_rows(
                message.kind,
                &message_text(view, message, &self.notices),
                self.width,
            );
            for (offset, line) in rows.into_iter().enumerate() {
                let row = message.start + offset;
                if row >= self.top && row < end {
                    visible.push(HistoryRow {
                        speaker: (offset == 0).then_some(message.kind),
                        matched: self.matched_row == Some(row),
                        text: line.text,
                    });
                }
            }
        }
        visible
    }
}

fn find_preview(query: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if escaped_terminal_width(query) <= width {
        return safe_truncate(query, width);
    }
    let mut start = query.len();
    let mut remaining = width.saturating_sub(1);
    for (byte, grapheme) in query.grapheme_indices(true).rev() {
        let cells = escaped_terminal_width(grapheme);
        if cells > remaining {
            break;
        }
        remaining -= cells;
        start = byte;
    }
    format!(
        "…{}",
        safe_truncate(&query[start..], width.saturating_sub(1))
    )
}

fn run_row_counts(runs: &[RunView], width: usize) -> Vec<(usize, Option<usize>)> {
    let total_bytes: usize = runs
        .iter()
        .map(|run| run.objective.len() + run.assistant_message.as_ref().map_or(0, String::len))
        .sum();
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    let count = |run: &RunView| {
        (
            message_row_count(&image_message_text(&run.objective, &run.images), width),
            run.assistant_message
                .as_ref()
                .map(|answer| message_row_count(answer, width)),
        )
    };
    if total_bytes < 4 * 1024 * 1024 || workers < 2 || runs.len() < workers {
        return runs.iter().map(count).collect();
    }
    let chunk_size = runs.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let tasks = runs
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || chunk.iter().map(count).collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        tasks
            .into_iter()
            .flat_map(|task| task.join().expect("history row counter"))
            .collect()
    })
}

fn message_text<'a>(
    view: &'a SessionView,
    message: &MessageIndex,
    notices: &'a VecDeque<Notice>,
) -> Cow<'a, str> {
    match message.source {
        MessageSource::User(run) => {
            image_message_text(&view.runs[run].objective, &view.runs[run].images)
        }
        MessageSource::Assistant(run) => Cow::Borrowed(
            view.runs[run]
                .assistant_message
                .as_deref()
                .expect("indexed answer"),
        ),
        MessageSource::Notice(id) => Cow::Borrowed(
            &notices
                .iter()
                .find(|notice| notice.id == id)
                .expect("indexed notice")
                .text,
        ),
    }
}

fn message_rows(kind: MessageKind, text: &str, width: usize) -> Vec<MessageRow> {
    let width = width.min(MAX_ROW_CELLS);
    let heading = match kind {
        MessageKind::User => "You:",
        MessageKind::Assistant => "Arany:",
        MessageKind::Notice => "Arany · notice:",
        MessageKind::Error => "Arany · error:",
    };
    let mut rows = vec![MessageRow {
        text: safe_truncate(heading, width),
        source_byte: 0,
    }];
    let mut line_start = 0;
    for logical in text.split('\n') {
        let mut row = String::from("  ");
        let mut cells = 2;
        let mut row_start = line_start;
        for (byte, grapheme) in logical.grapheme_indices(true) {
            let safe = escape_terminal(grapheme);
            let grapheme_cells = UnicodeWidthStr::width(safe.as_str());
            if cells + grapheme_cells > width && cells > 2 {
                rows.push(MessageRow {
                    text: row,
                    source_byte: row_start,
                });
                row = String::from("  ");
                cells = 2;
                row_start = line_start + byte;
            }
            if cells + grapheme_cells <= width {
                row.push_str(&safe);
                cells += grapheme_cells;
            } else if cells < width {
                row.push('…');
                cells += 1;
            }
        }
        rows.push(MessageRow {
            text: safe_truncate(&row, width),
            source_byte: row_start,
        });
        line_start += logical.len() + 1;
    }
    rows.push(MessageRow {
        text: String::new(),
        source_byte: text.len(),
    });
    rows
}

fn message_row_count(text: &str, width: usize) -> usize {
    let width = width.min(MAX_ROW_CELLS);
    if text
        .bytes()
        .all(|byte| byte == b'\n' || (b' '..=b'~').contains(&byte))
    {
        let content_width = width.saturating_sub(2);
        return 2 + text
            .split('\n')
            .map(|line| {
                if content_width == 0 {
                    1
                } else {
                    line.len().div_ceil(content_width).max(1)
                }
            })
            .sum::<usize>();
    }
    let mut rows = 2;
    for logical in text.split('\n') {
        rows += 1;
        let mut cells = 2;
        for grapheme in logical.graphemes(true) {
            let grapheme_cells = escaped_terminal_width(grapheme);
            if cells + grapheme_cells > width && cells > 2 {
                rows += 1;
                cells = 2;
            }
            if cells + grapheme_cells <= width {
                cells += grapheme_cells;
            } else if cells < width {
                cells += 1;
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        AgentDisposition, AgentRole, AgentRunId, CollaborationPolicy, Event, RunConfig,
        RunDisposition, RunStatus, RunView, SessionDefaults,
    };
    use crate::store::{StateRoot, Store};
    use std::time::Instant;

    fn session() -> SessionView {
        SessionView {
            id: SessionId::new(),
            title: "History".into(),
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            runs: Vec::new(),
            compactions: Vec::new(),
        }
    }

    fn run(objective: &str, answer: &str) -> RunView {
        RunView {
            id: RunId::new(),
            objective: objective.into(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: Some(answer.into()),
            status: RunStatus::Finished,
            accepted_sequence: 2,
            finished_sequence: Some(3),
            tools: Vec::new(),
        }
    }

    #[test]
    fn history_keeps_the_reading_row_when_a_committed_turn_arrives() {
        let mut view = session();
        view.runs.push(run("first objective", "first answer"));
        view.runs[0].images.push(crate::provider::test_image());
        view.runs.push(run("second objective", "second answer"));
        let mut full_history = History::default();
        full_history.sync(&view, 80, 20);
        let full_rows = full_history.visible(&view);
        assert!(
            full_rows
                .iter()
                .any(|row| row.text == "  Image 1: PNG 1x1, 69 bytes")
        );
        assert!(!full_rows.iter().any(|row| row.text.contains("iVBOR")));
        assert!(
            full_rows
                .iter()
                .any(|row| { row.text == "You:" && row.speaker == Some(MessageKind::User) })
        );
        assert!(
            full_rows
                .iter()
                .any(|row| { row.text == "Arany:" && row.speaker == Some(MessageKind::Assistant) })
        );
        let mut history = History::default();
        history.sync(&view, 16, 4);
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.text.contains("second answer"))
        );
        assert!(history.handle(super::super::TerminalInput::PageUp, &view));
        let before = history
            .visible(&view)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();
        history.sync(&view, 16, 1);
        assert_eq!(history.visible(&view)[0].text, before[0]);
        history.sync(&view, 16, 4);
        assert_eq!(
            history
                .visible(&view)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
            before,
            "transient completion rows preserve the older reading anchor",
        );
        let mut third = run("third objective", "third answer");
        third.assistant_message = None;
        third.status = RunStatus::Active;
        third.finished_sequence = None;
        view.runs.push(third);
        history.sync(&view, 16, 4);
        assert_eq!(history.messages.len(), 5, "new objective indexed once");
        let third = view.runs.last_mut().expect("new Run");
        third.assistant_message = Some("third answer".into());
        third.status = RunStatus::Finished;
        third.finished_sequence = Some(3);
        history.sync(&view, 16, 4);
        assert_eq!(history.messages.len(), 6, "committed answer indexed once");
        assert_eq!(
            history
                .visible(&view)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
            before
        );
        assert!(
            history
                .status()
                .expect("reading status")
                .contains("New · Ctrl+L")
        );
        assert!(history.handle(super::super::TerminalInput::HistoryLive, &view));
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.text.contains("third answer"))
        );
    }

    #[test]
    fn local_feedback_survives_edits_arrivals_resize_and_bounded_eviction() {
        let mut view = session();
        let mut history = History::default();
        history.sync(&view, 40, 20);
        history.record_notice("Error: Choose /setup\u{1b}[2J\nYour draft is retained");
        let rows = history.visible(&view);
        assert_eq!(rows[0].speaker, Some(MessageKind::Error));
        assert_eq!(rows[0].text, "Arany · error:");
        assert_eq!(rows[1].text, "  Choose /setup\\u{001b}[2J");
        assert_eq!(rows[2].text, "  Your draft is retained");
        let first_notice = history.messages[0].source;
        history.sync(&view, 40, 20);
        assert_eq!(history.visible(&view)[1].text, rows[1].text);
        history.record_notice("Error: Choose /setup\u{1b}[2J\nYour draft is retained");
        assert_eq!(history.notices.len(), 1, "repeated redraw is coalesced");

        let mut active = run("accepted objective", "committed answer");
        active.assistant_message = None;
        view.runs.push(active);
        history.sync(&view, 40, 20);
        history.record_notice("Local recovery completed");
        let second_notice = history.messages.last().expect("local notice").source;
        view.runs[0].assistant_message = Some("committed answer".into());
        history.sync(&view, 40, 20);
        let sources = history
            .messages
            .iter()
            .map(|message| message.source)
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            [
                first_notice,
                MessageSource::User(0),
                second_notice,
                MessageSource::Assistant(0)
            ]
        );
        history.sync(&view, 16, 4);
        assert_eq!(
            history
                .messages
                .iter()
                .map(|message| message.source)
                .collect::<Vec<_>>(),
            sources
        );

        history.handle(super::super::TerminalInput::HistoryFind, &view);
        for character in "Local recovery".chars() {
            history.handle(super::super::TerminalInput::Character(character), &view);
        }
        history.handle(super::super::TerminalInput::Submit, &view);
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.matched && row.text.contains("Local"))
        );
        history.sync(&view, 24, 4);
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.text.contains("Local recovery"))
        );
        let before = history
            .visible(&view)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();
        history.record_notice("Another local notice");
        assert_eq!(
            history
                .visible(&view)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
            before
        );

        history.handle(super::super::TerminalInput::HistoryLive, &view);
        history.record_notice(&"é".repeat(MAX_NOTICE_BYTES / 2 + 10));
        let bounded = &history.notices.back().expect("bounded notice").text;
        assert!(bounded.len() <= MAX_NOTICE_BYTES);
        assert!(bounded.ends_with('…'));
        for index in 0..MAX_NOTICES {
            history.record_notice(&format!("Notice {index}"));
        }
        assert_eq!(history.notices.len(), MAX_NOTICES);
        assert!(
            !history
                .messages
                .iter()
                .any(|message| message.source == first_notice)
        );
        history.handle(super::super::TerminalInput::PageUp, &view);
        let before = history
            .visible(&view)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();
        history.record_notice("Newest notice");
        assert_eq!(
            history
                .visible(&view)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(view.runs[0].objective, "accepted objective");
        assert_eq!(
            view.runs[0].assistant_message.as_deref(),
            Some("committed answer")
        );
        history.sync(&session(), 24, 4);
        assert!(history.notices.is_empty());
        assert!(history.messages.is_empty());
    }

    #[test]
    fn find_does_not_put_untrusted_controls_into_visible_rows() {
        let mut view = session();
        view.runs
            .push(run("hello\u{1b}[31m", "found line\nmore text"));
        let mut history = History::default();
        history.sync(&view, 16, 6);
        assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
        for character in "found".chars() {
            assert!(history.handle(super::super::TerminalInput::Character(character), &view));
        }
        assert!(history.status().expect("find hint").ends_with("Enter Esc"));
        assert!(history.handle(super::super::TerminalInput::Submit, &view));
        let rows = history.visible(&view);
        assert!(
            rows.iter()
                .any(|row| row.matched && row.text.contains("found"))
        );
        assert!(rows.iter().all(|row| !row.text.contains('\u{1b}')));
        assert!(
            message_rows(MessageKind::User, "x\u{202e}", 16)
                .iter()
                .any(|row| row.text.contains("\\u{202e}"))
        );
        assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
        assert!(!history.handle(super::super::TerminalInput::Interrupt, &view));
    }

    #[test]
    fn find_backspace_removes_one_visible_grapheme() {
        let view = session();
        let mut history = History::default();
        history.sync(&view, 16, 6);
        assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
        for character in "a👩‍💻e\u{301}".chars() {
            assert!(history.handle(super::super::TerminalInput::Character(character), &view));
        }
        for expected in ["a👩‍💻", "a", ""] {
            assert!(history.handle(super::super::TerminalInput::Backspace, &view));
            assert_eq!(history.find.as_deref(), Some(expected));
        }
    }

    #[test]
    fn find_status_keeps_actions_and_recent_query_visible() {
        let view = session();
        for width in [16, 24, 40, 50, 80] {
            let mut history = History::default();
            history.sync(&view, width, 6);
            assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
            for character in "\u{1b}abcdefghijklmnopqrstuvwx e\u{301}👩‍💻XYZ".chars() {
                assert!(history.handle(super::super::TerminalInput::Character(character), &view));
            }
            let status = history.status().expect("find hint");
            assert!(status.contains("XYZ"), "width={width}: {status}");
            assert!(!status.contains('\u{1b}'));
            assert!(
                status.ends_with(if width >= 50 {
                    "Esc close"
                } else {
                    "Enter Esc"
                }),
                "width={width}: {status}"
            );
            assert!(escaped_terminal_width(&status) <= width);

            assert!(history.handle(super::super::TerminalInput::Submit, &view));
            let status = history.status().expect("failed find hint");
            assert!(status.contains("XYZ"), "width={width}: {status}");
            assert!(!status.contains('\u{1b}'));
            if width >= 24 {
                assert!(
                    status.to_ascii_lowercase().contains("no match"),
                    "width={width}: {status}"
                );
            }
            assert!(status.ends_with("Esc"), "width={width}: {status}");
            assert!(escaped_terminal_width(&status) <= width);
        }
    }

    #[test]
    fn resize_keeps_the_same_message_and_source_position() {
        let mut view = session();
        view.runs.push(run(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron",
            "short answer",
        ));
        let mut history = History::default();
        history.sync(&view, 16, 3);
        let (old_kind, old_byte) = loop {
            history.handle(super::super::TerminalInput::PageUp, &view);
            let message = history
                .messages
                .iter()
                .find(|message| {
                    message.start <= history.top && history.top < message.start + message.rows
                })
                .expect("visible message");
            let rows = message_rows(
                message.kind,
                &message_text(&view, message, &history.notices),
                16,
            );
            let byte = rows[history.top - message.start].source_byte;
            if message.kind == MessageKind::User && byte > 0 {
                break (message.kind, byte);
            }
            assert!(
                history.top > 0,
                "reached the first row before a content anchor"
            );
        };
        history.sync(&view, 24, 3);
        let message = history
            .messages
            .iter()
            .find(|message| {
                message.start <= history.top && history.top < message.start + message.rows
            })
            .expect("resized visible message");
        let rows = message_rows(
            message.kind,
            &message_text(&view, message, &history.notices),
            24,
        );
        let offset = history.top - message.start;
        assert_eq!(message.kind, old_kind);
        assert!(rows[offset].source_byte <= old_byte);
        assert!(
            rows.get(offset + 1)
                .is_none_or(|row| row.source_byte > old_byte)
        );
    }

    #[test]
    fn row_count_matches_materialized_rows() {
        let long = "0123456789abcdef".repeat(100);
        for text in [
            "",
            "hello world",
            "hello\n\nworld\n",
            "é 中 👩‍💻 e\u{301}",
            "a\u{1b}[31m\u{202e}b\t",
            "\u{0000}\u{007f}\u{009f}\u{061c}\u{200e}\u{2028}\u{2069}",
            long.as_str(),
        ] {
            for width in [0, 1, 2, 3, 8, 16, 80, 4097] {
                assert_eq!(
                    message_row_count(text, width),
                    message_rows(MessageKind::User, text, width).len(),
                    "text={text:?}, width={width}"
                );
            }
        }
    }

    #[test]
    fn large_rebuild_preserves_message_order_and_find_anchor() {
        let mut view = session();
        let body = "0123456789abcdef".repeat(1024);
        for index in 0..128 {
            view.runs.push(run(
                &format!("user {index:04} {body}"),
                &format!("answer {index:04} {body}"),
            ));
        }
        let mut history = History::default();
        history.sync(&view, 80, 20);
        assert_eq!(history.messages.len(), 256);
        for (index, message) in history.messages.iter().enumerate() {
            assert_eq!(
                message.source,
                if index % 2 == 0 {
                    MessageSource::User(index / 2)
                } else {
                    MessageSource::Assistant(index / 2)
                }
            );
            assert_eq!(
                message.kind,
                if index % 2 == 0 {
                    MessageKind::User
                } else {
                    MessageKind::Assistant
                }
            );
            if index > 0 {
                let previous = &history.messages[index - 1];
                assert_eq!(message.start, previous.start + previous.rows);
            }
        }
        assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
        for character in "answer 0127".chars() {
            assert!(history.handle(super::super::TerminalInput::Character(character), &view));
        }
        assert!(history.handle(super::super::TerminalInput::Submit, &view));
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.matched && row.text.contains("answer 0127"))
        );
        history.sync(&view, 50, 20);
        assert_eq!(history.messages.len(), 256);
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.text.contains("answer 0127"))
        );
    }

    #[test]
    #[ignore = "explicit optimized named-host history-index measurement"]
    fn large_history_index_cost_on_named_host() {
        for (label, pattern) in [
            ("ascii", "0123456789abcdef"),
            ("unicode", "e\u{301}中👩‍💻\u{202e}"),
        ] {
            let mut view = session();
            let payload = pattern.repeat(24_576 / pattern.len());
            for _ in 0..2000 {
                view.runs.push(run(&payload, &payload));
            }
            let mut history = History::default();
            let start = Instant::now();
            history.sync(&view, 80, 20);
            let initial = start.elapsed();
            let start = Instant::now();
            assert!(!history.visible(&view).is_empty());
            let redraw = start.elapsed();
            let start = Instant::now();
            history.sync(&view, 50, 20);
            let resize = start.elapsed();
            assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
            for character in "absent".chars() {
                assert!(history.handle(super::super::TerminalInput::Character(character), &view));
            }
            let start = Instant::now();
            assert!(history.handle(super::super::TerminalInput::Submit, &view));
            assert!(history.find_failed);
            let find_miss = start.elapsed();
            println!(
                "history_94mib_{label} initial={initial:?} redraw={redraw:?} resize={resize:?} find_miss={find_miss:?}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "explicit file-backed history replay and index measurement"]
    async fn file_backed_history_cost_on_named_host() {
        let runs = std::env::var("ARANY_TEST_HISTORY_RUNS")
            .map(|value| value.parse::<usize>().expect("numeric Run count"))
            .unwrap_or(128);
        assert!((1..=1600).contains(&runs), "bounded Run count");
        let fill_state = std::env::var_os("ARANY_TEST_HISTORY_FILL_STATE").is_some();
        let private = tempfile::tempdir().expect("private State parent");
        let state = private.path().join("state");
        let session_id = SessionId::new();
        let store =
            Store::open(StateRoot::admit(&state).expect("private State")).expect("writable Store");
        let objective_body = "0123456789abcdef".repeat(511);
        let answer_body = "0123456789abcdef".repeat(2047);
        let mut histories = vec![(session_id, runs)];
        if fill_state {
            histories.extend([(SessionId::new(), runs), (SessionId::new(), 240)]);
        }
        let session_count = histories.len();
        let seed_start = Instant::now();
        for (current_session_id, current_runs) in histories {
            store
                .append(
                    current_session_id,
                    Event::SessionStarted {
                        title: "Synthetic history".into(),
                        workspace_identity: Some((1, 1)),
                    },
                )
                .await
                .expect("Session start");
            for index in 0..current_runs {
                let run_id = RunId::new();
                let agent_run_id = AgentRunId::new();
                let objective = format!("objective {index:04} {objective_body}");
                let answer = format!("answer {index:04} {answer_body}");
                for event in [
                    Event::MessageAccepted {
                        run_id,
                        text: objective,
                        images: Vec::new(),
                    },
                    Event::RunStarted {
                        run_id,
                        config: RunConfig {
                            provider: "scripted".into(),
                            model: "test-model".into(),
                            effort: None,
                            custom_profile_provenance: None,
                            saved_api_account_id: None,
                            chatgpt_provenance: None,
                            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
                            policy: CollaborationPolicy::Single,
                            output_token_cap: 4096,
                            provider_concurrency: 1,
                            workspace_device: 1,
                            workspace_inode: 1,
                            instruction_digest: None,
                            include_digests: Vec::new(),
                            history_run_ids: Vec::new(),
                            excluded_history_runs: 0,
                            context_usage: None,
                            compaction_event_sequence: None,
                            compaction_content_digest: None,
                            tool_policy: None,
                        },
                    },
                    Event::AgentSpawned {
                        run_id,
                        agent_run_id,
                        role: AgentRole::Primary,
                        ordinal: 0,
                        objective: None,
                    },
                    Event::AgentFinished {
                        run_id,
                        agent_run_id,
                        disposition: AgentDisposition::Finished,
                        summary: Some("done".into()),
                        result: Some(answer.clone()),
                    },
                    Event::MessageCommitted {
                        run_id,
                        text: answer,
                    },
                    Event::RunFinished {
                        run_id,
                        disposition: RunDisposition::Finished,
                    },
                ] {
                    store
                        .append(current_session_id, event)
                        .await
                        .expect("valid Event");
                }
            }
        }
        store.close().await.expect("closed writable Store");
        let seed = seed_start.elapsed();
        let database_bytes = std::fs::metadata(state.join("events.sqlite3"))
            .expect("file-backed journal")
            .len();
        let open_start = Instant::now();
        let store =
            Store::open_read_only(StateRoot::open_existing(&state).expect("existing State"))
                .expect("read-only Store");
        let open = open_start.elapsed();
        let replay_start = Instant::now();
        let view = store
            .load_view(session_id)
            .await
            .expect("strict replay")
            .expect("Session view");
        let replay = replay_start.elapsed();
        store.close().await.expect("closed read-only Store");
        assert_eq!(view.runs.len(), runs);
        let events = 1 + runs * 6;
        assert_eq!(view.last_sequence, events as u64);
        assert!(
            view.runs
                .iter()
                .all(|run| run.status == RunStatus::Finished)
        );
        let transcript_bytes: usize = view
            .runs
            .iter()
            .map(|run| run.objective.len() + run.assistant_message.as_ref().unwrap().len())
            .sum();
        let mut history = History::default();
        let index_start = Instant::now();
        history.sync(&view, 80, 20);
        let index = index_start.elapsed();
        assert_eq!(history.messages.len(), runs * 2);
        let visible_start = Instant::now();
        assert!(!history.visible(&view).is_empty());
        let visible = visible_start.elapsed();
        let resize_start = Instant::now();
        history.sync(&view, 50, 20);
        let resize = resize_start.elapsed();
        println!(
            "history_file_backed sessions={session_count} runs={runs} events={events} transcript_bytes={transcript_bytes} database_bytes={database_bytes} seed={seed:?} open={open:?} replay={replay:?} index={index:?} visible={visible:?} resize={resize:?}"
        );
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string("/proc/self/status").expect("process RSS");
            let peak_rss_kib = status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmHWM:")
                        .and_then(|value| value.split_whitespace().next())
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("peak process RSS");
            println!("history_file_backed peak_rss_kib={peak_rss_kib}");
        }
    }
}
