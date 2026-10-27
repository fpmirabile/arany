use super::inline::{Flow, chunks, envelope, place, widen_end, widen_start};
use super::*;
use pulldown_cmark::{Event, Tag, TagEnd};

const MIN_COLUMN_CELLS: usize = 8;
const SEPARATOR: &str = " │ ";

#[derive(Default)]
pub(super) struct Table {
    pub(super) range: Range<usize>,
    rows: Vec<TableRow>,
    columns: usize,
    flow: Flow,
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
        definition: Option<Range<usize>>,
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
                let mut cells = self.flow.take();
                envelope(&mut cells, range);
                *self.cell()? = cells;
            }
            _ => self.flow.event(source, event, range, definition)?,
        }
        Ok(())
    }
    fn cell(&mut self) -> Result<&mut Vec<Cell>, ()> {
        self.rows
            .last_mut()
            .and_then(|row| row.cells.last_mut())
            .ok_or(())
    }
    pub(super) fn line_at(&self, coordinate: Coordinate) -> Option<&[Cell]> {
        self.rows
            .iter()
            .flat_map(|row| &row.cells)
            .find(|cells| cells.iter().any(|cell| cell.coordinate == coordinate))
            .map(Vec::as_slice)
    }
    pub(super) fn finish(&mut self) {
        self.rows[0].range.start = self.range.start;
        for cell in self.rows[0].cells.iter_mut().flatten() {
            cell.style |= Modifier::BOLD;
        }
        for row in &mut self.rows {
            if let Some(first) = row.cells.iter_mut().find(|cell| !cell.is_empty()) {
                widen_start(first, row.range.start);
            }
            if let Some(last) = row.cells.iter_mut().rev().find(|cell| !cell.is_empty()) {
                widen_end(last, row.range.end);
            }
        }
        if let Some(last) = self
            .rows
            .iter_mut()
            .flat_map(|row| &mut row.cells)
            .rev()
            .find(|cell| !cell.is_empty())
        {
            widen_end(last, self.range.end);
        }
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
                        coordinate: Coordinate {
                            fragment: Fragment::Label,
                            offset: cell.len(),
                            ..Coordinate::source(anchor)
                        },
                        envelope: None,
                        definition: None,
                        source: anchor..anchor,
                        text: header.text.clone(),
                        style: header.style | Modifier::BOLD,
                    });
                }
                cell.push(Cell {
                    byte: anchor,
                    coordinate: Coordinate {
                        fragment: Fragment::Separator,
                        ..Coordinate::source(anchor)
                    },
                    envelope: None,
                    definition: None,
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
            let (columns, size) = place(&cell[range], widths[column], capture, &mut output);
            bytes += size;
            if column + 1 < cells.len() {
                let padding = widths[column].saturating_sub(columns);
                bytes += padding + SEPARATOR.len();
                if capture {
                    output.push(Cell {
                        byte: anchor,
                        coordinate: Coordinate {
                            fragment: Fragment::Rail,
                            offset: column,
                            ..Coordinate::source(anchor)
                        },
                        envelope: None,
                        definition: None,
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
