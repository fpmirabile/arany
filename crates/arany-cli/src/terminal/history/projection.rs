use super::*;
type Endpoints = ((usize, TextPoint), (usize, TextPoint));
impl History {
    pub(super) fn selection_points(&self) -> Option<Endpoints> {
        let selection = self.selection.filter(|selection| selection.shown)?;
        let locate = |point: TextPoint| {
            self.messages
                .iter()
                .position(|message| message.source == point.source)
                .map(|index| (index, point))
        };
        let mut low = locate(selection.anchor)?;
        let mut high = locate(selection.head)?;
        if (high.0, high.1.coordinate, !high.1.boundary)
            < (low.0, low.1.coordinate, !low.1.boundary)
        {
            std::mem::swap(&mut low, &mut high);
        }
        Some((low, high))
    }
}
pub(super) fn contains(index: usize, coordinate: Coordinate, (low, high): Endpoints) -> bool {
    (index, coordinate) >= (low.0, low.1.coordinate)
        && ((index, coordinate) < (high.0, high.1.coordinate)
            || ((index, coordinate) == (high.0, high.1.coordinate) && !high.1.boundary))
}
pub(super) fn row_coordinate(row: &MessageRow) -> Coordinate {
    row.cells
        .as_ref()
        .and_then(|cells| cells.first())
        .map_or_else(
            || Coordinate::source(row.source_byte),
            |cell| cell.coordinate,
        )
}
pub(super) fn row_of_coordinate(rows: &[MessageRow], coordinate: Coordinate) -> usize {
    rows.iter()
        .position(|row| {
            row.cells
                .as_ref()
                .is_some_and(|cells| cells.iter().any(|cell| cell.coordinate == coordinate))
        })
        .or_else(|| {
            rows.iter()
                .enumerate()
                .filter(|(_, row)| row_coordinate(row) <= coordinate)
                .max_by_key(|(_, row)| row_coordinate(row))
                .map(|(index, _)| index)
        })
        .unwrap_or(0)
}
pub(super) fn source_matches(
    text: &str,
    rows: &[MessageRow],
    query: &str,
) -> Vec<(usize, Coordinate)> {
    let positions: Vec<_> = text.match_indices(query).map(|(byte, _)| byte).collect();
    let mut covered = vec![false; positions.len()];
    let mut sites = std::collections::BTreeSet::new();
    let mut matches = Vec::new();
    for (row, rendered) in rows.iter().enumerate() {
        for cell in rendered.cells.iter().flatten() {
            let first = positions.partition_point(|byte| *byte < cell.source.start);
            let end = positions.partition_point(|byte| *byte < cell.source.end);
            for index in first..end {
                covered[index] = true;
                if sites.insert((cell.coordinate.site, positions[index])) {
                    matches.push((row, cell.coordinate));
                }
            }
        }
    }
    let mut resolved = covered.clone();
    let mut uses = std::collections::BTreeSet::new();
    for (row, rendered) in rows.iter().enumerate() {
        for cell in rendered.cells.iter().flatten() {
            if let Some(definition) = &cell.definition
                && uses.insert(cell.coordinate.site)
            {
                let first = positions.partition_point(|byte| *byte < definition.start);
                let end = positions.partition_point(|byte| *byte < definition.end);
                if covered[first..end].iter().any(|covered| !covered) {
                    matches.push((row, cell.coordinate));
                    resolved[first..end].fill(true);
                }
            }
        }
    }
    for (index, byte) in positions.into_iter().enumerate() {
        if !resolved[index] {
            let row = row_of_byte(rows, byte);
            let coordinate = if rows[row].cells.is_some() {
                row_coordinate(&rows[row])
            } else {
                Coordinate::source(byte)
            };
            matches.push((row, coordinate));
        }
    }
    matches.sort();
    matches.dedup();
    matches
}
pub(super) fn selected_cells(
    (kind, text, chrome): (MessageKind, &str, &[Range<usize>]),
    rows: &[MessageRow],
    (offset, index): (usize, usize),
    endpoints: Endpoints,
) -> Option<(usize, usize)> {
    let (low, high) = endpoints;
    if index < low.0 || index > high.0 || (offset == 0 && has_heading(kind)) {
        return None;
    }
    if let Some(cells) = &rows[offset].cells {
        let mut at = row_indent(kind);
        let mut selected = None;
        for cell in cells {
            let next = at + cell.text.width();
            if contains(index, cell.coordinate, endpoints) {
                selected = Some((selected.map_or(at, |(from, _)| from), next));
            }
            at = next;
        }
        return selected;
    }
    let (start, end) = row_span(text, rows, offset)?;
    let mut cells = row_cells(text, start, end, row_indent(kind)).filter(|(byte, _, _)| {
        contains(index, Coordinate::source(*byte), endpoints)
            && !chrome.iter().any(|cut| cut.contains(byte))
    });
    let first = cells.next()?;
    let last = cells.last().unwrap_or(first);
    Some((first.1, last.1 + last.2))
}

pub(super) fn word_span(graphemes: &[(usize, &str)], at: usize) -> (usize, usize) {
    if let Some(range) = url_span(graphemes, at) {
        return range;
    }
    let word = |grapheme: &str| {
        grapheme
            .chars()
            .all(|character| character.is_alphanumeric() || "_-./~".contains(character))
    };
    if !word(graphemes[at].1) {
        return (at, at);
    }
    let first = graphemes[..at]
        .iter()
        .rposition(|(_, grapheme)| !word(grapheme))
        .map_or(0, |index| index + 1);
    let last = graphemes[at..]
        .iter()
        .position(|(_, grapheme)| !word(grapheme))
        .map_or(graphemes.len(), |index| at + index)
        - 1;
    (first, last)
}
