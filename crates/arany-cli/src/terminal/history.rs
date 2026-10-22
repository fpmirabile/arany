use super::clipboard::{COPY_TOO_LARGE, MAX_COPY_BYTES, display_bytes, display_text};
use crate::presentation::{
    escape_terminal, escaped_terminal_bytes, escaped_terminal_width, image_message_text,
    safe_truncate,
};
use arany::{RunId, RunView, SessionId, SessionView};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_FIND_BYTES: usize = 64;
pub(super) const MAX_ROW_CELLS: usize = 4096;
pub(super) const MAX_CLASSIC_HISTORY_RUNS: usize = 16;
pub(super) const MAX_CLASSIC_HISTORY_BYTES: usize = 64 * 1024;
pub(super) const MAX_CLASSIC_HISTORY_ROWS: usize = 4096;
const MAX_NOTICES: usize = 32;
const MAX_NOTICE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pager {
    Handled,
    Close,
    Dump,
    Pass,
}

pub(super) struct ClassicProjection {
    pub(super) first_run_index: Option<usize>,
    pub(super) omitted_history: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MessageKind {
    User,
    Assistant,
    // A child's assignment and result in its agent transcript.
    Assignment,
    AgentResult,
    SteeringNote,
    Notice,
    Error,
    Preview,
    Incomplete,
    Tool,
    ToolDetails,
    Commentary,
    Supervisor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageSource {
    User(usize),
    Assistant(usize),
    Notice(u64),
    Tool(usize, usize),
    Steering(usize, usize),
    Commentary(usize, u32, u8),
    Supervisor(usize, usize),
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
    text: Option<String>,
}

struct MessageRow {
    text: String,
    source_byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextPoint {
    source: MessageSource,
    byte: usize,
    // A boundary sits between graphemes; otherwise the grapheme starting at `byte` is selected.
    boundary: bool,
}

#[derive(Clone, Copy)]
struct Selection {
    anchor: TextPoint,
    head: TextPoint,
    shown: bool,
}

#[derive(Clone)]
pub(super) struct HistoryRow {
    pub(super) text: String,
    pub(super) speaker: MessageKind,
    pub(super) heading: bool,
    pub(super) matched: bool,
    // Selected cells of `text`, as a half-open column range.
    pub(super) selected: Option<(usize, usize)>,
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
    last_find: Option<String>,
    matched_row: Option<usize>,
    notices: VecDeque<Notice>,
    next_notice: u64,
    sequence: u64,
    details: bool,
    dirty: bool,
    preview_dirty: bool,
    preview: Option<arany::DisplayUpdate>,
    selection: Option<Selection>,
    agent: bool,
}

impl History {
    // Labels the objective and answer as a child's assignment and result.
    pub(super) fn agent() -> Self {
        Self {
            agent: true,
            ..Self::default()
        }
    }

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
        let rebuild =
            rebuild || self.dirty || self.preview_dirty || self.sequence != view.last_sequence;
        let anchor = if !new_session && rebuild && !self.follow_tail {
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
            let previous_rows = self.total_rows;
            let full = new_session
                || width_changed
                || self.dirty
                || self.run_count > view.runs.len()
                || self.last_run != previous_run(view, self.run_count);
            if new_session {
                self.follow_tail = true;
                self.unseen_new_content = false;
                self.selection = None;
                self.find = None;
                self.last_find = None;
                self.matched_row = None;
                self.notices.clear();
                self.preview = None;
            }
            let first = if full {
                0
            } else {
                self.run_count.saturating_sub(1)
            };
            let retain = self
                .messages
                .iter()
                .position(|message| match message.source {
                    MessageSource::User(run)
                    | MessageSource::Assistant(run)
                    | MessageSource::Supervisor(run, _)
                    | MessageSource::Steering(run, _)
                    | MessageSource::Tool(run, _)
                    | MessageSource::Commentary(run, _, _) => run >= first,
                    MessageSource::Notice(_) => false,
                })
                .unwrap_or(self.messages.len());
            if full {
                self.messages.clear();
                self.total_rows = 0;
                self.push_notices_after(None, width);
            } else {
                self.messages.truncate(retain);
                self.total_rows = self.messages.last().map_or(0, |m| m.start + m.rows);
            }
            let counts = full.then(|| run_row_counts(&view.runs, width));
            for (index, run) in view.runs.iter().enumerate().skip(first) {
                let (user, assistant) = if self.agent {
                    (MessageKind::Assignment, MessageKind::AgentResult)
                } else {
                    (MessageKind::User, MessageKind::Assistant)
                };
                if let Some(counts) = &counts {
                    self.push_count(MessageSource::User(index), user, counts[index].0);
                } else {
                    self.push(
                        MessageSource::User(index),
                        user,
                        &image_message_text(&run.objective, &run.images),
                        width,
                    );
                }
                self.push_notices_after(Some(MessageSource::User(index)), width);
                let groups = if self.details {
                    run.tools
                        .iter()
                        .enumerate()
                        .map(|(index, tool)| {
                            (
                                index..index + 1,
                                crate::presentation::tool_detail_text(tool),
                            )
                        })
                        .collect::<Vec<_>>()
                } else {
                    crate::presentation::tool_groups(&run.tools).collect()
                };
                for (range, text) in groups {
                    self.push(
                        MessageSource::Tool(index, range.start),
                        if self.details {
                            MessageKind::ToolDetails
                        } else {
                            MessageKind::Tool
                        },
                        &text,
                        width,
                    );
                    for tool_index in range {
                        self.push_notices_after(
                            Some(MessageSource::Tool(index, tool_index)),
                            width,
                        );
                    }
                }
                if self.agent {
                    for (note, text) in run.steering_notes.iter().enumerate() {
                        self.push_owned(
                            MessageSource::Steering(index, note),
                            MessageKind::SteeringNote,
                            text.clone(),
                            width,
                        );
                        self.push_notices_after(Some(MessageSource::Steering(index, note)), width);
                    }
                }
                for (verdict_index, (_, verdict)) in run.supervisor.verdicts().iter().enumerate() {
                    let source = MessageSource::Supervisor(index, verdict_index);
                    if self.details {
                        self.push_owned(
                            source,
                            MessageKind::Supervisor,
                            crate::presentation::supervisor_line(verdict),
                            width,
                        );
                    }
                    self.push_notices_after(Some(source), width);
                }
                if self.details {
                    for (call, item, text) in crate::presentation::display_commentary(run) {
                        self.push(
                            MessageSource::Commentary(index, call, item),
                            MessageKind::Commentary,
                            &text,
                            width,
                        );
                    }
                }
                if self.details
                    && run
                        .display
                        .iter()
                        .any(|chunk| chunk.category == arany::DisplayCategory::Limit)
                {
                    self.push_owned(
                        MessageSource::Commentary(index, u32::MAX, 0),
                        MessageKind::Notice,
                        arany::DISPLAY_LIMIT_TEXT.to_owned(),
                        width,
                    );
                }
                if let Some(answer) = &run.assistant_message {
                    if let Some(counts) = &counts {
                        self.push_count(
                            MessageSource::Assistant(index),
                            assistant,
                            counts[index].1.expect("accepted answer"),
                        );
                    } else {
                        self.push(MessageSource::Assistant(index), assistant, answer, width);
                    }
                } else {
                    let live = self
                        .preview
                        .as_ref()
                        .filter(|preview| {
                            preview.run_id == run.id
                                && run.status == arany::RunStatus::Active
                                && crate::presentation::display_call_visible(
                                    run,
                                    preview.call_index,
                                )
                        })
                        .cloned();
                    let text = live
                        .as_ref()
                        .map(|preview| preview.text.answer.clone())
                        .filter(|text| !text.is_empty())
                        .or_else(|| crate::presentation::display_answer(run));
                    if let Some(text) = text {
                        let kind = if run.status == arany::RunStatus::Active {
                            MessageKind::Preview
                        } else {
                            MessageKind::Incomplete
                        };
                        if live.is_some() {
                            self.push_owned(MessageSource::Assistant(index), kind, text, width);
                        } else {
                            self.push(MessageSource::Assistant(index), kind, &text, width);
                        }
                    }
                    if self.details
                        && let Some(preview) = live
                    {
                        for (item, text) in preview.text.commentary.iter().enumerate() {
                            let source =
                                MessageSource::Commentary(index, preview.call_index, item as u8);
                            if !self.messages.iter().any(|message| message.source == source) {
                                self.push_owned(
                                    source,
                                    MessageKind::Commentary,
                                    text.clone(),
                                    width,
                                );
                            }
                        }
                    }
                }
                self.push_notices_after(Some(MessageSource::Assistant(index)), width);
            }
            self.session = Some(view.id);
            self.width = width;
            self.last_run = last_run;
            self.run_count = view.runs.len();
            self.last_answered = last_answered;
            self.sequence = view.last_sequence;
            self.dirty = false;
            self.preview_dirty = false;
            if !self.follow_tail && self.total_rows > previous_rows {
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
        let rows = if matches!(
            kind,
            MessageKind::Supervisor | MessageKind::Tool | MessageKind::ToolDetails
        ) {
            message_rows(kind, text, width).len()
        } else {
            message_row_count(text, width)
        };
        self.push_count(source, kind, rows);
    }

    pub(super) fn set_preview(&mut self, preview: Option<arany::DisplayUpdate>) {
        self.preview = preview;
        self.preview_dirty = true;
    }

    pub(super) fn set_details(&mut self, details: bool) {
        if self.details != details {
            self.details = details;
            self.dirty = true;
            // The rows change, so a highlighted match would mark the wrong row.
            self.matched_row = None;
        }
    }

    fn push_owned(&mut self, source: MessageSource, kind: MessageKind, text: String, width: usize) {
        self.push(source, kind, &text, width);
        self.messages
            .last_mut()
            .expect("added display message")
            .text = Some(text);
    }

    pub(super) fn live_rows(&self, view: &SessionView) -> Vec<HistoryRow> {
        let Some(run) = view
            .runs
            .last()
            .filter(|run| run.status == arany::RunStatus::Active)
        else {
            return Vec::new();
        };
        let text = self
            .preview
            .as_ref()
            .filter(|preview| preview.run_id == run.id)
            .map(|preview| preview.text.answer.clone())
            .filter(|text| !text.is_empty())
            .or_else(|| crate::presentation::display_answer(run));
        let Some(text) = text else {
            return Vec::new();
        };
        let rows = display_rows(MessageKind::Preview, &text, self.width);
        rows.into_iter()
            .rev()
            .take(self.height)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    fn push_count(&mut self, source: MessageSource, kind: MessageKind, rows: usize) {
        self.messages.push(MessageIndex {
            source,
            kind,
            start: self.total_rows,
            rows,
            text: None,
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

    pub(super) fn record_notice(&mut self, notice: super::TerminalNotice<'_>) {
        let super::TerminalNotice::History(text) = notice else {
            return;
        };
        let (kind, text) = notice_kind(text);
        let bounded = bounded_notice_text(text);
        let text = bounded.as_ref();
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
                    } else {
                        self.last_find = Some(query);
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
        let page = self.height.saturating_sub(1).max(1);
        match input {
            TerminalInput::PageUp => self.scroll(true, page),
            TerminalInput::PageDown => self.scroll(false, page),
            TerminalInput::PointerScrollUp { .. } => self.scroll(true, 3),
            TerminalInput::PointerScrollDown { .. } => self.scroll(false, 3),
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

    pub(super) fn scroll(&mut self, up: bool, rows: usize) -> bool {
        if up {
            let top = self.top.saturating_sub(rows);
            if top < self.top {
                self.top = top;
                self.follow_tail = false;
            }
        } else {
            self.top = self.top.saturating_add(rows).min(self.tail());
            self.follow_tail = self.top == self.tail();
            if self.follow_tail {
                self.unseen_new_content = false;
            }
        }
        true
    }

    // `row` and `column` are offsets into the visible rows; one click anchors, two pick a word, three a line.
    pub(super) fn select_at(&mut self, view: &SessionView, row: usize, column: usize, clicks: u8) {
        self.selection = self
            .point_at(view, row, column)
            .and_then(|point| match clicks {
                1 => Some(Selection {
                    anchor: point,
                    head: point,
                    shown: false,
                }),
                2 => self.word_at(view, point),
                _ => self.line_at(view, point),
            });
    }

    pub(super) fn extend_selection(&mut self, view: &SessionView, row: usize, column: usize) {
        let head = self.point_at(view, row, column);
        if let Some((selection, head)) = self.selection.as_mut().zip(head) {
            selection.shown = selection.shown || head != selection.anchor;
            selection.head = head;
        }
    }

    pub(super) fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    pub(super) fn has_selection(&self) -> bool {
        self.selection.is_some_and(|selection| selection.shown)
    }

    pub(super) fn clear_selection(&mut self) -> bool {
        self.selection.take().is_some()
    }

    // The escaped selected text without headings, rails or wrap breaks; refused before it exceeds the copy limit.
    pub(super) fn selection_text(
        &self,
        view: &SessionView,
    ) -> Option<Result<String, &'static str>> {
        let ((first, start), (last, end)) = self.selection_range(view)?;
        let mut parts = Vec::new();
        let mut bytes = 0usize;
        for index in first..=last {
            let message = &self.messages[index];
            let source = message_source(view, message, &self.notices);
            let from = source.floor(if index == first { start } else { 0 });
            let to = source.floor(if index == last { end } else { source.len() });
            let kept = plain_ranges(&chrome(message.kind, &source.head), from, to);
            let plain_bytes = kept.iter().map(ExactSizeIterator::len).sum::<usize>();
            if plain_bytes == 0 {
                continue;
            }
            let separator = if parts.is_empty() { 0 } else { 2 };
            // Escaping never shrinks text, so the plain length refuses an oversized copy before it is joined.
            if bytes.saturating_add(separator + plain_bytes) > MAX_COPY_BYTES {
                return Some(Err(COPY_TOO_LARGE));
            }
            let mut plain = String::with_capacity(plain_bytes);
            for range in kept {
                source.push_to(&mut plain, range);
            }
            bytes = bytes.saturating_add(separator + display_bytes(&plain));
            if bytes > MAX_COPY_BYTES {
                return Some(Err(COPY_TOO_LARGE));
            }
            parts.push(plain);
        }
        (!parts.is_empty()).then(|| {
            Ok(parts
                .iter()
                .map(|plain| display_text(plain))
                .collect::<Vec<_>>()
                .join("\n\n"))
        })
    }

    fn point_at(&self, view: &SessionView, row: usize, column: usize) -> Option<TextPoint> {
        let last = self.total_rows.checked_sub(1)?;
        let (row, column) = match self.top.checked_add(row) {
            Some(row) if row <= last => (row, column),
            _ => (last, usize::MAX),
        };
        let message = self.messages.get(
            self.messages
                .partition_point(|message| message.start + message.rows <= row),
        )?;
        let text = message_text(view, message, &self.notices);
        let boundary = |byte| TextPoint {
            source: message.source,
            byte,
            boundary: true,
        };
        let offset = row - message.start;
        if offset == 0 && has_heading(message.kind) {
            return Some(boundary(0));
        }
        let rows = message_rows(message.kind, &text, self.width);
        let Some((start, end)) = row_span(&text, &rows, offset) else {
            return Some(boundary(text.len()));
        };
        let indent = row_indent(message.kind);
        if column < indent {
            return Some(boundary(start));
        }
        Some(
            row_cells(&text, start, end, indent)
                .find(|(_, cell, width)| column < cell + width)
                .map_or(boundary(end), |(byte, _, _)| TextPoint {
                    source: message.source,
                    byte,
                    boundary: false,
                }),
        )
    }

    fn word_at(&self, view: &SessionView, point: TextPoint) -> Option<Selection> {
        if point.boundary {
            return None;
        }
        let message = self
            .messages
            .iter()
            .find(|message| message.source == point.source)?;
        let text = message_text(view, message, &self.notices);
        let (line_start, line_end) = line_bounds(&text, point.byte);
        let graphemes = text[line_start..line_end]
            .grapheme_indices(true)
            .map(|(byte, grapheme)| (line_start + byte, grapheme))
            .collect::<Vec<_>>();
        let at = graphemes.iter().position(|(byte, _)| *byte == point.byte)?;
        let at_index = |index: usize| TextPoint {
            byte: graphemes[index].0,
            ..point
        };
        if let Some((first, last)) = url_span(&graphemes, at) {
            return Some(Selection {
                anchor: at_index(first),
                head: at_index(last),
                shown: true,
            });
        }
        let word = |grapheme: &str| {
            grapheme
                .chars()
                .all(|character| character.is_alphanumeric() || "_-./~".contains(character))
        };
        let (first, last) = if word(graphemes[at].1) {
            (
                graphemes[..at]
                    .iter()
                    .rposition(|(_, grapheme)| !word(grapheme))
                    .map_or(0, |index| index + 1),
                graphemes[at..]
                    .iter()
                    .position(|(_, grapheme)| !word(grapheme))
                    .map_or(graphemes.len(), |index| at + index)
                    - 1,
            )
        } else {
            (at, at)
        };
        Some(Selection {
            anchor: at_index(first),
            head: at_index(last),
            shown: true,
        })
    }

    fn line_at(&self, view: &SessionView, point: TextPoint) -> Option<Selection> {
        let message = self
            .messages
            .iter()
            .find(|message| message.source == point.source)?;
        let (start, end) = line_bounds(&message_text(view, message, &self.notices), point.byte);
        let boundary = |byte| TextPoint {
            source: point.source,
            byte,
            boundary: true,
        };
        Some(Selection {
            anchor: boundary(start),
            head: boundary(end),
            shown: start < end,
        })
    }

    // Message indices and source bytes of the selection, ordered, with an exclusive end.
    fn selection_range(&self, view: &SessionView) -> Option<((usize, usize), (usize, usize))> {
        let selection = self.selection.filter(|selection| selection.shown)?;
        let locate = |point: TextPoint| {
            self.messages
                .iter()
                .position(|message| message.source == point.source)
                .map(|index| (index, point))
        };
        let mut low = locate(selection.anchor)?;
        let mut high = locate(selection.head)?;
        let order = |(index, point): (usize, TextPoint)| (index, point.byte, !point.boundary);
        if order(high) < order(low) {
            std::mem::swap(&mut low, &mut high);
        }
        let end = if high.1.boundary {
            high.1.byte
        } else {
            let source = message_source(view, &self.messages[high.0], &self.notices);
            let byte = source.floor(high.1.byte);
            source
                .grapheme_at(byte)
                .filter(|grapheme| *grapheme != "\n")
                .map_or(byte, |grapheme| byte + grapheme.len())
        };
        Some(((low.0, low.1.byte), (high.0, end)))
    }

    fn search(&mut self, view: &SessionView, query: &str) -> bool {
        if query.is_empty() {
            return false;
        }
        for message in self.messages.iter().rev() {
            let text = message_text(view, message, &self.notices);
            if let Some(byte) = text.find(query) {
                let offset = if matches!(
                    message.kind,
                    MessageKind::Supervisor | MessageKind::Tool | MessageKind::ToolDetails
                ) {
                    message_rows(message.kind, &text, self.width)
                        .iter()
                        .rposition(|row| row.source_byte <= byte)
                        .unwrap_or(0)
                } else {
                    message_row_count(&text[..byte], self.width).saturating_sub(2)
                };
                self.show_match(message.start + offset);
                return true;
            }
        }
        false
    }

    fn show_match(&mut self, row: usize) {
        self.top = row.saturating_sub(1).min(self.tail());
        self.follow_tail = false;
        self.matched_row = Some(row);
    }

    // Moves to the nearest match of the last query above (`older`) or below the current match.
    fn step_match(&mut self, view: &SessionView, older: bool) -> bool {
        let Some(query) = self.last_find.clone() else {
            return false;
        };
        let Some(current) = self.matched_row else {
            return self.search(view, &query);
        };
        let rows = |message: &MessageIndex| {
            let text = message_text(view, message, &self.notices);
            if !text.contains(query.as_str()) {
                return Vec::new();
            }
            let rows = message_rows(message.kind, &text, self.width);
            text.match_indices(query.as_str())
                .map(|(byte, _)| {
                    message.start
                        + rows
                            .partition_point(|row| row.source_byte <= byte)
                            .saturating_sub(1)
                })
                .collect::<Vec<_>>()
        };
        let found = if older {
            self.messages
                .iter()
                .rev()
                .filter(|message| message.start < current)
                .find_map(|message| rows(message).into_iter().rev().find(|row| *row < current))
        } else {
            self.messages
                .iter()
                .filter(|message| message.start + message.rows > current + 1)
                .find_map(|message| rows(message).into_iter().find(|row| *row > current))
        };
        found.inspect(|row| self.show_match(*row)).is_some()
    }

    // The transcript pager's less-style keys; find typing, pointer and process input pass through.
    pub(super) fn pager_input(&mut self, input: super::TerminalInput, view: &SessionView) -> Pager {
        use super::TerminalInput as Key;
        if self.find.is_some() {
            return if self.handle(input, view) {
                Pager::Handled
            } else {
                Pager::Pass
            };
        }
        let half = (self.height / 2).max(1);
        match input {
            Key::Character('q') | Key::Escape | Key::Transcript => return Pager::Close,
            Key::Character('[') => return Pager::Dump,
            Key::Character('/') | Key::HistoryFind => {
                self.handle(Key::HistoryFind, view);
            }
            Key::Character(next @ ('n' | 'N')) => {
                self.step_match(view, next == 'n');
            }
            Key::Character('j') | Key::Down => {
                self.scroll(false, 1);
            }
            Key::Character('k') | Key::Up => {
                self.scroll(true, 1);
            }
            Key::Character('g') | Key::Home => {
                self.scroll(true, usize::MAX);
            }
            Key::Character('G') | Key::End | Key::HistoryLive => {
                self.scroll(false, usize::MAX);
            }
            Key::HalfPageUp => {
                self.scroll(true, half);
            }
            Key::EndOfInput => {
                self.scroll(false, half);
            }
            Key::PageUp | Key::PageDown => {
                self.handle(input, view);
            }
            Key::Character(_)
            | Key::Newline
            | Key::Backspace
            | Key::Delete
            | Key::Left
            | Key::Right
            | Key::WordLeft
            | Key::WordRight
            | Key::BackspaceWord
            | Key::Tab
            | Key::Submit
            | Key::CycleApprovalMode
            | Key::QuickActions
            | Key::ClipboardPaste => {}
            _ => return Pager::Pass,
        }
        Pager::Handled
    }

    pub(super) fn pager_status(&self) -> String {
        if let Some(status) = self.status().filter(|_| self.find.is_some()) {
            return status;
        }
        match self.width {
            0..=23 => "q close · [ dump",
            24..=49 => "/ n N · [ dump · q close",
            50..=79 => "/ find · n/N · j/k · g/G · [ scrollback · q close",
            _ => "Transcript · / find · n/N · j/k · g/G · Ctrl+U/D · [ scrollback · q close",
        }
        .to_owned()
    }

    pub(super) fn find_open(&self) -> bool {
        self.find.is_some()
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
        let range = self.selection_range(view);
        for (index, message) in self.messages.iter().enumerate() {
            if message.start + message.rows <= self.top {
                continue;
            }
            if message.start >= end {
                break;
            }
            let text = message_text(view, message, &self.notices);
            let rows = message_rows(message.kind, &text, self.width);
            let chrome = chrome(message.kind, &text);
            for offset in 0..rows.len() {
                let row = message.start + offset;
                if row >= self.top && row < end {
                    let selected = range.and_then(|range| {
                        selected_cells(message.kind, &text, &rows, &chrome, offset, index, range)
                    });
                    visible.push(HistoryRow {
                        speaker: message.kind,
                        heading: offset == 0,
                        matched: self.matched_row == Some(row),
                        text: rows[offset].text.clone(),
                        selected,
                    });
                }
            }
        }
        visible
    }
}

pub(super) const SCROLLBACK_RETURN_CUE: &str = "Enter returns to Arany";
const SCROLLBACK_HEADING: &str = "Arany transcript:";
const SCROLLBACK_OMITTED: &str = "Earlier Runs omitted; arany show prints the whole conversation";
const SCROLLBACK_NONE_FIT: &str =
    "The newest Run exceeds the transcript limit; arany show prints the whole conversation";

// The pager's `[` text: the newest whole Runs read like chat (objective, details, committed
// answer), escaped line by line with CRLF endings, within the Run and byte bounds.
pub(super) fn scrollback_transcript(view: &SessionView) -> String {
    let emitted = |indent: &str, text: &str| indent.len() + escaped_terminal_bytes(text) + 2;
    let mut remaining = MAX_CLASSIC_HISTORY_BYTES
        - emitted("", SCROLLBACK_HEADING)
        - emitted("", SCROLLBACK_OMITTED).max(emitted("", SCROLLBACK_NONE_FIT))
        - emitted("", SCROLLBACK_RETURN_CUE);
    let mut blocks = Vec::new();
    for run in view.runs.iter().rev().take(MAX_CLASSIC_HISTORY_RUNS) {
        let objective = image_message_text(&run.objective, &run.images);
        let details = super::linear::transcript_details(run);
        let mut lines = vec![("", "You:")];
        lines.extend(objective.split('\n').map(|text| ("  ", text)));
        lines.extend(
            details
                .iter()
                .flat_map(|part| part.split('\n'))
                .map(|text| ("", text)),
        );
        if let Some(answer) = &run.assistant_message {
            lines.push(("", "Arany:"));
            lines.extend(answer.split('\n').map(|text| ("  ", text)));
        }
        lines.push(("", ""));
        // Sizes are measured before escaping allocates, so an oversized Run costs no copy.
        let bytes = lines
            .iter()
            .map(|(indent, text)| emitted(indent, text))
            .sum::<usize>();
        if bytes > remaining {
            break;
        }
        remaining -= bytes;
        blocks.push(
            lines
                .iter()
                .map(|(indent, text)| format!("{indent}{}\r\n", escape_terminal(text)))
                .collect::<String>(),
        );
    }
    let mut text = format!("{SCROLLBACK_HEADING}\r\n");
    let cue = if blocks.is_empty() {
        SCROLLBACK_NONE_FIT
    } else {
        SCROLLBACK_OMITTED
    };
    if blocks.len() < view.runs.len() {
        text.push_str(cue);
        text.push_str("\r\n");
    }
    text.extend(blocks.into_iter().rev());
    text.push_str(SCROLLBACK_RETURN_CUE);
    text.push_str("\r\n");
    text
}

pub(super) fn notice_kind(text: &str) -> (MessageKind, &str) {
    match text.strip_prefix("Error:") {
        Some(body) => (MessageKind::Error, body.trim_start()),
        None => (MessageKind::Notice, text),
    }
}

pub(super) fn bounded_notice_text(text: &str) -> Cow<'_, str> {
    if text.len() <= MAX_NOTICE_BYTES {
        return Cow::Borrowed(text);
    }
    let mut end = MAX_NOTICE_BYTES - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…", &text[..end]))
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

pub(super) fn classic_projection(view: &SessionView, width: usize) -> ClassicProjection {
    let first_candidate = view.runs.len().saturating_sub(MAX_CLASSIC_HISTORY_RUNS);
    let mut first_selected = view.runs.len();
    let mut bytes = 0usize;
    let mut rows = 0usize;
    for index in (first_candidate..view.runs.len()).rev() {
        let cost = classic_run_cost(&view.runs[index], width);
        if bytes.saturating_add(cost.0) > MAX_CLASSIC_HISTORY_BYTES
            || rows.saturating_add(cost.1) > MAX_CLASSIC_HISTORY_ROWS
        {
            break;
        }
        bytes += cost.0;
        rows += cost.1;
        first_selected = index;
    }
    ClassicProjection {
        first_run_index: view.runs.get(first_selected).map(|_| first_selected),
        omitted_history: first_selected > 0,
    }
}

fn classic_run_cost(run: &RunView, width: usize) -> (usize, usize) {
    let mut user_body_bytes = crate::presentation::escaped_terminal_bytes(&run.objective);
    let mut user_body_rows = if run.images.is_empty() || !run.objective.is_empty() {
        message_row_count(&run.objective, width).saturating_sub(2)
    } else {
        0
    };
    let mut has_user_text = !run.objective.is_empty();
    for (index, image) in run.images.iter().enumerate() {
        if has_user_text {
            user_body_bytes = user_body_bytes.saturating_add(1);
        }
        let description = crate::presentation::image_description(image, index);
        user_body_bytes = user_body_bytes
            .saturating_add(crate::presentation::escaped_terminal_bytes(&description));
        user_body_rows =
            user_body_rows.saturating_add(message_row_count(&description, width).saturating_sub(2));
        has_user_text = true;
    }
    let (mut bytes, mut rows) =
        classic_message_cost(MessageKind::User, user_body_bytes, user_body_rows);
    let mut add_message = |kind, text: &str| {
        let (message_bytes, message_rows) = if kind == MessageKind::Tool {
            let rendered = message_rows(kind, text, width);
            (
                rendered.iter().map(|row| row.text.len()).sum(),
                rendered.len(),
            )
        } else {
            classic_message_cost(
                kind,
                crate::presentation::escaped_terminal_bytes(text),
                message_row_count(text, width).saturating_sub(2),
            )
        };
        bytes = bytes.saturating_add(message_bytes);
        rows = rows.saturating_add(message_rows);
    };
    for (_, summary) in crate::presentation::settled_tool_groups(run) {
        add_message(MessageKind::Tool, &summary);
    }
    if let Some(answer) = &run.assistant_message {
        add_message(MessageKind::Assistant, answer);
    } else if !matches!(
        run.status,
        arany::RunStatus::Pending | arany::RunStatus::Active
    ) && let Some(chunks) = crate::presentation::display_answer_chunks(run)
    {
        let mut chunk_count = 0usize;
        let mut body_rows = 0usize;
        for chunk in chunks {
            bytes = bytes.saturating_add(crate::presentation::escaped_terminal_bytes(chunk));
            body_rows = body_rows.saturating_add(message_row_count(chunk, width).saturating_sub(2));
            chunk_count = chunk_count.saturating_add(1);
        }
        if chunk_count > 0 {
            bytes = bytes.saturating_add(1);
            let (message_bytes, message_rows) = classic_message_cost(
                MessageKind::Incomplete,
                0,
                body_rows.saturating_add(chunk_count.saturating_sub(1)),
            );
            bytes = bytes.saturating_add(message_bytes);
            rows = rows.saturating_add(message_rows);
        }
    }
    (bytes, rows)
}

fn classic_message_cost(kind: MessageKind, body_bytes: usize, body_rows: usize) -> (usize, usize) {
    let body_decoration = if kind == MessageKind::User {
        native_marker(kind, false).len()
    } else {
        2 + native_marker(kind, false).len()
    };
    let bytes = body_bytes
        .saturating_add(body_rows.saturating_mul(body_decoration))
        .saturating_add(message_heading(kind).len())
        .saturating_add(native_marker(kind, true).len());
    (bytes, body_rows.saturating_add(3))
}

fn previous_run(view: &SessionView, count: usize) -> Option<RunId> {
    count
        .checked_sub(1)
        .and_then(|index| view.runs.get(index))
        .map(|run| run.id)
}

fn message_text<'a>(
    view: &'a SessionView,
    message: &'a MessageIndex,
    notices: &'a VecDeque<Notice>,
) -> Cow<'a, str> {
    if let Some(text) = &message.text {
        return Cow::Borrowed(text);
    }
    match message.source {
        MessageSource::Steering(run, note) => Cow::Borrowed(&view.runs[run].steering_notes[note]),
        MessageSource::Supervisor(run, verdict) => Cow::Owned(
            crate::presentation::supervisor_line(&view.runs[run].supervisor.verdicts()[verdict].1),
        ),
        MessageSource::Tool(run, tool) => Cow::Owned(if message.kind == MessageKind::ToolDetails {
            crate::presentation::tool_detail_text(&view.runs[run].tools[tool])
        } else {
            crate::presentation::tool_groups(&view.runs[run].tools[tool..])
                .next()
                .expect("indexed Tool group")
                .1
        }),
        MessageSource::Commentary(_, u32::MAX, _) => Cow::Borrowed(arany::DISPLAY_LIMIT_TEXT),
        MessageSource::Commentary(run, call, item) => Cow::Owned(
            view.runs[run]
                .display
                .iter()
                .filter(|chunk| {
                    chunk.call_index == call
                        && chunk.item == item
                        && chunk.category == arany::DisplayCategory::Commentary
                })
                .map(|chunk| chunk.text.as_str())
                .collect(),
        ),
        MessageSource::User(run) => {
            image_message_text(&view.runs[run].objective, &view.runs[run].images)
        }
        MessageSource::Assistant(run) => view.runs[run].assistant_message.as_deref().map_or_else(
            || Cow::Owned(crate::presentation::display_answer(&view.runs[run]).unwrap_or_default()),
            Cow::Borrowed,
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

pub(super) fn display_rows(kind: MessageKind, text: &str, width: usize) -> Vec<HistoryRow> {
    message_rows(kind, text, width)
        .into_iter()
        .enumerate()
        .map(|(index, row)| HistoryRow {
            text: row.text,
            speaker: kind,
            heading: index == 0,
            matched: false,
            selected: None,
        })
        .collect()
}

// A message's text as a bounded head and a borrowed tail; only selected bytes are ever joined.
struct SourceText<'a> {
    head: Cow<'a, str>,
    tail: &'a str,
}

impl SourceText<'_> {
    fn len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    fn floor(&self, byte: usize) -> usize {
        match byte.checked_sub(self.head.len()) {
            Some(tail) if tail > 0 => self.head.len() + floor_boundary(self.tail, tail),
            _ => floor_boundary(&self.head, byte),
        }
    }

    fn grapheme_at(&self, byte: usize) -> Option<&str> {
        match byte.checked_sub(self.head.len()) {
            Some(tail) => self.tail.get(tail..)?.graphemes(true).next(),
            None => self.head.get(byte..)?.graphemes(true).next(),
        }
    }

    fn push_to(&self, plain: &mut String, range: Range<usize>) {
        let split = self.head.len();
        plain.push_str(&self.head[range.start.min(split)..range.end.min(split)]);
        plain.push_str(&self.tail[range.start.max(split) - split..range.end.max(split) - split]);
    }
}

fn message_source<'a>(
    view: &'a SessionView,
    message: &'a MessageIndex,
    notices: &'a VecDeque<Notice>,
) -> SourceText<'a> {
    if message.text.is_none()
        && message.kind == MessageKind::ToolDetails
        && let MessageSource::Tool(run, tool) = message.source
    {
        let (head, tail) = crate::presentation::tool_detail_parts(&view.runs[run].tools[tool]);
        return SourceText {
            head: Cow::Owned(head),
            tail,
        };
    }
    SourceText {
        head: message_text(view, message, notices),
        tail: "",
    }
}

// Tool summary markers that the selection highlights and copies leave out.
fn chrome(kind: MessageKind, text: &str) -> Vec<Range<usize>> {
    if !matches!(kind, MessageKind::Tool | MessageKind::ToolDetails) {
        return Vec::new();
    }
    let mut start = 0;
    let mut ranges = Vec::new();
    for line in text.split_inclusive('\n').take(2) {
        if let Some(marker) = ["⏺ ", "  ⎿ "]
            .into_iter()
            .find(|marker| line.starts_with(marker))
        {
            ranges.push(start..start + marker.len());
        }
        start += line.len();
    }
    ranges
}

fn plain_ranges(chrome: &[Range<usize>], from: usize, to: usize) -> Vec<Range<usize>> {
    let mut kept = Vec::new();
    let mut at = from;
    for cut in chrome {
        if cut.start >= to {
            break;
        }
        if cut.end <= at {
            continue;
        }
        if cut.start > at {
            kept.push(at..cut.start);
        }
        at = cut.end;
    }
    if at < to {
        kept.push(at..to);
    }
    kept
}

// A double click inside `scheme://...` selects the whole URL, trimming trailing punctuation.
fn url_span(graphemes: &[(usize, &str)], at: usize) -> Option<(usize, usize)> {
    let blank = |grapheme: &str| grapheme.chars().all(char::is_whitespace);
    let first = graphemes[..at]
        .iter()
        .rposition(|(_, grapheme)| blank(grapheme))
        .map_or(0, |index| index + 1);
    let end = graphemes[at..]
        .iter()
        .position(|(_, grapheme)| blank(grapheme))
        .map_or(graphemes.len(), |index| at + index);
    let token = graphemes[first..end]
        .iter()
        .map(|(_, grapheme)| *grapheme)
        .collect::<Vec<_>>();
    let separator = token.windows(3).position(|part| part == [":", "/", "/"])?;
    let scheme = token[..separator]
        .iter()
        .rposition(|grapheme| {
            !grapheme
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
        })
        .map_or(0, |index| index + 1);
    if scheme == separator
        || !token[scheme]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
    {
        return None;
    }
    let mut last = token.len().checked_sub(1)?;
    while last > separator + 2 {
        let closing = |open: &str, close: &str| {
            token[last] == close
                && token[scheme..=last].iter().filter(|g| **g == close).count()
                    > token[scheme..=last].iter().filter(|g| **g == open).count()
        };
        if [".", ",", ";", ":", "!", "?", "'", "\""].contains(&token[last])
            || closing("(", ")")
            || closing("[", "]")
            || closing("<", ">")
        {
            last -= 1;
        } else {
            break;
        }
    }
    let (url_first, url_last) = (first + scheme, first + last);
    (last > separator + 2 && (url_first..=url_last).contains(&at)).then_some((url_first, url_last))
}

fn has_heading(kind: MessageKind) -> bool {
    !matches!(
        kind,
        MessageKind::Tool | MessageKind::ToolDetails | MessageKind::Supervisor
    )
}

fn row_indent(kind: MessageKind) -> usize {
    if kind == MessageKind::Tool { 0 } else { 2 }
}

fn floor_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn line_bounds(text: &str, byte: usize) -> (usize, usize) {
    let byte = floor_boundary(text, byte);
    (
        text[..byte].rfind('\n').map_or(0, |index| index + 1),
        text[byte..]
            .find('\n')
            .map_or(text.len(), |index| byte + index),
    )
}

// The source bytes one rendered body row shows, excluding its line break.
fn row_span(text: &str, rows: &[MessageRow], offset: usize) -> Option<(usize, usize)> {
    let start = floor_boundary(text, rows.get(offset)?.source_byte);
    let line_end = text[start..]
        .find('\n')
        .map_or(text.len(), |index| start + index);
    let next = rows
        .get(offset + 1)
        .map_or(text.len(), |row| row.source_byte);
    Some((start, floor_boundary(text, next.clamp(start, line_end))))
}

// Source byte, first cell and width of each grapheme in a row, as `message_rows` lays them out.
fn row_cells(
    text: &str,
    start: usize,
    end: usize,
    indent: usize,
) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let mut cell = indent;
    text[start..end]
        .grapheme_indices(true)
        .map(move |(byte, grapheme)| {
            let width = escaped_terminal_width(grapheme);
            let item = (start + byte, cell, width);
            cell += width;
            item
        })
}

fn selected_cells(
    kind: MessageKind,
    text: &str,
    rows: &[MessageRow],
    chrome: &[Range<usize>],
    offset: usize,
    index: usize,
    ((first, start), (last, end)): ((usize, usize), (usize, usize)),
) -> Option<(usize, usize)> {
    if index < first || index > last || (offset == 0 && has_heading(kind)) {
        return None;
    }
    let (row_start, row_end) = row_span(text, rows, offset)?;
    let from = if index == first { start } else { 0 };
    let to = if index == last { end } else { usize::MAX };
    let mut cells = row_cells(text, row_start, row_end, row_indent(kind)).filter(|(byte, _, _)| {
        (from..to).contains(byte) && !chrome.iter().any(|cut| cut.contains(byte))
    });
    let first = cells.next()?;
    let last = cells.last().unwrap_or(first);
    Some((first.1, last.1 + last.2))
}

pub(super) fn native_marker(kind: MessageKind, heading: bool) -> &'static str {
    match (kind, heading) {
        (MessageKind::User | MessageKind::Assignment, true) => "› ",
        (MessageKind::Assistant | MessageKind::AgentResult, true) => "● ",
        (MessageKind::User | MessageKind::Assignment, false) => "│ ",
        _ => "",
    }
}

fn message_heading(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::User => "You:",
        MessageKind::Assistant => "Arany:",
        MessageKind::Assignment => "Assignment:",
        MessageKind::AgentResult => "Agent result:",
        MessageKind::SteeringNote => crate::presentation::STEERING_NOTE_HEADING,
        MessageKind::Notice => "Arany · notice:",
        MessageKind::Error => "Arany · error:",
        MessageKind::Preview => "Arany · provisional:",
        MessageKind::Incomplete => "Arany · incomplete:",
        MessageKind::Tool | MessageKind::ToolDetails => "",
        MessageKind::Commentary => "Arany · commentary:",
        MessageKind::Supervisor => "Supervisor (shadow):",
    }
}

fn message_rows(kind: MessageKind, text: &str, width: usize) -> Vec<MessageRow> {
    if kind == MessageKind::Supervisor {
        let mut rows = message_rows(MessageKind::Commentary, text, width);
        rows.remove(0);
        rows.pop();
        return rows;
    }
    let width = width.min(MAX_ROW_CELLS);
    let heading = message_heading(kind);
    let indent = if kind == MessageKind::Tool { "" } else { "  " };
    let mut rows = if matches!(kind, MessageKind::Tool | MessageKind::ToolDetails) {
        Vec::new()
    } else {
        vec![MessageRow {
            text: safe_truncate(heading, width),
            source_byte: 0,
        }]
    };
    let mut line_start = 0;
    for logical in text.split('\n') {
        let mut row = String::from(indent);
        let mut cells = indent.len();
        let mut row_start = line_start;
        for (byte, grapheme) in logical.grapheme_indices(true) {
            let safe = escape_terminal(grapheme);
            let grapheme_cells = UnicodeWidthStr::width(safe.as_str());
            if cells + grapheme_cells > width && cells > indent.len() {
                rows.push(MessageRow {
                    text: row,
                    source_byte: row_start,
                });
                row = String::from(indent);
                cells = indent.len();
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

    #[test]
    fn a_notice_after_a_steering_note_survives_a_rebuild() {
        let mut view = crate::presentation::team_view();
        let (run, child) = (view.runs[0].id, view.runs[0].agents[2].id);
        view.runs[0].agents[2].steering_notes = vec!["guidance".into()];
        view.runs[0].agents[2].result = None;
        view.runs[0].agents[2].summary = None;
        let transcript =
            crate::presentation::AgentTranscript::from_session(&view, run, child).unwrap();
        let mut history = History::agent();
        history.sync(&transcript.view, 80, 4096);
        history.record_notice(super::super::TerminalNotice::History("After the note"));
        history.dirty = true;
        history.sync(&transcript.view, 80, 4096);
        let kinds: Vec<_> = history
            .messages
            .iter()
            .map(|message| message.kind)
            .collect();
        let note = kinds
            .iter()
            .position(|kind| *kind == MessageKind::SteeringNote)
            .expect("steering note row");
        assert!(matches!(
            history.messages[note].source,
            MessageSource::Steering(_, 0)
        ));
        assert!(matches!(
            history.notices.back().and_then(|notice| notice.after),
            Some(MessageSource::Steering(_, 0))
        ));
        assert_eq!(kinds.get(note + 1), Some(&MessageKind::Notice));
    }
    use arany::{
        AgentDisposition, AgentRole, AgentRunId, CollaborationPolicy, Event, RunConfig,
        RunDisposition, RunStatus, RunView, SessionDefaults, WorkspaceIdentity,
    };
    use arany::{StateRoot, Store};
    use std::time::Instant;

    fn session() -> SessionView {
        SessionView {
            rewinds: Default::default(),
            id: SessionId::new(),
            title: "History".into(),
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
        }
    }

    fn run(objective: &str, answer: &str) -> RunView {
        RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: RunId::new(),
            objective: objective.into(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: Some(answer.into()),
            status: RunStatus::Finished,
            accepted_sequence: 2,
            finished_sequence: Some(3),
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
        }
    }

    #[test]
    fn selection_anchors_to_source_text_and_copies_without_rails_or_wraps() {
        let answer = "alpha beta gamma delta epsilon";
        let mut view = session();
        view.runs.push(run("first objective", answer));
        let mut history = History::default();
        history.sync(&view, 20, 40);
        let row_of = |history: &History, view: &SessionView, needle: &str| {
            history
                .visible(view)
                .iter()
                .position(|row| row.text.contains(needle))
                .expect("rendered row")
        };
        let copied = |history: &History, view: &SessionView| {
            history
                .selection_text(view)
                .map(|text| text.expect("bounded selection"))
        };
        let heading = row_of(&history, &view, "Arany:");
        assert_eq!(
            history.visible(&view)[heading + 1].text,
            "  alpha beta gamma d"
        );
        history.select_at(&view, heading, 0, 1);
        assert!(history.selecting() && !history.has_selection());
        assert_eq!(
            copied(&history, &view),
            None,
            "a click alone selects nothing"
        );
        history.extend_selection(&view, heading + 2, 19);
        assert_eq!(copied(&history, &view).as_deref(), Some(answer));
        let rows = history.visible(&view);
        assert_eq!(rows[heading].selected, None, "headings are never selected");
        assert_eq!(rows[heading + 1].selected, Some((2, 20)));
        assert_eq!(rows[heading + 2].selected, Some((2, 14)));
        assert!(rows[..heading].iter().all(|row| row.selected.is_none()));

        view.runs.push(run("second", "later answer"));
        view.last_sequence = 2;
        history.sync(&view, 20, 40);
        assert_eq!(copied(&history, &view).as_deref(), Some(answer), "append");
        history.sync(&view, 37, 40);
        assert_eq!(copied(&history, &view).as_deref(), Some(answer), "resize");
        let body = row_of(&history, &view, "alpha");
        assert_eq!(history.visible(&view)[body].selected, Some((2, 32)));

        // Dragging up from the answer's trailing row to the user heading copies both bodies.
        history.select_at(&view, body + 1, 0, 1);
        history.extend_selection(&view, 0, 0);
        assert_eq!(
            copied(&history, &view).as_deref(),
            Some("first objective\n\nalpha beta gamma delta epsilon")
        );
        history.select_at(&view, body, 8, 2);
        assert_eq!(copied(&history, &view).as_deref(), Some("beta"));
        history.select_at(&view, body, 8, 3);
        assert_eq!(copied(&history, &view).as_deref(), Some(answer));
        history.select_at(&view, body, 0, 2);
        assert!(
            !history.has_selection(),
            "a double click on the rail selects nothing"
        );
        assert!(!history.clear_selection());

        let hostile = "a\u{1b}]52;c;eA==\u{7}b";
        let mut view = session();
        view.runs.push(run("hostile", hostile));
        let mut history = History::default();
        history.sync(&view, 80, 40);
        let heading = row_of(&history, &view, "Arany:");
        history.select_at(&view, heading, 0, 1);
        history.extend_selection(&view, heading + 1, 79);
        let text = copied(&history, &view).expect("escaped selection");
        assert_eq!(text, super::super::clipboard::display_text(hostile));
        assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'));
        assert!(history.clear_selection());
        assert!(!history.selecting());
    }

    #[test]
    fn tool_selection_copies_plain_text_without_markers_and_urls_whole() {
        let mut view = session();
        let mut first = run(
            "inspect",
            "see https://example.com/a_b?x=1. ok (http://x.y/z) a://",
        );
        let read = |output: &str| {
            crate::presentation::tool_fixture(
                arany::ToolCall::Read {
                    path: "a".into(),
                    offset: None,
                    limit: Some(100),
                },
                output,
            )
        };
        first.tools = vec![read(r#"{"text":"a\nb\n"}"#)];
        view.runs.push(first);
        let mut history = History::default();
        history.sync(&view, 80, 40);
        let row_of = |history: &History, needle: &str| {
            history
                .visible(&view)
                .iter()
                .position(|row| row.text.contains(needle))
                .expect("rendered row")
        };
        let copied = |history: &History| {
            history
                .selection_text(&view)
                .map(|text| text.expect("bounded selection"))
        };
        let tool = row_of(&history, "Read(a)");
        history.select_at(&view, tool, 0, 1);
        history.extend_selection(&view, tool + 1, 79);
        let summary = history.visible(&view)[tool].text.clone();
        let result = history.visible(&view)[tool + 1].text.clone();
        let plain = |row: &str| {
            row.trim_start_matches("⏺ ")
                .trim_start_matches("  ⎿ ")
                .to_owned()
        };
        assert_eq!(
            copied(&history),
            Some(format!("{}\n{}", plain(&summary), plain(&result)))
        );
        let rows = history.visible(&view);
        assert!(rows[tool].selected.is_some_and(|(from, _)| from > 0));
        assert!(rows[tool + 1].selected.is_some_and(|(from, _)| from >= 4));
        history.select_at(&view, tool, 3, 3);
        assert_eq!(copied(&history), Some(plain(&summary)));

        let body = row_of(&history, "https://");
        let answer = "see https://example.com/a_b?x=1. ok (http://x.y/z) a://";
        for (needle, expected) in [
            ("example", "https://example.com/a_b?x=1"),
            ("https", "https://example.com/a_b?x=1"),
            ("x.y", "http://x.y/z"),
            ("ok", "ok"),
            ("a://", "a"),
        ] {
            history.select_at(&view, body, 2 + answer.find(needle).unwrap(), 2);
            assert_eq!(copied(&history).as_deref(), Some(expected), "{needle}");
        }

        history.set_details(true);
        history.sync(&view, 80, 40);
        let tool = row_of(&history, "Read(a)");
        history.select_at(&view, tool, 0, 1);
        history.extend_selection(&view, 1_000, 0);
        let details = crate::presentation::tool_detail_text(&view.runs[0].tools[0])
            .replacen("⏺ ", "", 1)
            .replacen("\n  ⎿ ", "\n", 1);
        let text = copied(&history).expect("details selection");
        assert!(
            text.starts_with(&super::super::clipboard::display_text(&details)),
            "{text:?}"
        );

        // A detail result larger than the limit is refused whole, yet its summary stays copyable.
        let mut view = session();
        let mut large = run("inspect", "done");
        large.tools = vec![read(&"z".repeat(MAX_COPY_BYTES + 1))];
        view.runs.push(large);
        let mut history = History::default();
        history.set_details(true);
        history.sync(&view, 80, 40);
        history.scroll(true, usize::MAX);
        let tool = history
            .visible(&view)
            .iter()
            .position(|row| row.text.contains("Read(a)"))
            .expect("detail row");
        history.select_at(&view, tool, 0, 1);
        history.extend_selection(&view, 1_000_000, 0);
        assert_eq!(history.selection_text(&view), Some(Err(COPY_TOO_LARGE)));
        history.select_at(&view, tool, 3, 3);
        assert!(
            history
                .selection_text(&view)
                .is_some_and(|text| text.is_ok())
        );
    }

    #[test]
    fn selection_copy_refuses_over_the_limit_before_materializing_text() {
        for (answer_bytes, accepted) in [(49_998, true), (49_999, false)] {
            let mut view = session();
            view.runs
                .push(run(&"x".repeat(50_000), &"y".repeat(answer_bytes)));
            let mut history = History::default();
            history.sync(&view, 80, 40);
            history.scroll(true, usize::MAX);
            history.select_at(&view, 0, 0, 1);
            history.extend_selection(&view, 1_000_000, 0);
            let text = history.selection_text(&view).expect("selection");
            assert_eq!(text.is_ok(), accepted, "{answer_bytes} answer bytes");
            match text {
                Ok(text) => assert_eq!(text.len(), MAX_COPY_BYTES),
                Err(error) => assert_eq!(error, COPY_TOO_LARGE),
            }
        }
    }

    #[test]
    fn peer_rows_collapse_reads_but_keep_failure_and_details_at_both_widths() {
        let mut view = session();
        let mut run = run("inspect", "done");
        let read = crate::presentation::tool_fixture(
            arany::ToolCall::Read {
                path: "a".into(),
                offset: None,
                limit: Some(100),
            },
            r#"{"text":"a\nb\n"}"#,
        );
        let mut failed = read.clone();
        failed.observation.as_mut().unwrap().disposition = arany::ToolDisposition::Denied;
        failed.observation.as_mut().unwrap().output = "outside pinned grant".into();
        run.tools = vec![read.clone(), read.clone(), failed, read];
        view.runs.push(run);
        for width in [80, 120] {
            let mut history = History::default();
            history.sync(&view, width, 40);
            let rows = history.visible(&view);
            let text = rows
                .iter()
                .map(|row| row.text.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                text,
                "You:\n  inspect\n\n⏺ Read 2 files\n  ⎿ 4 lines\n\n⏺ Read(a)\n  ⎿ denied: outside pinned grant\n\n⏺ Read(a)\n  ⎿ 2 lines\n\nArany:\n  done\n"
            );
            assert!(
                rows.iter()
                    .all(|row| UnicodeWidthStr::width(row.text.as_str()) <= width)
            );
            let (_, cost) = classic_run_cost(&view.runs[0], width);
            assert_eq!(cost, rows.len() + 2);
            history.set_details(true);
            history.sync(&view, width, 80);
            let details = history
                .visible(&view)
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(details.matches("⏺ Read(a)").count(), 4);
            assert_eq!(details.matches("Arguments:").count(), 4);
        }
        for width in [16, 40, 80, 120] {
            for details in [false, true] {
                let mut history = History::default();
                history.set_details(details);
                history.sync(&view, width, 4096);
                for query in ["Read(a)", "denied", "pinned"] {
                    assert!(history.search(&view, query));
                    assert!(
                        history
                            .visible(&view)
                            .iter()
                            .any(|row| row.matched && row.text.contains(query)),
                        "width={width}, details={details}, query={query}"
                    );
                }
            }
        }
    }

    #[test]
    fn supervisor_verdicts_are_detail_only_width_bounded_and_stable_on_redraw() {
        let mut view = session();
        let mut run = run("objective", "answer");
        run.assistant_message = None;
        run.status = RunStatus::Active;
        run.supervisor = crate::presentation::supervisor_fixture(AgentRunId::new(), 256);
        view.runs.push(run);
        for width in [16, 40, 80, 160] {
            let mut history = History::default();
            history.sync(&view, width, 4096);
            assert!(
                !history
                    .messages
                    .iter()
                    .any(|m| m.kind == MessageKind::Supervisor)
            );
            history.set_details(true);
            history.sync(&view, width, 4096);
            assert_eq!(
                history
                    .messages
                    .iter()
                    .filter(|m| m.kind == MessageKind::Supervisor)
                    .count(),
                256
            );
            let rows = history.visible(&view);
            assert!(
                rows.iter()
                    .all(|r| UnicodeWidthStr::width(r.text.as_str()) <= width)
            );
            let text = rows
                .iter()
                .map(|r| r.text.strip_prefix("  ").unwrap_or(&r.text))
                .collect::<String>();
            assert!(text.contains("Supervisor (shadow):"));
            assert!(text.contains("no action taken"));
            assert!(text.contains("verdict limit reached"));
            history.sync(&view, width, 4096);
            assert_eq!(
                history
                    .messages
                    .iter()
                    .filter(|m| m.kind == MessageKind::Supervisor)
                    .count(),
                256
            );
            let last = history.messages.last().unwrap().start;
            assert!(history.search(&view, "Supervisor"));
            assert_eq!(history.matched_row, Some(last));
            history.record_notice(super::super::TerminalNotice::History(
                "Local notice after shadow verdict",
            ));
            history.dirty = true;
            history.sync(&view, width, 4096);
            assert!(
                history
                    .messages
                    .iter()
                    .any(|m| m.kind == MessageKind::Notice)
            );
            history.set_details(false);
            history.sync(&view, width, 4096);
            assert!(
                !history
                    .messages
                    .iter()
                    .any(|m| m.kind == MessageKind::Supervisor)
            );
            assert!(
                history
                    .messages
                    .iter()
                    .any(|m| m.kind == MessageKind::Notice)
            );
        }
    }

    #[test]
    fn classic_projection_uses_run_order_across_fork_sequence_coordinates() {
        let mut view = session();
        for index in 0..MAX_CLASSIC_HISTORY_RUNS + 2 {
            let mut run = run(&format!("objective {index}"), "answer");
            run.accepted_sequence = if index < 16 {
                index as u64 + 100
            } else {
                index as u64 - 15
            };
            view.runs.push(run);
        }
        view.last_sequence = 10;

        let projection = classic_projection(&view, 80);

        assert_eq!(projection.first_run_index, Some(2));
        assert!(view.runs[2].accepted_sequence > view.last_sequence);
        assert!(view.runs[16].accepted_sequence <= view.last_sequence);
        assert!(projection.omitted_history);
    }

    #[test]
    fn classic_projection_byte_budget_includes_native_headings_and_rails() {
        let mut view = session();
        for index in 0..MAX_CLASSIC_HISTORY_RUNS {
            let mut run = run(&"x".repeat(4093), "a");
            run.accepted_sequence = index as u64 + 2;
            view.runs.push(run);
        }
        view.last_sequence = 100;

        let projection = classic_projection(&view, 80);
        let first = projection.first_run_index.expect("recent suffix");
        let selected_bytes: usize = view.runs[first..]
            .iter()
            .map(|run| classic_run_cost(run, 80).0)
            .sum();

        assert_eq!(first, 1);
        assert!(selected_bytes <= MAX_CLASSIC_HISTORY_BYTES);
        assert!(
            selected_bytes + classic_run_cost(&view.runs[first - 1], 80).0
                > MAX_CLASSIC_HISTORY_BYTES
        );
    }

    #[test]
    fn classic_projection_omits_a_whole_run_when_escaped_bytes_exceed_limit() {
        let mut view = session();
        let mut run = run("objective", &"\u{202e}".repeat(9000));
        run.accepted_sequence = 2;
        view.runs.push(run);
        view.last_sequence = 3;

        let projection = classic_projection(&view, 80);

        assert_eq!(projection.first_run_index, None);
        assert!(projection.omitted_history);
        assert!(
            crate::presentation::escaped_terminal_bytes(
                view.runs[0].assistant_message.as_ref().unwrap()
            ) > MAX_CLASSIC_HISTORY_BYTES
        );
    }

    #[test]
    fn classic_projection_bounds_rendered_rows_from_newlines() {
        let mut view = session();
        let mut run = run(&"line\n".repeat(MAX_CLASSIC_HISTORY_ROWS), "answer");
        run.accepted_sequence = 2;
        view.runs.push(run);
        view.last_sequence = 3;

        let projection = classic_projection(&view, 80);

        assert_eq!(projection.first_run_index, None);
        assert!(projection.omitted_history);
        assert!(message_row_count(&view.runs[0].objective, 80) > MAX_CLASSIC_HISTORY_ROWS);
    }

    #[test]
    fn full_managed_history_can_find_a_run_outside_the_classic_projection() {
        let mut view = session();
        for index in 0..MAX_CLASSIC_HISTORY_RUNS + 2 {
            let mut run = run(&format!("objective {index}"), "answer");
            run.accepted_sequence = index as u64 + 2;
            view.runs.push(run);
        }
        view.last_sequence = 100;
        let projection = classic_projection(&view, 80);
        assert!(projection.omitted_history);
        assert!(projection.first_run_index.unwrap() > 0);

        let mut history = History::default();
        history.sync(&view, 80, 20);
        assert!(history.handle(super::super::TerminalInput::HistoryFind, &view));
        for character in "objective 0".chars() {
            assert!(history.handle(super::super::TerminalInput::Character(character), &view));
        }
        assert!(history.handle(super::super::TerminalInput::Submit, &view));
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.text.contains("objective 0"))
        );
    }

    #[test]
    fn streaming_two_hundred_lines_preserves_reading_anchor_and_replaces_preview() {
        let mut view = session();
        let mut active = run("question", "");
        active.assistant_message = None;
        active.status = arany::RunStatus::Active;
        let id = active.id;
        view.runs.push(active);
        let text = (0..200)
            .map(|line| format!("UNIQUE_LINE_{line:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut history = History::default();
        history.sync(&view, 40, 10);
        for lines in [50, 100, 200] {
            let answer = text.lines().take(lines).collect::<Vec<_>>().join("\n");
            history.set_preview(Some(arany::DisplayUpdate {
                run_id: id,
                agent_run_id: arany::AgentRunId::new(),
                call_index: 0,
                text: std::sync::Arc::new(arany::PreviewText {
                    answer,
                    ..Default::default()
                }),
            }));
            let before = history.visible(&view).first().map(|row| row.text.clone());
            history.sync(&view, 40, 10);
            if lines > 50 {
                assert_eq!(
                    history.visible(&view).first().map(|row| row.text.clone()),
                    before
                );
            } else {
                history.handle(super::super::TerminalInput::PageUp, &view);
            }
        }
        let anchored = history.visible(&view)[0].text.clone();
        view.runs[0].assistant_message = Some(text.clone());
        view.runs[0].status = arany::RunStatus::Finished;
        view.last_sequence += 1;
        history.set_preview(None);
        history.sync(&view, 40, 10);
        assert_eq!(history.visible(&view)[0].text, anchored);
        assert_eq!(
            history
                .messages
                .iter()
                .filter(|message| message.source == MessageSource::Assistant(0))
                .count(),
            1
        );
        assert_eq!(
            history.messages.last().unwrap().kind,
            MessageKind::Assistant
        );
        history.sync(&view, 40, 1000);
        let rows = history.visible(&view);
        for line in text.lines() {
            assert_eq!(
                rows.iter().filter(|row| row.text.trim() == line).count(),
                1,
                "{line}"
            );
        }
        assert!(rows.iter().all(|row| !row.text.contains("provisional")));
    }

    #[test]
    fn history_keeps_the_reading_row_when_a_committed_turn_arrives() {
        let mut view = session();
        view.runs.push(run("first objective", "first answer"));
        view.runs[0].images.push(arany::test_support::test_image());
        view.runs.push(run("second objective", "second answer"));
        let mut full_history = History::default();
        full_history.sync(&view, 80, 20);
        let full_rows = full_history.visible(&view);
        assert!(full_rows.iter().any(|row| {
            row.text == "  Image 1: PNG 1x1, 69 B"
                && row.speaker == MessageKind::User
                && !row.heading
        }));
        assert!(!full_rows.iter().any(|row| row.text.contains("iVBOR")));
        assert!(
            full_rows.iter().any(|row| {
                row.text == "You:" && row.heading && row.speaker == MessageKind::User
            })
        );
        assert!(full_rows.iter().any(|row| {
            row.text == "Arany:" && row.heading && row.speaker == MessageKind::Assistant
        }));
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
        history.record_notice(super::super::TerminalNotice::Transient(
            "Error: temporary progress with changed copy",
        ));
        assert!(
            history.visible(&view).is_empty(),
            "transient copy must not enter reading history"
        );
        history.record_notice(super::super::TerminalNotice::History(
            "Error: Choose /setup\u{1b}[2J\nYour draft is retained",
        ));
        let rows = history.visible(&view);
        assert!(rows[0].heading);
        assert_eq!(rows[0].speaker, MessageKind::Error);
        assert_eq!(rows[0].text, "Arany · error:");
        assert_eq!(rows[1].text, "  Choose /setup\\u{001b}[2J");
        assert_eq!(rows[2].text, "  Your draft is retained");
        let first_notice = history.messages[0].source;
        history.sync(&view, 40, 20);
        assert_eq!(history.visible(&view)[1].text, rows[1].text);
        history.record_notice(super::super::TerminalNotice::History(
            "Error: Choose /setup\u{1b}[2J\nYour draft is retained",
        ));
        assert_eq!(history.notices.len(), 1, "repeated redraw is coalesced");

        let mut active = run("accepted objective", "committed answer");
        active.assistant_message = None;
        view.runs.push(active);
        history.sync(&view, 40, 20);
        history.record_notice(super::super::TerminalNotice::History(
            "Local recovery completed",
        ));
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
        history.record_notice(super::super::TerminalNotice::History(
            "Another local notice",
        ));
        assert_eq!(
            history
                .visible(&view)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
            before
        );

        history.handle(super::super::TerminalInput::HistoryLive, &view);
        history.record_notice(super::super::TerminalNotice::History(
            &"é".repeat(MAX_NOTICE_BYTES / 2 + 10),
        ));
        let bounded = &history.notices.back().expect("bounded notice").text;
        assert!(bounded.len() <= MAX_NOTICE_BYTES);
        assert!(bounded.ends_with('…'));
        for index in 0..MAX_NOTICES {
            history.record_notice(super::super::TerminalNotice::History(&format!(
                "Notice {index}"
            )));
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
        history.record_notice(super::super::TerminalNotice::History("Newest notice"));
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
        history.record_notice(super::super::TerminalNotice::History("Terminal resumed"));
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.text.contains("Terminal resumed")),
            "persistent category must survive wording formerly classified as transient"
        );
    }

    #[test]
    fn pager_keys_scroll_find_step_and_close_like_less() {
        use super::super::TerminalInput as Key;
        let mut view = session();
        for index in 0..6 {
            view.runs.push(run(
                &format!("objective {index}"),
                &format!("needle {index}\nline a\nline b\nneedle again {index}"),
            ));
        }
        let mut history = History::default();
        history.sync(&view, 40, 10);
        assert_eq!((history.top, history.tail()), (44, 44));
        for (input, action, top) in [
            (Key::Character('n'), Pager::Handled, 44),
            (Key::Character('k'), Pager::Handled, 43),
            (Key::Up, Pager::Handled, 42),
            (Key::Character('j'), Pager::Handled, 43),
            (Key::Down, Pager::Handled, 44),
            (Key::HalfPageUp, Pager::Handled, 39),
            (Key::EndOfInput, Pager::Handled, 44),
            (Key::PageUp, Pager::Handled, 35),
            (Key::PageDown, Pager::Handled, 44),
            (Key::Character('g'), Pager::Handled, 0),
            (Key::Character('x'), Pager::Handled, 0),
            (Key::Submit, Pager::Handled, 0),
            (Key::Backspace, Pager::Handled, 0),
            (Key::End, Pager::Handled, 44),
            (Key::Home, Pager::Handled, 0),
            (Key::Character('G'), Pager::Handled, 44),
            (Key::Character('q'), Pager::Close, 44),
            (Key::Escape, Pager::Close, 44),
            (Key::Transcript, Pager::Close, 44),
            (Key::Character('['), Pager::Dump, 44),
            (Key::Interrupt, Pager::Pass, 44),
            (Key::CopySelection, Pager::Pass, 44),
            (Key::Suspend, Pager::Pass, 44),
            (Key::Resize, Pager::Pass, 44),
            (Key::Paste, Pager::Pass, 44),
            (Key::PointerScrollUp { column: 0, row: 0 }, Pager::Pass, 44),
            (
                Key::PointerClick {
                    column: 0,
                    row: 0,
                    clicks: 1,
                },
                Pager::Pass,
                44,
            ),
        ] {
            assert_eq!(history.pager_input(input, &view), action, "{input:?}");
            assert_eq!(history.top, top, "{input:?}");
        }
        assert!(history.follow_tail, "G returns to the live end");

        // `/` finds the newest match; `n` steps to older matches and `N` back to newer ones.
        let matched = |history: &History| {
            history
                .visible(&view)
                .into_iter()
                .find(|row| row.matched)
                .map(|row| row.text)
        };
        assert_eq!(
            history.pager_input(Key::Character('/'), &view),
            Pager::Handled
        );
        for character in "needle".chars() {
            assert_eq!(
                history.pager_input(Key::Character(character), &view),
                Pager::Handled
            );
        }
        assert_eq!(history.pager_input(Key::Submit, &view), Pager::Handled);
        assert_eq!(matched(&history).as_deref(), Some("  needle 5"));
        for (key, expected) in [
            ('n', "  needle again 4"),
            ('n', "  needle 4"),
            ('N', "  needle again 4"),
            ('N', "  needle 5"),
            ('N', "  needle again 5"),
            ('N', "  needle again 5"),
        ] {
            assert_eq!(
                history.pager_input(Key::Character(key), &view),
                Pager::Handled
            );
            assert_eq!(matched(&history).as_deref(), Some(expected), "{key}");
        }

        // Ctrl+F is `/`; while find is open, letters type and Escape closes only the find.
        assert_eq!(history.pager_input(Key::HistoryFind, &view), Pager::Handled);
        assert_eq!(
            history.pager_input(Key::Character('q'), &view),
            Pager::Handled
        );
        assert!(history.pager_status().starts_with("Find: q"));
        assert_eq!(history.pager_input(Key::Escape, &view), Pager::Handled);
        assert!(!history.find_open());
        assert_eq!(
            history.pager_input(Key::Character('/'), &view),
            Pager::Handled
        );
        assert_eq!(history.pager_input(Key::Interrupt, &view), Pager::Pass);
        assert!(
            !history.find_open(),
            "Ctrl+C closes find and keeps its meaning"
        );

        for (width, footer) in [
            (16, "q close · [ dump"),
            (24, "/ n N · [ dump · q close"),
            (50, "/ find · n/N · j/k · g/G · [ scrollback · q close"),
            (
                80,
                "Transcript · / find · n/N · j/k · g/G · Ctrl+U/D · [ scrollback · q close",
            ),
        ] {
            history.sync(&view, width, 10);
            assert_eq!(history.pager_status(), footer);
            assert!(escaped_terminal_width(footer) <= width, "{width}");
        }
    }

    #[test]
    fn scrollback_transcript_keeps_newest_whole_runs_escaped_within_bounds() {
        let mut view = session();
        for index in 0..18 {
            view.runs.push(run(
                &format!("objective {index}\nsecond line"),
                &format!("answer {index} \u{1b}]52;c;eA==\u{7}"),
            ));
        }
        let text = scrollback_transcript(&view);
        assert!(!text.contains(['\u{1b}', '\u{7}']), "{text}");
        let lines = text.split("\r\n").collect::<Vec<_>>();
        assert!(lines.iter().all(|line| !line.contains(['\r', '\n'])));
        assert_eq!(lines[..2], ["Arany transcript:", SCROLLBACK_OMITTED]);
        assert_eq!(
            lines[2..8],
            [
                "You:",
                "  objective 2",
                "  second line",
                "Arany:",
                "  answer 2 \\u{001b}]52;c;eA==\\u{0007}",
                "",
            ]
        );
        assert_eq!(lines.iter().filter(|line| **line == "You:").count(), 16);
        assert!(text.ends_with(
            "  answer 17 \\u{001b}]52;c;eA==\\u{0007}\r\n\r\nEnter returns to Arany\r\n"
        ));

        // CRLF endings and indents count against the byte bound, so line-heavy Runs stop early.
        for (newlines, kept) in [(10_000, true), (20_000, false)] {
            let mut view = session();
            view.runs.push(run("older", "small"));
            view.runs.push(run("newest", &"\n".repeat(newlines)));
            let text = scrollback_transcript(&view);
            assert!(text.len() <= MAX_CLASSIC_HISTORY_BYTES, "{newlines}");
            assert_eq!(text.contains("  newest\r\n"), kept, "{newlines}");
            assert_eq!(text.contains("  older\r\n"), kept, "{newlines}");
            assert!(!text.contains(SCROLLBACK_OMITTED), "{newlines}");
            assert_eq!(text.contains(SCROLLBACK_NONE_FIT), !kept, "{newlines}");
            assert!(text.ends_with("Enter returns to Arany\r\n"));
        }
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
                        workspace_identity: WorkspaceIdentity {
                            device: 1,
                            inode: 1,
                        },
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
                        skill: None,
                        run_id,
                        text: objective,
                        images: Vec::new(),
                    },
                    Event::RunStarted {
                        run_id,
                        config: RunConfig {
                            supervisor: None,
                            worker_policy: None,
                            agent_definitions: None,
                            roster: None,
                            providers: None,
                            provider: "scripted".into(),
                            model: "test-model".into(),
                            effort: None,
                            custom_profile_provenance: None,
                            saved_api_account_id: None,
                            chatgpt_provenance: None,
                            output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                            context_window_tokens: None,
                            policy: CollaborationPolicy::Single,
                            output_token_cap: 4096,
                            provider_concurrency: 1,
                            workspace_device: 1,
                            workspace_inode: 1,
                            user_instruction_digest: None,
                            instruction_sources: Vec::new(),
                            launch_directory: String::new(),
                            include_digests: Vec::new(),
                            history_run_ids: Vec::new(),
                            excluded_history_runs: 0,
                            context_usage: None,
                            compaction_event_sequence: None,
                            compaction_content_digest: None,
                            tool_policy: None,
                        },
                        active_deadline_ms: None,
                    },
                    Event::AgentSpawned {
                        profile: None,
                        assignment: None,
                        definition: None,
                        run_id,
                        agent_run_id,
                        role: AgentRole::Primary,
                        ordinal: 0,
                        objective: None,
                        grant: None,
                    },
                    Event::AgentFinished {
                        worker: None,
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
                        failure: None,
                        active_ms: 0,
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
