use crate::presentation::{escape_terminal, escaped_terminal_bytes, escaped_terminal_width};
use pulldown_cmark::{Event, Parser, Tag};
use ratatui::style::Modifier;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) const MAX_MARKDOWN_PARSE_BYTES: usize = 32 * 1024;
pub(super) const MAX_MARKDOWN_DEPTH: usize = 32;
pub(super) const MAX_MARKDOWN_EVENTS: usize = 65_536;
pub(super) const MAX_MARKDOWN_OUTPUT_BYTES: usize = 16 * MAX_MARKDOWN_PARSE_BYTES;
pub(super) const MAX_MARKDOWN_ROWS: usize = super::history::MAX_CLASSIC_HISTORY_ROWS;
pub(super) const LIMIT_CUE: &str = "Markdown formatting limit; showing source";

pub(super) struct Cell {
    pub(super) byte: usize,
    pub(super) source: Range<usize>,
    pub(super) text: String,
    pub(super) style: Modifier,
}
pub(super) struct Row {
    pub(super) cells: Vec<Cell>,
    pub(super) source_byte: usize,
}
struct Heading {
    envelope: Range<usize>,
    content: Range<usize>,
}
fn headings(source: &str) -> Result<Vec<Heading>, ()> {
    if source.len() > MAX_MARKDOWN_PARSE_BYTES {
        return Err(());
    }
    if source.split("\r\n").any(|part| part.contains('\r')) {
        return Ok(Vec::new());
    }
    let (mut headings, mut depth) = (Vec::new(), 0usize);
    for (index, (event, range)) in Parser::new(source).into_offset_iter().enumerate() {
        if index >= MAX_MARKDOWN_EVENTS || headings.len() >= MAX_MARKDOWN_ROWS {
            return Err(());
        }
        match event {
            Event::Start(tag) => {
                if depth == MAX_MARKDOWN_DEPTH {
                    return Err(());
                }
                if depth == 0
                    && matches!(tag, Tag::Heading { .. })
                    && source[range.clone()].lines().nth(1).is_none()
                {
                    let start = source[..range.start].rfind('\n').map_or(0, |at| at + 1);
                    let line = source[range.clone()].trim_end_matches(['\r', '\n']);
                    let body = line.trim_start_matches('#').trim_start_matches([' ', '\t']);
                    let content_start = range.start + line.len() - body.len();
                    let body = body.trim_end_matches([' ', '\t']);
                    let closing = body.trim_end_matches('#');
                    let body = if closing.is_empty() || closing.ends_with([' ', '\t']) {
                        closing.trim_end_matches([' ', '\t'])
                    } else {
                        body
                    };
                    headings.push(Heading {
                        content: content_start..content_start + body.len(),
                        envelope: start..range.end,
                    });
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(())?,
            _ => {}
        }
    }
    Ok(headings)
}
fn layout(
    source: &str,
    width: usize,
    capture: bool,
    headings: Option<&[Heading]>,
) -> Result<Layout, ()> {
    let bounded = headings.is_some();
    let mut headings = headings.unwrap_or_default().iter().peekable();
    let mut rows = Vec::new();
    let (mut bytes, mut count, mut start) = (0, 0, 0);
    let mut record = |cells: &mut Vec<Cell>, source_byte, row_bytes| {
        bytes += row_bytes;
        count += 1;
        if bounded
            && (bytes + count * 2 > MAX_MARKDOWN_OUTPUT_BYTES || count + 2 > MAX_MARKDOWN_ROWS)
        {
            return Err(());
        }
        if capture {
            rows.push(Row {
                cells: std::mem::take(cells),
                source_byte,
            });
        }
        Ok(())
    };
    for line in source.split('\n') {
        let end = start + line.len();
        let heading = headings.next_if(|heading| heading.envelope.start == start);
        let range = heading.map_or(start..end, |heading| heading.content.clone());
        let (mut columns, mut row_bytes, mut row_start) = (0, 0, start);
        let mut cells = Vec::new();
        for (offset, grapheme) in source[range.clone()].grapheme_indices(true) {
            let byte = range.start + offset;
            let size = escaped_terminal_width(grapheme);
            if columns > 0 && columns + size > width {
                record(&mut cells, row_start, row_bytes)?;
                (columns, row_bytes, row_start) = (0, 0, byte);
            }
            let replacement = (size > width).then_some(if width == 0 { "" } else { "…" });
            columns += replacement.map_or(size, UnicodeWidthStr::width);
            row_bytes += replacement.map_or_else(|| escaped_terminal_bytes(grapheme), str::len);
            if capture {
                cells.push(Cell {
                    byte,
                    source: source_range(heading, byte, grapheme.len()),
                    text: replacement.map_or_else(|| escape_terminal(grapheme), str::to_owned),
                    style: heading.map_or(Modifier::empty(), |_| Modifier::BOLD),
                });
            }
        }
        record(&mut cells, row_start, row_bytes)?;
        start = end + 1;
    }
    Ok((rows, bytes, count))
}
fn source_range(heading: Option<&Heading>, byte: usize, len: usize) -> Range<usize> {
    let mut mapped = byte..byte + len;
    if let Some(heading) = heading {
        if byte == heading.content.start {
            mapped.start = heading.envelope.start;
        }
        if mapped.end == heading.content.end {
            mapped.end = heading.envelope.end;
        }
    }
    mapped
}
type Layout = (Vec<Row>, usize, usize);
pub(super) fn project(source: &str, width: usize, capture: bool) -> (Layout, Option<&'static str>) {
    match headings(source).and_then(|headings| layout(source, width, capture, Some(&headings))) {
        Ok(layout) => (layout, None),
        Err(()) => (literal(source, width, capture), Some(LIMIT_CUE)),
    }
}
pub(super) fn literal(source: &str, width: usize, capture: bool) -> Layout {
    layout(source, width, capture, None).expect("literal layout has no formatting gate")
}
macro_rules! limit {
    ($name:ident, $unit:literal) => {
        arany::limits::Limit {
            name: stringify!($name),
            value: $name as u64,
            unit: $unit,
            owner: "crates/arany-cli/src/terminal/markdown.rs",
        }
    };
}
pub(super) const LIMITS: &[arany::limits::Limit] = &[
    limit!(MAX_MARKDOWN_PARSE_BYTES, "bytes"),
    limit!(MAX_MARKDOWN_DEPTH, "containers"),
    limit!(MAX_MARKDOWN_EVENTS, "events"),
    limit!(MAX_MARKDOWN_OUTPUT_BYTES, "bytes"),
    limit!(MAX_MARKDOWN_ROWS, "rows"),
];
