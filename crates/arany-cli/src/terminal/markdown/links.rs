use pulldown_cmark::LinkType;
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
}

impl Link {
    pub(super) fn destination_range(&self, source: &str, label_end: usize) -> Option<Range<usize>> {
        let tail = source[label_end..self.range.end]
            .strip_prefix("](")?
            .trim_ascii_start();
        let (tail, angled) = tail
            .strip_prefix('<')
            .map_or((tail, false), |tail| (tail, true));
        let rest = tail.strip_prefix(&self.destination)?;
        if (angled && !rest.starts_with('>'))
            || (!angled && !rest.starts_with(|c: char| c == ')' || c.is_ascii_whitespace()))
        {
            return None;
        }
        let start = self.range.end - tail.len();
        Some(start..start + self.destination.len())
    }
}
