use crate::presentation::{escape_terminal, escaped_terminal_bytes, escaped_terminal_width};
mod blocks;
mod tables;
use blocks::BlockLine;
use ratatui::style::Modifier;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) const MAX_MARKDOWN_PARSE_BYTES: usize = 32 * 1024;
pub(super) const MAX_MARKDOWN_DEPTH: usize = 32;
pub(super) const MAX_MARKDOWN_EVENTS: usize = 65_536;
pub(super) const MAX_MARKDOWN_OUTPUT_BYTES: usize = 16 * MAX_MARKDOWN_PARSE_BYTES;
pub(super) const MAX_MARKDOWN_ROWS: usize = super::history::MAX_CLASSIC_HISTORY_ROWS;
pub(super) const MAX_MARKDOWN_TABLE_COLUMNS: usize = 16;
pub(super) const MAX_MARKDOWN_TABLE_ROWS: usize = 128;
pub(super) const LIMIT_CUE: &str = "Markdown formatting limit; showing source";

#[derive(Clone)]
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
fn layout(
    source: &str,
    width: usize,
    capture: bool,
    projection: Option<&[BlockLine]>,
) -> Result<Layout, ()> {
    let bounded = projection.is_some();
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
    for (index, text) in source.split('\n').enumerate() {
        let line = projection.map(|lines| &lines[index]);
        if !line.is_some_and(|line| line.hidden) {
            let content = line.map_or(text, |line| &source[line.content.clone()]);
            if let Some(table) = line.and_then(|line| line.table.as_ref()) {
                tables::layout(table, width, capture, &mut record)?;
            } else {
                layout_line((start, content), line, width, capture, &mut record)?;
            }
        }
        start += text.len() + 1;
    }
    Ok((rows, bytes, count))
}
fn layout_line(
    (start, source): (usize, &str),
    line: Option<&BlockLine>,
    width: usize,
    capture: bool,
    record: &mut impl FnMut(&mut Vec<Cell>, usize, usize) -> Result<(), ()>,
) -> Result<(), ()> {
    let range = line.map_or(start..start + source.len(), |line| line.content.clone());
    let text = source;
    let (mut columns, mut bytes, mut row_start) =
        (0, 0, line.map_or(start, |line| line.envelope.start));
    let mut cells = Vec::new();
    let mut source_columns = line.map_or(0, |line| line.padding);
    let mut decoration = prefix(line, false, width);
    for (offset, grapheme) in text.grapheme_indices(true) {
        let origin = range.start + offset..range.start + offset + grapheme.len();
        let byte = origin.start;
        let tab = grapheme == "\t" && line.is_some_and(|line| line.style == Modifier::REVERSED);
        let size = if tab {
            blocks::TAB_CELLS - source_columns % blocks::TAB_CELLS
        } else {
            escaped_terminal_width(grapheme)
        };
        source_columns += size;
        let available = width.saturating_sub(decoration.width());
        if columns > 0 && columns + size > available {
            decorate(&mut cells, &decoration, line, row_start, capture);
            record(&mut cells, row_start, bytes + decoration.len())?;
            decoration = prefix(line, true, width);
            (columns, bytes, row_start) = (0, 0, byte);
        }
        let available = width.saturating_sub(decoration.width());
        let replacement = (size > available).then_some(if available == 0 { "" } else { "…" });
        columns += replacement.map_or(size, UnicodeWidthStr::width);
        bytes += match replacement {
            Some(value) => value.len(),
            None if tab => size,
            None => escaped_terminal_bytes(grapheme),
        };
        if capture {
            cells.push(Cell {
                byte,
                source: source_range(line, origin),
                text: if let Some(value) = replacement {
                    value.to_owned()
                } else if tab {
                    " ".repeat(size)
                } else {
                    escape_terminal(grapheme)
                },
                style: line.map_or(Modifier::empty(), |line| line.style),
            });
        }
    }
    decorate(&mut cells, &decoration, line, row_start, capture);
    record(&mut cells, row_start, bytes + decoration.len())
}
fn prefix(line: Option<&BlockLine>, continuation: bool, width: usize) -> String {
    let padding = line.map_or(0, |line| if continuation { 0 } else { line.padding });
    crate::presentation::safe_truncate(&" ".repeat(padding), width.saturating_sub(1))
}
fn decorate(
    cells: &mut Vec<Cell>,
    prefix: &str,
    line: Option<&BlockLine>,
    byte: usize,
    capture: bool,
) {
    if !capture || prefix.is_empty() {
        return;
    }
    let source = line.map_or(byte..byte, |line| {
        if byte == line.envelope.start {
            line.envelope.start..if line.content.is_empty() {
                line.envelope.end
            } else {
                line.content.start
            }
        } else {
            byte..byte
        }
    });
    let mut decoration = prefix
        .graphemes(true)
        .map(|text| Cell {
            byte: source.start,
            source: source.clone(),
            text: text.to_owned(),
            style: Modifier::empty(),
        })
        .collect::<Vec<_>>();
    decoration.append(cells);
    *cells = decoration;
}
fn source_range(line: Option<&BlockLine>, mut mapped: Range<usize>) -> Range<usize> {
    if let Some(line) = line {
        if mapped.start == line.content.start {
            mapped.start = line.envelope.start;
        }
        if mapped.end == line.content.end {
            mapped.end = line.envelope.end;
        }
    }
    mapped
}
type Layout = (Vec<Row>, usize, usize);
pub(super) fn content_at(source: &str, byte: usize, width: usize) -> Option<Range<usize>> {
    let lines = blocks::project(source).ok()?;
    layout(source, width, false, Some(&lines)).ok()?;
    lines
        .iter()
        .find(|line| !line.hidden && line.content.contains(&byte))
        .map(|line| line.content.clone())
}
pub(super) fn project(source: &str, width: usize, capture: bool) -> (Layout, Option<&'static str>) {
    match blocks::project(source).and_then(|lines| layout(source, width, capture, Some(&lines))) {
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
    limit!(MAX_MARKDOWN_TABLE_COLUMNS, "columns"),
    limit!(MAX_MARKDOWN_TABLE_ROWS, "data rows"),
];
