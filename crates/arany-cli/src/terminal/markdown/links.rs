use pulldown_cmark::{CodeBlockKind, Event, LinkType, Parser, Tag};
use std::ops::Range;

const COLLAPSED_SUFFIX: &str = "[]";

pub(super) fn literal_range(source: &str, mut range: Range<usize>, kind: LinkType) -> Range<usize> {
    // The parser consumes the collapsed suffix but excludes it from link and image offsets.
    if kind == LinkType::Collapsed && source[range.end..].starts_with(COLLAPSED_SUFFIX) {
        range.end += COLLAPSED_SUFFIX.len();
    }
    range
}

pub(super) struct Link {
    pub(super) range: Range<usize>,
    pub(super) start: usize,
    pub(super) breaks: usize,
    pub(super) destination: String,
    pub(super) definition: Option<Range<usize>>,
}
impl Link {
    pub(super) fn destination_range(&self, source: &str, label_end: usize) -> Option<Range<usize>> {
        let range = if let Some(definition) = &self.definition {
            let text = &source[definition.clone()];
            let start = definition.start + reference_label_end(text)? + 2;
            start..definition.end
        } else {
            label_end + 2..self.range.end
        };
        let raw = source.get(range.clone())?;
        let tail = raw.trim_ascii_start();
        let start = range.start + raw.len() - tail.len();
        if let Some(tail) = tail.strip_prefix('<') {
            return Some(start + 1..start + 1 + unescaped(tail, '>')?);
        }
        let (mut depth, mut escaped) = (0usize, false);
        for (offset, character) in tail.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '(' => depth += 1,
                ')' if depth > 0 => depth -= 1,
                ')' => return Some(start..start + offset),
                c if c.is_ascii_whitespace() => return Some(start..start + offset),
                _ => {}
            }
        }
        Some(start..start + tail.len())
    }
}
fn unescaped(source: &str, delimiter: char) -> Option<usize> {
    let mut escaped = false;
    source.char_indices().find_map(|(index, character)| {
        if escaped {
            escaped = false;
            return None;
        }
        if character == '\\' {
            escaped = true;
            return None;
        }
        (character == delimiter).then_some(index)
    })
}
fn reference_label_end(source: &str) -> Option<usize> {
    let end = unescaped(source, ']')?;
    source[end..].starts_with("]:").then_some(end)
}
struct Entity {
    range: Range<usize>,
    remaining: usize,
}
pub(super) struct Origins<'a> {
    source: &'a str,
    cursor: usize,
    end: usize,
    entity: Option<Entity>,
}
impl<'a> Origins<'a> {
    pub(super) fn new(source: &'a str, range: Range<usize>) -> Self {
        Self {
            source,
            cursor: range.start,
            end: range.end,
            entity: None,
        }
    }
    pub(super) fn next(&mut self, grapheme: &str) -> Result<Range<usize>, ()> {
        let mut mapped = self.cursor..self.cursor;
        for character in grapheme.chars() {
            let range = if let Some(entity) = &mut self.entity {
                entity.remaining -= 1;
                let range = entity.range.clone();
                if entity.remaining == 0 {
                    self.entity = None;
                }
                range
            } else if let Some((range, count)) = decoded_origin(self.source, self.cursor, self.end)
            {
                if count > 1 {
                    self.entity = Some(Entity {
                        range: range.clone(),
                        remaining: count - 1,
                    });
                }
                range
            } else {
                let tail = &self.source[self.cursor..self.end];
                let escaped = tail.strip_prefix('\\').is_some_and(|tail| {
                    character.is_ascii_punctuation() && tail.starts_with(character)
                });
                if !escaped && !tail.starts_with(character) {
                    return Err(());
                }
                self.cursor..self.cursor + character.len_utf8() + usize::from(escaped)
            };
            mapped.start = mapped.start.min(range.start);
            mapped.end = mapped.end.max(range.end);
            self.cursor = range.end;
        }
        Ok(mapped)
    }
}
fn decoded_origin(source: &str, cursor: usize, end: usize) -> Option<(Range<usize>, usize)> {
    let tail = &source[cursor..end];
    if !tail.starts_with('&') {
        return None;
    }
    let end = tail[1..]
        .bytes()
        .position(|byte| !byte.is_ascii_alphanumeric() && byte != b'#')?
        + 1;
    if tail.as_bytes()[end] != b';' {
        return None;
    }
    let length = end + 1;
    let entity = &tail[..length];
    if !entity[1..length - 1]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '#')
    {
        return None;
    }
    let decoded = Parser::new(entity).find_map(|event| match event {
        Event::Text(text) => Some(text),
        _ => None,
    })?;
    (decoded.as_ref() != entity).then(|| (cursor..cursor + length, decoded.chars().count()))
}

pub(super) fn charge(event: &Event<'_>, expanded: &mut usize) -> Result<(), ()> {
    let bytes = match event {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(label))) => {
            super::escaped_terminal_bytes(label)
        }
        Event::Start(Tag::Link {
            link_type:
                LinkType::Inline | LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut,
            dest_url,
            ..
        }) => super::escaped_terminal_bytes(dest_url) + " ()".len(),
        Event::Text(text) | Event::Code(text) => super::escaped_terminal_bytes(text),
        _ => 0,
    };
    *expanded += bytes;
    if *expanded > super::MAX_MARKDOWN_OUTPUT_BYTES {
        Err(())
    } else {
        Ok(())
    }
}

pub(super) fn definition(
    event: &Event<'_>,
    definitions: &pulldown_cmark::RefDefs<'_>,
) -> Option<Range<usize>> {
    if let Event::Start(Tag::Link { id, .. }) = event {
        definitions
            .get(id)
            .map(|definition| definition.span.clone())
    } else {
        None
    }
}
