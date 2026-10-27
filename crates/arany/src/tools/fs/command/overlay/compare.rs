use super::*;

const COMMAND_COMPARE_BYTES_PER_UPPER_BYTE: u64 = 2;
const COMPARE_BUFFER_BYTES: usize = 16 * 1024;

pub(super) struct Comparison {
    remaining: u64,
}

impl Comparison {
    pub(super) fn new(upper_bytes: u64) -> Result<Self, ToolError> {
        Ok(Self {
            remaining: upper_bytes
                .checked_mul(COMMAND_COMPARE_BYTES_PER_UPPER_BYTE)
                .ok_or(ToolError::Limit)?,
        })
    }

    pub(super) fn identical(
        &mut self,
        capture: &Capture<'_>,
        path: &str,
        executable: bool,
    ) -> Result<bool, ToolError> {
        capture.baseline.check(capture.lower, path)?;
        let Some(before) = capture.baseline.entries.get(path) else {
            return Ok(false);
        };
        if !before.is_file() || (before.mode() & 0o111 != 0) != executable {
            return Ok(false);
        }
        let mut upper = readable(capture.upper, path)?;
        if upper.metadata().map_err(|_| ToolError::Path)?.len() != before.len() {
            return Ok(false);
        }
        let mut lower = open_file(capture.lower, path)?;
        let matches = |file: &File| {
            file.metadata()
                .map(Metadata::from_just_metadata)
                .is_ok_and(|now| fingerprint(&now) == fingerprint(before))
        };
        if !matches(&lower) {
            return Err(ToolError::Conflict);
        }
        let identical = self.content(&mut lower, &mut upper, before.len(), capture.selector)?;
        if !matches(&lower) {
            return Err(ToolError::Conflict);
        }
        capture.baseline.check(capture.lower, path)?;
        Ok(identical)
    }

    fn content(
        &mut self,
        lower: &mut File,
        upper: &mut File,
        mut bytes: u64,
        selector: &Selector,
    ) -> Result<bool, ToolError> {
        let mut left = [0; COMPARE_BUFFER_BYTES];
        let mut right = [0; COMPARE_BUFFER_BYTES];
        while bytes > 0 {
            selector.check_time()?;
            let count = bytes.min(COMPARE_BUFFER_BYTES as u64) as usize;
            self.remaining = self
                .remaining
                .checked_sub(count as u64 * COMMAND_COMPARE_BYTES_PER_UPPER_BYTE)
                .ok_or(ToolError::Limit)?;
            lower
                .read_exact(&mut left[..count])
                .map_err(|_| ToolError::Conflict)?;
            upper
                .read_exact(&mut right[..count])
                .map_err(|_| ToolError::Path)?;
            if left[..count] != right[..count] {
                return Ok(false);
            }
            bytes -= count as u64;
        }
        Ok(true)
    }
}
