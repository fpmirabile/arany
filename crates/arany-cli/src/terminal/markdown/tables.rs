use super::*;
use pulldown_cmark::{Event, Tag, TagEnd};

const MIN_COLUMN_CELLS: usize = 8;
const SEPARATOR: &str = " │ ";

#[derive(Default)]
pub(super) struct Table {
    pub(super) range: Range<usize>,
    rows: Vec<TableRow>,
    columns: usize,
    bytes: usize,
    styles: Vec<(Modifier, Range<usize>, usize)>,
    literal: Option<Range<usize>>,
}
#[derive(Default)]
struct TableRow {
    range: Range<usize>,
    cells: Vec<Vec<Cell>>,
}
impl Table {
    pub(super) fn new(range: Range<usize>, columns: usize) -> Result<Self, ()> {
        if columns > MAX_MARKDOWN_TABLE_COLUMNS {
            return Err(());
        }
        Ok(Self {
            range,
            columns,
            ..Self::default()
        })
    }
    pub(super) fn event(
        &mut self,
        source: &str,
        event: &Event<'_>,
        range: Range<usize>,
    ) -> Result<(), ()> {
        match event {
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                if self.rows.len() > MAX_MARKDOWN_TABLE_ROWS {
                    return Err(());
                }
                self.rows.push(TableRow {
                    range,
                    ..TableRow::default()
                });
            }
            Event::Start(Tag::TableCell) => {
                let row = self.rows.last_mut().ok_or(())?;
                if row.cells.len() == self.columns {
                    return Err(());
                }
                row.cells.push(Vec::new());
            }
            Event::End(TagEnd::TableCell) => {
                let cell = self.cell()?;
                envelope(cell, range);
            }
            _ => self.inline(source, event, range)?,
        }
        Ok(())
    }
    fn inline(&mut self, source: &str, event: &Event<'_>, range: Range<usize>) -> Result<(), ()> {
        if self
            .literal
            .as_ref()
            .is_some_and(|literal| range.start >= literal.start && range.end <= literal.end)
        {
            return Ok(());
        }
        match event {
            Event::Start(Tag::Emphasis | Tag::Strong) => {
                let style = if matches!(event, Event::Start(Tag::Strong)) {
                    Modifier::BOLD
                } else {
                    Modifier::ITALIC
                };
                let start = self.cell()?.len();
                self.styles.push((style, range, start));
            }
            Event::End(TagEnd::Emphasis | TagEnd::Strong) => {
                let (_, range, start) = self.styles.pop().ok_or(())?;
                envelope(&mut self.cell()?[start..], range);
            }
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => {
                self.append(
                    source,
                    &source[range.clone()],
                    range.clone(),
                    Modifier::empty(),
                )?;
                self.literal = Some(range);
            }
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                let style = self
                    .styles
                    .iter()
                    .fold(Modifier::empty(), |style, (next, _, _)| style | *next);
                self.append(
                    source,
                    text,
                    range,
                    style
                        | if matches!(event, Event::Code(_)) {
                            Modifier::REVERSED
                        } else {
                            Modifier::empty()
                        },
                )?;
            }
            _ => {}
        }
        Ok(())
    }
    fn cell(&mut self) -> Result<&mut Vec<Cell>, ()> {
        self.rows
            .last_mut()
            .and_then(|row| row.cells.last_mut())
            .ok_or(())
    }
    pub(super) fn finish(&mut self) {
        self.rows[0].range.start = self.range.start;
        for cell in self.rows[0].cells.iter_mut().flatten() {
            cell.style |= Modifier::BOLD;
        }
        for row in &mut self.rows {
            if let Some(first) = row.cells.iter_mut().flatten().next() {
                first.source.start = row.range.start;
            }
            if let Some(last) = row.cells.iter_mut().flatten().next_back() {
                last.source.end = row.range.end;
            }
        }
        if let Some(last) = self
            .rows
            .iter_mut()
            .flat_map(|row| &mut row.cells)
            .flatten()
            .next_back()
        {
            last.source.end = self.range.end;
        }
    }
    fn append(
        &mut self,
        source: &str,
        text: &str,
        range: Range<usize>,
        style: Modifier,
    ) -> Result<(), ()> {
        self.bytes += escaped_terminal_bytes(text);
        if self.bytes > MAX_MARKDOWN_OUTPUT_BYTES {
            return Err(());
        }
        let mut cursor = range.start;
        let cell = self.cell()?;
        let first = cell.len();
        for text in text.graphemes(true) {
            let byte = source[cursor..range.end]
                .find(text)
                .map(|offset| cursor + offset);
            let origin = byte.map_or(range.clone(), |byte| cursor..byte + text.len());
            cursor = origin.end;
            cell.push(Cell {
                byte: byte.unwrap_or(origin.start),
                source: origin,
                text: escape_terminal(text),
                style,
            });
        }
        envelope(&mut cell[first..], range);
        Ok(())
    }
}
fn envelope(cells: &mut [Cell], range: Range<usize>) {
    if let Some(first) = cells.first_mut() {
        first.source.start = range.start;
    }
    if let Some(last) = cells.last_mut() {
        last.source.end = range.end;
    }
}

pub(super) fn layout(
    table: &Table,
    width: usize,
    capture: bool,
    record: &mut impl FnMut(&mut Vec<Cell>, usize, usize) -> Result<(), ()>,
) -> Result<(), ()> {
    let mut natural = vec![0; table.columns];
    for row in &table.rows {
        for (column, cell) in row.cells.iter().enumerate() {
            natural[column] = natural[column].max(cell.iter().map(|cell| cell.text.width()).sum());
        }
    }
    let mut widths: Vec<_> = natural
        .iter()
        .map(|size| (*size).clamp(1, MIN_COLUMN_CELLS))
        .collect();
    let minimum =
        widths.iter().sum::<usize>() + SEPARATOR.width() * table.columns.saturating_sub(1);
    if minimum > width {
        records(table, width, capture, record)?;
    } else {
        let mut spare = width - minimum;
        while spare > 0 {
            let mut changed = false;
            for (size, natural) in widths.iter_mut().zip(&natural) {
                if spare > 0 && *size < *natural {
                    *size += 1;
                    spare -= 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for row in &table.rows {
            emit(&row.cells, &widths, row.range.start, capture, record)?;
        }
    }
    Ok(())
}
fn records(
    table: &Table,
    width: usize,
    capture: bool,
    record: &mut impl FnMut(&mut Vec<Cell>, usize, usize) -> Result<(), ()>,
) -> Result<(), ()> {
    for (index, row) in table.rows.iter().enumerate() {
        for (column, value) in row.cells.iter().enumerate() {
            let anchor = value
                .first()
                .map_or(row.range.end, |cell| cell.source.start);
            let mut cell = Vec::new();
            if index > 0 {
                for header in &table.rows[0].cells[column] {
                    cell.push(Cell {
                        byte: anchor,
                        source: anchor..anchor,
                        text: header.text.clone(),
                        style: header.style | Modifier::BOLD,
                    });
                }
                cell.push(Cell {
                    byte: anchor,
                    source: anchor..anchor,
                    text: ": ".into(),
                    style: Modifier::empty(),
                });
            }
            cell.extend(value.iter().cloned());
            emit(&[cell], &[width], anchor, capture, record)?;
        }
    }
    Ok(())
}
fn chunks(cells: &[Cell], width: usize) -> Vec<Range<usize>> {
    let (mut start, mut columns, mut space) = (0, 0, None);
    let mut rows = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        while index > start && columns + cell.text.width() > width {
            let end = space.filter(|end| *end > start).unwrap_or(index);
            rows.push(start..end);
            start = end;
            columns = cells[start..index]
                .iter()
                .map(|cell| cell.text.width().min(width))
                .sum();
            space = None;
        }
        columns += cell.text.width().min(width);
        if cell.text == " " {
            space = Some(index + 1);
        }
    }
    rows.push(start..cells.len());
    rows
}
fn emit(
    cells: &[Vec<Cell>],
    widths: &[usize],
    anchor: usize,
    capture: bool,
    record: &mut impl FnMut(&mut Vec<Cell>, usize, usize) -> Result<(), ()>,
) -> Result<(), ()> {
    let wrapped: Vec<_> = cells
        .iter()
        .zip(widths)
        .map(|(cells, width)| chunks(cells, *width))
        .collect();
    for line in 0..wrapped.iter().map(Vec::len).max().unwrap_or(1) {
        let (mut output, mut bytes) = (Vec::new(), 0);
        for (column, cell) in cells.iter().enumerate() {
            let range = wrapped[column].get(line).cloned().unwrap_or(0..0);
            let mut columns = 0;
            for cell in &cell[range] {
                let text =
                    overflow_replacement(cell.text.width(), widths[column]).unwrap_or(&cell.text);
                columns += text.width();
                bytes += text.len();
                if capture {
                    output.push(Cell {
                        text: text.to_owned(),
                        ..cell.clone()
                    });
                }
            }
            if column + 1 < cells.len() {
                let padding = widths[column].saturating_sub(columns);
                bytes += padding + SEPARATOR.len();
                if capture {
                    output.push(Cell {
                        byte: anchor,
                        source: anchor..anchor,
                        text: format!("{}{SEPARATOR}", " ".repeat(padding)),
                        style: Modifier::empty(),
                    });
                }
            }
        }
        let source_byte = cells
            .iter()
            .zip(&wrapped)
            .filter_map(|(cell, rows)| rows.get(line).and_then(|range| cell.get(range.start)))
            .map(|cell| cell.source.start)
            .min()
            .unwrap_or(anchor);
        record(&mut output, source_byte, bytes)?;
    }
    Ok(())
}
