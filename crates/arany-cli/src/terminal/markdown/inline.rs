use super::*;
use pulldown_cmark::{Event, LinkType, Tag, TagEnd};

#[derive(Default)]
pub(super) struct Flow {
    base: Modifier,
    cells: Vec<Cell>,
    breaks: Vec<(usize, Range<usize>)>,
    styles: Vec<(Modifier, Range<usize>, usize)>,
    literal: Option<Range<usize>>,
    link: Option<super::links::Link>,
    bytes: usize,
    source_end: usize,
    #[cfg(test)]
    scanned_bytes: usize,
}
pub(super) struct Lines {
    cells: Vec<Cell>,
    breaks: Vec<(usize, Range<usize>)>,
}
impl Flow {
    pub(super) fn new(base: Modifier) -> Self {
        Self {
            base,
            ..Self::default()
        }
    }
    pub(super) fn event(
        &mut self,
        source: &str,
        event: &Event<'_>,
        range: Range<usize>,
    ) -> Result<(), ()> {
        if self
            .literal
            .as_ref()
            .is_some_and(|literal| range.start >= literal.start && range.end <= literal.end)
        {
            return Ok(());
        }
        let gap_start = self.source_end.min(range.start);
        // Opening tags span future content, so they do not consume it yet.
        if !matches!(event, Event::Start(_)) {
            self.source_end = range.end;
        }
        if matches!(
            event,
            Event::Start(Tag::Link { .. }) | Event::End(TagEnd::Link)
        ) {
            return self.link_event(source, event, range);
        }
        match event {
            Event::Start(Tag::Emphasis | Tag::Strong) => {
                let style = if matches!(event, Event::Start(Tag::Strong)) {
                    Modifier::BOLD
                } else {
                    Modifier::ITALIC
                };
                self.styles.push((style, range, self.cells.len()));
            }
            Event::End(TagEnd::Emphasis | TagEnd::Strong) => {
                let (_, range, start) = self.styles.pop().ok_or(())?;
                self.enclose(start, range);
            }
            Event::Start(Tag::Image { link_type, .. }) => {
                let range = super::links::literal_range(source, range, *link_type);
                self.source_end = range.end;
                self.literal_source(source, range.clone(), self.style())?;
                self.literal = Some(range);
            }
            Event::Text(text) => {
                let slashes = source.as_bytes()[gap_start..range.start]
                    .iter()
                    .rev()
                    .take_while(|byte| {
                        #[cfg(test)]
                        {
                            self.scanned_bytes += 1;
                        }
                        **byte == b'\\'
                    })
                    .count();
                let escaped = slashes % 2 == 1;
                let outer = range.start - usize::from(escaped)..range.end;
                let atomic =
                    **text != source[range.clone()] && source[range.clone()].starts_with('&');
                let origin = Origin {
                    content: range,
                    outer,
                    atomic,
                };
                self.append(source, text, origin, self.style())?;
            }
            Event::Code(text) => {
                let slice = &source[range.clone()];
                let open = slice.bytes().take_while(|byte| *byte == b'`').count();
                let close = slice.bytes().rev().take_while(|byte| *byte == b'`').count();
                let inner = range.start + open..range.end - close.min(slice.len() - open);
                let origin = Origin::plain(inner, range);
                self.append(source, text, origin, self.style() | Modifier::REVERSED)?;
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                self.literal_source(source, range, self.style())?;
            }
            Event::SoftBreak | Event::HardBreak => self.line_break(range),
            _ => {}
        }
        Ok(())
    }
    pub(super) fn take(&mut self) -> Vec<Cell> {
        std::mem::take(&mut self.cells)
    }
    pub(super) fn finish(&mut self, range: Range<usize>) -> Lines {
        if let Some((0, leading)) = self.breaks.first_mut() {
            leading.start = range.start;
        }
        self.enclose(0, range);
        Lines {
            cells: self.take(),
            breaks: std::mem::take(&mut self.breaks),
        }
    }
    fn style(&self) -> Modifier {
        self.styles
            .iter()
            .fold(self.base, |style, (next, _, _)| style | *next)
            | if self.link.is_some() {
                Modifier::UNDERLINED
            } else {
                Modifier::empty()
            }
    }
    fn link_event(
        &mut self,
        source: &str,
        event: &Event<'_>,
        range: Range<usize>,
    ) -> Result<(), ()> {
        match event {
            Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                ..
            }) => {
                self.link = Some(super::links::Link {
                    range,
                    start: self.cells.len(),
                    breaks: self.breaks.len(),
                    destination: dest_url.to_string(),
                });
            }
            Event::Start(Tag::Link {
                link_type: LinkType::Autolink | LinkType::Email,
                ..
            }) => {
                self.source_end = range.end;
                let inner = range.start + 1..range.end - 1;
                let style = self.style() | Modifier::UNDERLINED;
                self.append(
                    source,
                    &source[inner.clone()],
                    Origin::plain(inner, range.clone()),
                    style,
                )?;
                self.literal = Some(range);
            }
            Event::Start(Tag::Link { link_type, .. }) => {
                let range = super::links::literal_range(source, range, *link_type);
                self.source_end = range.end;
                self.literal_source(source, range.clone(), self.style())?;
                self.literal = Some(range);
            }
            Event::End(_) => self.finish_link(source)?,
            _ => unreachable!("link events only"),
        }
        Ok(())
    }
    fn finish_link(&mut self, source: &str) -> Result<(), ()> {
        let link = self.link.take().ok_or(())?;
        let label_end = self.cells[link.start..]
            .last()
            .map_or(link.range.start + 1, |cell| cell.source.end)
            .max(
                self.breaks[link.breaks..]
                    .last()
                    .map_or(0, |(_, range)| range.end),
            );
        let Some(destination) = link.destination_range(source, label_end) else {
            self.bytes -= self
                .cells
                .drain(link.start..)
                .map(|cell| cell.text.len())
                .sum::<usize>();
            self.breaks.truncate(link.breaks);
            return self.literal_source(source, link.range, self.style());
        };
        let style = self.style() | Modifier::UNDERLINED;
        let separator = label_end..destination.start;
        self.append(
            source,
            " (",
            Origin::plain(separator.clone(), separator),
            style,
        )?;
        self.append(
            source,
            &link.destination,
            Origin::plain(destination.clone(), destination.clone()),
            style,
        )?;
        let closing = destination.end..link.range.end;
        self.append(source, ")", Origin::plain(closing.clone(), closing), style)?;
        self.enclose(link.start, link.range);
        Ok(())
    }
    fn enclose(&mut self, start: usize, mut range: Range<usize>) {
        let end = self.breaks.partition_point(|(index, _)| *index <= start);
        if let Some((index, source)) = self.breaks[..end].last()
            && *index == start
        {
            // A following cell must not claim a leading empty row's break anchor.
            range.start = range.start.max(source.end);
        }
        envelope(&mut self.cells[start..], range);
    }
    fn line_break(&mut self, range: Range<usize>) {
        self.breaks.push((self.cells.len(), range));
    }
    fn literal_source(
        &mut self,
        source: &str,
        range: Range<usize>,
        style: Modifier,
    ) -> Result<(), ()> {
        let mut start = range.start;
        for (index, line) in source[range].split('\n').enumerate() {
            if index > 0 {
                self.line_break(start..start);
            }
            let shown = line.trim_end_matches('\r');
            let span = start..start + shown.len();
            self.append(source, shown, Origin::plain(span.clone(), span), style)?;
            start += line.len() + 1;
        }
        Ok(())
    }
    fn append(
        &mut self,
        source: &str,
        text: &str,
        Origin {
            content,
            outer,
            atomic,
        }: Origin,
        style: Modifier,
    ) -> Result<(), ()> {
        self.bytes += escaped_terminal_bytes(text);
        if self.bytes > MAX_MARKDOWN_OUTPUT_BYTES {
            return Err(());
        }
        let first = self.cells.len();
        let mut cursor = content.start;
        for grapheme in text.graphemes(true) {
            let origin = if atomic {
                content.clone()
            } else {
                origin(source, grapheme, cursor, content.end)
            };
            cursor = origin.end;
            self.cells.push(Cell {
                byte: origin.start,
                source: origin,
                text: escape_terminal(grapheme),
                style,
            });
        }
        envelope(&mut self.cells[first..], outer);
        Ok(())
    }
}
// An atomic entity gives every decoded grapheme the whole entity range.
struct Origin {
    content: Range<usize>,
    outer: Range<usize>,
    atomic: bool,
}
impl Origin {
    fn plain(content: Range<usize>, outer: Range<usize>) -> Self {
        Self {
            content,
            outer,
            atomic: false,
        }
    }
}
impl Lines {
    fn lines(&self) -> impl Iterator<Item = (&[Cell], usize)> {
        let mut start = 0;
        self.breaks
            .iter()
            .map(|(end, range)| (*end, Some(range.start)))
            .chain([(self.cells.len(), None)])
            .filter_map(move |(end, anchor)| {
                let line = &self.cells[start..end];
                start = end;
                (!line.is_empty() || anchor.is_some()).then(|| (line, anchor.unwrap_or(0)))
            })
    }
    pub(super) fn line_at(&self, byte: usize) -> Option<&[Cell]> {
        self.lines()
            .map(|(line, _)| line)
            .find(|line| line.iter().any(|cell| cell.byte == byte))
    }
}
// A decoded grapheme maps to the next source bytes that show it; one with no counterpart (a joined line break) takes the bytes it replaces.
fn origin(source: &str, grapheme: &str, cursor: usize, end: usize) -> Range<usize> {
    let rest = &source[cursor..end];
    let (skip, length) = if rest.starts_with(grapheme) {
        (0, grapheme.len())
    } else if grapheme == " " && rest.starts_with(['\r', '\n']) {
        (0, 1)
    } else if rest
        .strip_prefix('\\')
        .is_some_and(|rest| rest.starts_with(grapheme))
    {
        (0, 1 + grapheme.len())
    } else if let Some(offset) = rest.find(grapheme) {
        (offset, grapheme.len())
    } else {
        (0, rest.graphemes(true).next().map_or(0, str::len))
    };
    cursor + skip..cursor + skip + length
}
pub(super) fn envelope(cells: &mut [Cell], range: Range<usize>) {
    widen_start(cells, range.start);
    widen_end(cells, range.end);
}
// Cells of one atomic entity share a byte anchor and widen together.
pub(super) fn widen_start(cells: &mut [Cell], start: usize) {
    if let Some(byte) = cells.first().map(|cell| cell.byte) {
        for cell in cells.iter_mut().take_while(|cell| cell.byte == byte) {
            cell.source.start = start;
        }
    }
}
pub(super) fn widen_end(cells: &mut [Cell], end: usize) {
    if let Some(byte) = cells.last().map(|cell| cell.byte) {
        for cell in cells.iter_mut().rev().take_while(|cell| cell.byte == byte) {
            cell.source.end = end;
        }
    }
}
pub(super) fn chunks(cells: &[Cell], width: usize) -> Vec<Range<usize>> {
    let (mut start, mut columns, mut space) = (0, 0, None);
    let mut rows = Vec::new();
    if width == 0 {
        rows.push(0..cells.len());
        return rows;
    }
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
pub(super) fn place(
    cells: &[Cell],
    width: usize,
    capture: bool,
    output: &mut Vec<Cell>,
) -> (usize, usize) {
    let (mut columns, mut bytes) = (0, 0);
    for cell in cells {
        let text = overflow_replacement(cell.text.width(), width).unwrap_or(&cell.text);
        columns += text.width();
        bytes += text.len();
        if capture {
            output.push(Cell {
                text: text.to_owned(),
                ..cell.clone()
            });
        }
    }
    (columns, bytes)
}
pub(super) fn layout(
    lines: &Lines,
    width: usize,
    capture: bool,
    record: &mut impl FnMut(&mut Vec<Cell>, usize, usize) -> Result<(), ()>,
) -> Result<(), ()> {
    for (line, anchor) in lines.lines() {
        if line.is_empty() {
            record(&mut Vec::new(), anchor, 0)?;
            continue;
        }
        for range in chunks(line, width) {
            let mut output = Vec::new();
            let (_, bytes) = place(&line[range.clone()], width, capture, &mut output);
            record(&mut output, line[range.start].source.start, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
