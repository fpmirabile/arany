use super::*;
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
pub(super) const TAB_CELLS: usize = 4;

pub(super) struct BlockLine {
    pub(super) original: Range<usize>,
    pub(super) envelope: Range<usize>,
    pub(super) content: Range<usize>,
    pub(super) padding: usize,
    pub(super) style: Modifier,
    pub(super) hidden: bool,
}

pub(super) fn project(source: &str) -> Result<Vec<BlockLine>, ()> {
    if source.len() > MAX_MARKDOWN_PARSE_BYTES {
        return Err(());
    }
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
        });
        start = end + 1;
    }
    if source.split("\r\n").any(|part| part.contains('\r')) {
        return Ok(lines);
    }
    let (mut depth, mut code) = (0, None);
    for (index, (event, range)) in Parser::new(source).into_offset_iter().enumerate() {
        if index >= MAX_MARKDOWN_EVENTS {
            return Err(());
        }
        match event {
            Event::Start(tag) => {
                if depth == MAX_MARKDOWN_DEPTH {
                    return Err(());
                }
                match tag {
                    Tag::CodeBlock(kind) if depth == 0 => {
                        code_block(source, &mut lines, range.clone(), kind);
                        code = Some(range);
                    }
                    Tag::Heading { .. } if depth == 0 => heading(source, &mut lines, range),
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
            }
            Event::Text(text) if code.is_some() => {
                code_text(source, &mut lines, range, &text);
            }
            _ => {}
        }
    }
    Ok(lines)
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

fn heading(source: &str, lines: &mut [BlockLine], range: Range<usize>) {
    if source[range.clone()].lines().nth(1).is_some() {
        return;
    }
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
