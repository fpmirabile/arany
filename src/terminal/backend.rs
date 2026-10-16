use super::{TerminalMode, TerminalOutput, input::CursorQuery};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Rect, Size},
};
use std::{io, ops::Range};

pub(super) struct TerminalBackend {
    inner: CrosstermBackend<TerminalOutput>,
    cursor: Option<CursorQuery>,
    mode: TerminalMode,
    area: Rect,
}

impl TerminalBackend {
    pub(super) fn new(mode: TerminalMode, cursor: Option<CursorQuery>) -> Self {
        Self {
            inner: CrosstermBackend::new(TerminalOutput(Some(io::stderr()))),
            cursor,
            mode,
            area: Rect::default(),
        }
    }

    pub(super) fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    pub(super) fn clear_owned(&mut self) -> io::Result<()> {
        if self.mode == TerminalMode::Fullscreen {
            self.inner.clear()
        } else {
            let height = self.inner.size()?.height;
            self.inner
                .set_cursor_position((0, self.area.y.min(height.saturating_sub(1))))?;
            self.inner.clear_region(ClearType::AfterCursor)
        }
    }

    pub(super) fn detach(&mut self) {
        self.inner = CrosstermBackend::new(TerminalOutput(None));
        self.cursor = None;
    }
}

impl Backend for TerminalBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, count: u16) -> io::Result<()> {
        self.inner.append_lines(count)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        #[cfg(unix)]
        {
            let (column, row) = self
                .cursor
                .as_ref()
                .ok_or_else(|| io::Error::other("cursor input owner unavailable"))?
                .query(&mut self.inner)?;
            let size = self.size()?;
            if column >= size.width || row >= size.height {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid terminal cursor response",
                ));
            }
            Ok(Position::new(column, row))
        }
        #[cfg(not(unix))]
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        if self.mode == TerminalMode::Fullscreen {
            return self.clear_owned();
        }
        let position = self.get_cursor_position()?;
        self.clear_owned()?;
        self.inner.set_cursor_position(position)
    }

    fn clear_region(&mut self, region: ClearType) -> io::Result<()> {
        if region == ClearType::All {
            self.clear()
        } else {
            self.inner.clear_region(region)
        }
    }

    fn size(&self) -> io::Result<Size> {
        let size = self.inner.size()?;
        if size.width == 0
            || size.height == 0
            || usize::from(size.width) > super::history::MAX_ROW_CELLS
            || usize::from(size.width) * usize::from(size.height) > 262_144
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal geometry outside visual bounds",
            ));
        }
        Ok(size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn scroll_region_up(&mut self, region: Range<u16>, count: u16) -> io::Result<()> {
        self.inner.scroll_region_up(region, count)
    }

    fn scroll_region_down(&mut self, region: Range<u16>, count: u16) -> io::Result<()> {
        self.inner.scroll_region_down(region, count)
    }
}
