use super::inline::{Flow, Lines};
use super::*;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
pub(super) const TAB_CELLS: usize = 4;

pub(super) struct BlockLine {
    pub(super) original: Range<usize>,
    pub(super) envelope: Range<usize>,
    pub(super) content: Range<usize>,
    pub(super) padding: usize,
    pub(super) column: usize,
    pub(super) decoration: super::lists::Decoration,
    pub(super) style: Modifier,
    pub(super) hidden: bool,
    pub(super) table: Option<super::tables::Table>,
    pub(super) flow: Option<Lines>,
}

pub(super) fn project(source: &str) -> Result<Vec<BlockLine>, ()> {
    if source.len() > MAX_MARKDOWN_PARSE_BYTES {
        return Err(());
    }
    let mut lines = source_lines(source)?;
    if source.split("\r\n").any(|part| part.contains('\r')) {
        return Ok(lines);
    }
    let mut projection = Projection::default();
    let parser = Parser::new_ext(source, Options::ENABLE_TABLES);
    for (_, definition) in parser.reference_definitions().iter() {
        let span = definition.span.clone();
        let covered_lines = covered(&mut lines, span.clone());
        // Definitions nested in quotes or list items keep their visible marker line.
        let starts_line = covered_lines.first().is_some_and(|line| {
            source
                .get(line.original.start..span.start)
                .is_some_and(|prefix| prefix.trim_ascii().is_empty())
        });
        if starts_line {
            for line in covered_lines {
                line.hidden = true;
            }
        }
    }
    let mut expanded = 0usize;
    let mut events = parser.into_offset_iter();
    for index in 0..=MAX_MARKDOWN_EVENTS {
        let Some((event, range)) = events.next() else {
            break;
        };
        if index >= MAX_MARKDOWN_EVENTS {
            return Err(());
        }
        if matches!(event, Event::Start(_)) && projection.depth == MAX_MARKDOWN_DEPTH {
            return Err(());
        }
        let definition = super::links::definition(&event, events.reference_definitions());
        super::links::charge(&event, &mut expanded)?;
        projection.event(source, &mut lines, (event, range), definition)?;
    }
    Ok(lines)
}

#[derive(Default)]
struct Projection {
    depth: usize,
    code: Option<Range<usize>>,
    table: Option<super::tables::Table>,
    flow: Option<Flow>,
    tight: Option<Range<usize>>,
    lists: Vec<Option<u64>>,
    items: Vec<usize>,
}

impl Projection {
    fn event(
        &mut self,
        source: &str,
        lines: &mut [BlockLine],
        (event, range): (Event<'_>, Range<usize>),
        definition: Option<Range<usize>>,
    ) -> Result<(), ()> {
        if matches!(
            event,
            Event::Start(
                Tag::List(_)
                    | Tag::Item
                    | Tag::BlockQuote
                    | Tag::CodeBlock(_)
                    | Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::Table(_)
                    | Tag::HtmlBlock
            ) | Event::End(TagEnd::Item | TagEnd::List(_) | TagEnd::BlockQuote)
                | Event::Rule
        ) {
            self.finish_tight(source, lines);
        }
        if self.code.is_none()
            && self.table.is_none()
            && self.flow.is_none()
            && self.items.last() == Some(&self.depth)
            && matches!(
                event,
                Event::Text(_)
                    | Event::Code(_)
                    | Event::InlineHtml(_)
                    | Event::Start(
                        Tag::Emphasis | Tag::Strong | Tag::Link { .. } | Tag::Image { .. }
                    )
            )
        {
            self.flow = Some(Flow::new(Modifier::empty()));
            self.tight = Some(range.clone());
        }
        if let Some(tight) = &mut self.tight {
            tight.end = tight.end.max(range.end);
        }
        if let Some(table) = &mut self.table {
            table.event(source, &event, range.clone(), definition.clone())?;
        }
        if let Some(flow) = &mut self.flow {
            flow.event(source, &event, range.clone(), definition)?;
        }
        match event {
            Event::Start(tag) => {
                self.start(source, lines, tag, range)?;
                self.depth += 1;
            }
            Event::End(tag) => {
                self.depth = self.depth.checked_sub(1).ok_or(())?;
                self.end(source, lines, tag, range);
            }
            Event::Text(text) if self.code.is_some() => {
                code_text(source, lines, range, &text);
            }
            _ => {}
        }
        Ok(())
    }

    fn start(
        &mut self,
        source: &str,
        lines: &mut [BlockLine],
        tag: Tag<'_>,
        range: Range<usize>,
    ) -> Result<(), ()> {
        match tag {
            Tag::List(number) => self.lists.push(number),
            Tag::Item => {
                self.items.push(self.depth + 1);
                let number = self.lists.last_mut().expect("parser item belongs to a list");
                super::lists::item(source, covered(lines, range), *number);
                if let Some(number) = number {
                    *number += 1;
                }
            }
            Tag::BlockQuote => super::lists::quote(source, covered(lines, range)),
            Tag::Table(columns) if self.depth == 0 => {
                self.table = Some(super::tables::Table::new(range, columns.len())?);
            }
            Tag::CodeBlock(kind) => {
                code_block(source, lines, range.clone(), kind)?;
                self.code = Some(range);
            }
            tag @ (Tag::Paragraph | Tag::Heading { .. }) if self.table.is_none() => {
                self.flow = open_flow(source, lines, &tag, range);
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, source: &str, lines: &mut [BlockLine], tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                self.items.pop();
            }
            TagEnd::CodeBlock => {
                if let Some(range) = self.code.take() {
                    finish_code(lines, range);
                }
            }
            TagEnd::Paragraph | TagEnd::Heading(_) => {
                if let Some(flow) = self.flow.take() {
                    finish_flow(source, lines, flow, range);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    finish_table(lines, table);
                }
            }
            _ => {}
        }
    }

    fn finish_tight(&mut self, source: &str, lines: &mut [BlockLine]) {
        if let Some(range) = self.tight.take()
            && let Some(flow) = self.flow.take()
        {
            finish_flow(source, lines, flow, range);
        }
    }
}

fn source_lines(source: &str) -> Result<Vec<BlockLine>, ()> {
    let mut start = 0;
    let mut lines = Vec::new();
    for line in source.split('\n') {
        if lines.len() >= MAX_MARKDOWN_ROWS {
            return Err(());
        }
        let end = start + line.len();
        lines.push(BlockLine {
            original: start..end,
            envelope: start..end,
            content: start..end,
            padding: 0,
            column: 0,
            decoration: super::lists::Decoration::default(),
            style: Modifier::empty(),
            hidden: false,
            table: None,
            flow: None,
        });
        start = end + 1;
    }
    Ok(lines)
}

fn finish_table(lines: &mut [BlockLine], mut table: super::tables::Table) {
    let lines = covered(lines, table.range.clone());
    table.range.start = lines[0].original.start;
    table.finish();
    for line in lines.iter_mut() {
        line.hidden = true;
    }
    lines[0].hidden = false;
    lines[0].table = Some(table);
}

fn covered(lines: &mut [BlockLine], range: Range<usize>) -> &mut [BlockLine] {
    let start = lines.partition_point(|line| line.original.end < range.start);
    let end = if range.is_empty() {
        (start + 1).min(lines.len())
    } else {
        lines.partition_point(|line| line.original.start < range.end)
    };
    &mut lines[start..end.max(start)]
}

fn open_flow(
    source: &str,
    lines: &mut [BlockLine],
    tag: &Tag<'_>,
    range: Range<usize>,
) -> Option<Flow> {
    if matches!(tag, Tag::Paragraph) {
        return Some(Flow::new(Modifier::empty()));
    }
    if source[range.clone()].lines().nth(1).is_some() {
        return Some(Flow::new(Modifier::BOLD));
    }
    heading(source, lines, range);
    None
}

fn finish_flow(source: &str, lines: &mut [BlockLine], mut flow: Flow, range: Range<usize>) {
    let lines = covered(lines, range.clone());
    let end = range.start + source[range].trim_end_matches(['\r', '\n']).len();
    lines[0].envelope.end = end;
    lines[0].flow = Some(flow.finish(lines[0].original.start..end));
    for line in lines.iter_mut() {
        line.hidden = true;
    }
    lines[0].hidden = false;
}

fn heading(source: &str, lines: &mut [BlockLine], range: Range<usize>) {
    let line = &mut covered(lines, range.clone())[0];
    let text = source[range.clone()].trim_end_matches(['\r', '\n']);
    let body = text.trim_start_matches('#').trim_start_matches([' ', '\t']);
    let start = range.start + text.len() - body.len();
    let body = body.trim_end_matches([' ', '\t']);
    let closing = body.trim_end_matches('#');
    let body = if closing.is_empty() || closing.ends_with([' ', '\t']) {
        closing.trim_end_matches([' ', '\t'])
    } else {
        body
    };
    line.content = start..start + body.len();
    line.envelope.end = range.end;
    line.style = Modifier::BOLD;
}

fn code_block(
    source: &str,
    lines: &mut [BlockLine],
    range: Range<usize>,
    kind: CodeBlockKind<'_>,
) -> Result<(), ()> {
    let lines = covered(lines, range.clone());
    for line in lines.iter_mut() {
        line.padding = 0;
        line.hidden = true;
        line.style = Modifier::REVERSED;
    }
    if let CodeBlockKind::Fenced(language) = kind
        && !language.is_empty()
    {
        let line = &mut lines[0];
        line.hidden = false;
        let opening = &source[range.start..line.original.end];
        let raw = opening.trim_start_matches(opening.chars().next().expect("fence marker"));
        let info = raw.trim_ascii();
        let start = line.original.end - raw.trim_ascii_start().len();
        line.content = start..start + info.len();
        line.padding = 0;
        line.style = Modifier::BOLD;
        line.flow = Some(Lines::label(
            source,
            &language,
            line.content.clone(),
            line.envelope.clone(),
        )?);
    }
    Ok(())
}

fn finish_code(lines: &mut [BlockLine], range: Range<usize>) {
    let lines = covered(lines, range.clone());
    if lines.iter().all(|line| line.hidden) && lines[0].decoration.width(usize::MAX) > 0 {
        lines[0].hidden = false;
        lines[0].content = range.start..range.start;
    }
    let decoration = lines[0].decoration.clone();
    if lines[0].hidden
        && decoration.has_marker()
        && let Some(first) = lines.iter_mut().find(|line| !line.hidden)
    {
        first.decoration = decoration;
    }
    let start = lines[0].original.start;
    if lines[0].hidden
        && let Some(first) = lines.iter_mut().find(|line| !line.hidden)
    {
        first.envelope.start = start;
    }
    if let Some(last) = lines.iter_mut().rev().find(|line| !line.hidden) {
        last.envelope.end = range.end;
        if let Some(flow) = &mut last.flow {
            flow.enclose(last.envelope.clone());
        }
    }
}

fn code_text(source: &str, lines: &mut [BlockLine], range: Range<usize>, text: &str) {
    for line in covered(lines, range.clone()) {
        if range.is_empty() {
            line.padding += text.len();
            line.content = range.clone();
            continue;
        }
        let start = line.content.start.max(range.start);
        let end = line.original.end.min(range.end);
        if start > end {
            continue;
        }
        if line.hidden {
            line.content = start..end;
        } else {
            line.content.end = end.max(line.content.end);
        }
        line.hidden = false;
        line.content.end -= usize::from(source[line.content.clone()].ends_with('\r'));
    }
}
