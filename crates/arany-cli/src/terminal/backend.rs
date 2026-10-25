use super::{TerminalMode, TerminalOutput, input::TerminalQueries};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Rect, Size},
};
use std::{
    io::{self, Write},
    ops::Range,
};

pub(super) struct TerminalBackend {
    inner: CrosstermBackend<TerminalOutput>,
    queries: Option<TerminalQueries>,
    mode: TerminalMode,
    area: Rect,
    position: Position,
    #[cfg(test)]
    test_size: Option<Size>,
    #[cfg(test)]
    fail_graphics_writes: bool,
    #[cfg(test)]
    lifecycle_failures: Vec<&'static str>,
    #[cfg(test)]
    lifecycle_steps: Option<std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>>,
    #[cfg(test)]
    drop_name: Option<&'static str>,
}

impl TerminalBackend {
    pub(super) fn new(mode: TerminalMode, queries: Option<TerminalQueries>) -> Self {
        Self {
            inner: CrosstermBackend::new(TerminalOutput(Some(io::stderr()))),
            queries,
            mode,
            area: Rect::default(),
            position: Position::default(),
            #[cfg(test)]
            test_size: None,
            #[cfg(test)]
            fail_graphics_writes: false,
            #[cfg(test)]
            lifecycle_failures: Vec::new(),
            #[cfg(test)]
            lifecycle_steps: None,
            #[cfg(test)]
            drop_name: None,
        }
    }

    #[cfg(test)]
    pub(super) fn failing_graphics_writer(width: u16, height: u16) -> Self {
        let mut backend = Self::new(TerminalMode::Fullscreen, None);
        backend.inner = CrosstermBackend::new(TerminalOutput(None));
        backend.test_size = Some(Size::new(width, height));
        backend.fail_graphics_writes = true;
        backend
    }

    #[cfg(test)]
    pub(super) fn failing_lifecycle_writer(
        width: u16,
        height: u16,
        failures: Vec<&'static str>,
        steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    ) -> Self {
        let mut backend = Self::new(TerminalMode::Fullscreen, None);
        backend.inner = CrosstermBackend::new(TerminalOutput(None));
        backend.test_size = Some(Size::new(width, height));
        backend.fail_graphics_writes = failures.contains(&"graphics");
        backend.lifecycle_failures = failures;
        backend.lifecycle_steps = Some(steps);
        backend
    }

    #[cfg(test)]
    pub(super) fn track_drop_for_test(
        &mut self,
        name: &'static str,
        steps: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    ) {
        self.inner = CrosstermBackend::new(TerminalOutput(None));
        self.test_size = Some(Size::new(80, 24));
        self.drop_name = Some(name);
        self.lifecycle_steps = Some(steps);
    }

    #[cfg(test)]
    fn lifecycle_step(&mut self, step: &'static str) -> io::Result<()> {
        self.record_lifecycle_step(step);
        if self.lifecycle_failures.contains(&step) {
            return Err(io::Error::other(format!("injected {step} failure")));
        }
        Ok(())
    }

    #[cfg(test)]
    fn record_lifecycle_step(&mut self, step: &'static str) {
        if let Some(steps) = &self.lifecycle_steps {
            steps.borrow_mut().push(step);
        }
    }

    pub(super) fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    pub(super) fn cursor_offset(&self) -> u16 {
        self.position.y.saturating_sub(self.area.y)
    }

    pub(super) fn write_graphics(&mut self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        {
            let result = self.lifecycle_step("graphics");
            if self.fail_graphics_writes {
                return Err(io::Error::other("injected graphics writer failure"));
            }
            result?;
        }
        self.inner.write_all(bytes)?;
        Write::flush(&mut self.inner)
    }

    pub(super) fn clear_owned(&mut self) -> io::Result<()> {
        #[cfg(test)]
        self.lifecycle_step("clear")?;
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
        self.queries = None;
    }
}

#[cfg(test)]
impl Drop for TerminalBackend {
    fn drop(&mut self) {
        if let (Some(name), Some(steps)) = (self.drop_name, &self.lifecycle_steps) {
            steps.borrow_mut().push(name);
        }
    }
}

impl Backend for TerminalBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content.inspect(|(_, y, _)| {
            self.position.y = *y;
        }))
    }

    fn append_lines(&mut self, count: u16) -> io::Result<()> {
        self.inner.append_lines(count)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        #[cfg(test)]
        self.lifecycle_step("cursor")?;
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        #[cfg(unix)]
        {
            let (column, row) = self
                .queries
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
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.position = position;
        Ok(())
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
        #[cfg(test)]
        let size = self.test_size.map_or_else(|| self.inner.size(), Ok)?;
        #[cfg(not(test))]
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
        #[cfg(test)]
        self.lifecycle_step("flush")?;
        Backend::flush(&mut self.inner)
    }

    fn scroll_region_up(&mut self, region: Range<u16>, count: u16) -> io::Result<()> {
        self.inner.scroll_region_up(region, count)
    }

    fn scroll_region_down(&mut self, region: Range<u16>, count: u16) -> io::Result<()> {
        self.inner.scroll_region_down(region, count)
    }
}
