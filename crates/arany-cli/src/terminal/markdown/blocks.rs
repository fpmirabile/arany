use super::inline::{Flow, Lines};
use super::*;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
pub(super) const TAB_CELLS: usize = 4;

pub(super) struct BlockLine {
    pub(super) original: Range<usize>,
    pub(super) envelope: Range<usize>,
    pub(super) content: Range<usize>,
    pub(super) padding: usize,
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
    let (mut depth, mut code, mut table) = (0, None, None::<super::tables::Table>);
    let mut flow = None::<Flow>;
    for (index, (event, range)) in Parser::new_ext(source, Options::ENABLE_TABLES)
        .into_offset_iter()
        .enumerate()
    {
        if index >= MAX_MARKDOWN_EVENTS {
            return Err(());
        }
        if matches!(event, Event::Start(_)) && depth == MAX_MARKDOWN_DEPTH {
            return Err(());
        }
        if let Some(table) = &mut table {
            table.event(source, &event, range.clone())?;
        }
        if let Some(flow) = &mut flow {
            flow.event(source, &event, range.clone())?;
        }
        match event {
            Event::Start(tag) => {
                match tag {
                    Tag::Table(columns) if depth == 0 => {
                        table = Some(super::tables::Table::new(range, columns.len())?);
                    }
                    Tag::CodeBlock(kind) if depth == 0 => {
                        code_block(source, &mut lines, range.clone(), kind);
                        code = Some(range);
                    }
                    tag @ (Tag::Paragraph | Tag::Heading { .. }) if depth == 0 => {
                        flow = open_flow(source, &mut lines, &tag, range);
                    }
                    _ => {}
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth = depth.checked_sub(1).ok_or(())?;
                if tag == TagEnd::CodeBlock
                    && let Some(range) = code.take()
                {
                    finish_code(&mut lines, range);
                }
                if matches!(tag, TagEnd::Paragraph | TagEnd::Heading(_))
                    && let Some(flow) = flow.take()
                {
                    finish_flow(source, &mut lines, flow, range);
                }
                if tag == TagEnd::Table
                    && let Some(table) = table.take()
                {
                    finish_table(&mut lines, table);
                }
            }
            Event::Text(text) if code.is_some() => {
                code_text(source, &mut lines, range, &text);
            }
            _ => {}
        }
    }
    Ok(lines)
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

fn code_block(source: &str, lines: &mut [BlockLine], range: Range<usize>, kind: CodeBlockKind<'_>) {
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
    }
}

fn finish_code(lines: &mut [BlockLine], range: Range<usize>) {
    let lines = covered(lines, range.clone());
    let start = lines[0].original.start;
    if lines[0].hidden
        && let Some(first) = lines.iter_mut().find(|line| !line.hidden)
    {
        first.envelope.start = start;
    }
    if let Some(last) = lines.iter_mut().rev().find(|line| !line.hidden) {
        last.envelope.end = range.end;
    }
}

fn code_text(source: &str, lines: &mut [BlockLine], range: Range<usize>, text: &str) {
    for line in covered(lines, range.clone()) {
        if range.is_empty() {
            line.padding += text.len();
            line.content = range.clone();
            continue;
        }
        let start = line.original.start.max(range.start);
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
