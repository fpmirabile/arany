use super::*;
use blocks::TAB_CELLS;

#[derive(Clone, Default)]
pub(super) struct Decoration(Vec<Part>);

#[derive(Clone)]
struct Part {
    text: String,
    source: Range<usize>,
    fragment: Fragment,
}

pub(super) fn row_line(
    lines: &[BlockLine],
    (owner, fallback): (usize, usize),
    byte: usize,
) -> (usize, &BlockLine) {
    let index = if lines[owner].envelope.contains(&byte) {
        lines.partition_point(|line| line.original.end < byte)
    } else {
        fallback
    };
    (index, &lines[index])
}

impl Decoration {
    pub(super) fn width(&self, width: usize) -> usize {
        self.0
            .iter()
            .map(|part| part.text.width())
            .sum::<usize>()
            .min(width.saturating_sub(1))
    }

    pub(super) fn has_marker(&self) -> bool {
        self.0.iter().any(|part| part.fragment == Fragment::Marker)
    }

    pub(super) fn decorate(
        &self,
        cells: &mut Vec<Cell>,
        (first, capture): (bool, bool),
        (byte, width): (usize, usize),
    ) -> usize {
        let mut available = self.width(width);
        let mut bytes = 0;
        let mut output = Vec::new();
        // Decoration shares the content anchor through reflow.
        let coordinate = cells
            .first()
            .map_or_else(|| Coordinate::source(byte), |cell| cell.coordinate);
        for part in &self.0 {
            let columns = part.text.width().min(available);
            available -= columns;
            let text = if !first && part.fragment == Fragment::Marker {
                " ".repeat(columns)
            } else {
                part.text.graphemes(true).take(columns).collect::<String>()
            };
            bytes += text.len();
            if capture && !text.is_empty() {
                output.push(Cell {
                    byte,
                    coordinate,
                    envelope: None,
                    definition: None,
                    source: if first {
                        part.source.clone()
                    } else {
                        byte..byte
                    },
                    text,
                    style: Modifier::empty(),
                });
            }
        }
        if capture {
            output.append(cells);
            *cells = output;
        }
        bytes
    }
}

pub(super) fn quote(source: &str, lines: &mut [BlockLine]) {
    for line in lines {
        let mut cursor = Cursor::new(source, line);
        let start = cursor.byte;
        let saved = cursor;
        cursor.spaces(TAB_CELLS - 1);
        if cursor.take(b'>') {
            cursor.spaces(1);
        } else {
            cursor = saved;
        }
        cursor.save(line);
        line.decoration.0.push(Part {
            text: "│ ".to_owned(),
            source: start..cursor.byte,
            fragment: Fragment::Rail,
        });
    }
}

pub(super) fn item(source: &str, lines: &mut [BlockLine], number: Option<u64>) {
    let mut first = Cursor::new(source, &lines[0]);
    let start = first.byte;
    first.spaces(TAB_CELLS - 1);
    while first.peek().is_some_and(|byte| byte.is_ascii_digit()) {
        first.byte += 1;
        first.column += 1;
    }
    if first.peek().is_some() {
        first.byte += 1;
        first.column += 1;
    }
    let mut indent = first.column - lines[0].column;
    first.spaces(1);
    indent += 1;
    let saved = first;
    let extra = first.spaces(TAB_CELLS);
    if extra < TAB_CELLS {
        indent += extra;
    } else {
        first = saved;
    }
    first.save(&mut lines[0]);
    let text = number.map_or_else(|| "• ".to_owned(), |number| format!("{number}. "));
    let hanging = " ".repeat(text.width());
    for (index, line) in lines.iter_mut().enumerate() {
        if index > 0 {
            let mut cursor = Cursor::new(source, line);
            let saved = cursor;
            if cursor.spaces(indent) < indent && cursor.peek().is_some() {
                cursor = saved;
            }
            cursor.save(line);
        }
        line.decoration.0.push(Part {
            text: if index == 0 {
                text.clone()
            } else {
                hanging.clone()
            },
            source: if index == 0 {
                start..first.byte
            } else {
                line.original.start..line.content.start
            },
            fragment: if index == 0 {
                Fragment::Marker
            } else {
                Fragment::Rail
            },
        });
    }
}

#[derive(Clone, Copy)]
struct Cursor<'a> {
    source: &'a [u8],
    byte: usize,
    end: usize,
    column: usize,
    padding: usize,
}

impl<'a> Cursor<'a> {
    fn new(source: &'a str, line: &BlockLine) -> Self {
        Self {
            source: source.as_bytes(),
            byte: line.content.start,
            end: line.content.end - usize::from(source[line.content.clone()].ends_with('\r')),
            column: line.column,
            padding: line.padding,
        }
    }

    fn peek(self) -> Option<u8> {
        (self.byte < self.end).then(|| self.source[self.byte])
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.padding > 0 || self.peek() != Some(byte) {
            return false;
        }
        self.byte += 1;
        self.column += 1;
        true
    }

    fn spaces(&mut self, limit: usize) -> usize {
        let mut count = 0;
        while count < limit {
            if self.padding == 0 {
                match self.peek() {
                    Some(b' ') => self.padding = 1,
                    Some(b'\t') => self.padding = TAB_CELLS - self.column % TAB_CELLS,
                    _ => break,
                }
                self.byte += 1;
            }
            let used = self.padding.min(limit - count);
            self.padding -= used;
            self.column += used;
            count += used;
        }
        count
    }

    fn save(self, line: &mut BlockLine) {
        line.content.start = self.byte;
        line.content.end = self.end;
        line.column = self.column;
        line.padding = self.padding;
    }
}
