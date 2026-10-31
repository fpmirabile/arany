use super::ToolError;
use super::types::OutputTotal;
use std::collections::VecDeque;
use tokio::io::{AsyncRead, AsyncReadExt};

pub(super) const DRAIN_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Window {
    #[serde(skip)]
    pub(super) captured: Vec<u8>,
    #[serde(skip)]
    capture: bool,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    cap: usize,
    total: OutputTotal,
    last: Option<u8>,
}

impl Window {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Self::default()
        }
    }

    pub(super) fn capturing(cap: usize) -> Self {
        Self {
            capture: true,
            ..Self::new(cap)
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub(super) fn from_recorded(text: &str, total: OutputTotal) -> Option<Self> {
        let (head, tail) = recorded_parts(total, text)?;
        if head.len() == text.len() {
            let mut window = Self::new(text.len());
            window.push(text.as_bytes(), DRAIN_BYTES).ok()?;
            window.total = total;
            window.last = None;
            return Some(window);
        }
        Some(Self {
            captured: Vec::new(),
            capture: false,
            head: head.as_bytes().to_vec(),
            tail: tail.bytes().collect(),
            cap: (head.len() + tail.len()).min(total.bytes.saturating_sub(1) as usize),
            total,
            last: None,
        })
    }

    pub(super) fn push(&mut self, bytes: &[u8], ceiling: u64) -> Result<(), ToolError> {
        if self.total.bytes + bytes.len() as u64 > ceiling {
            return Err(ToolError::OutputCeiling);
        }
        if self.capture {
            let keep = bytes
                .len()
                .min(super::spill::MAX_SPILL_RESULT_BYTES.saturating_sub(self.captured.len()));
            self.captured.extend_from_slice(&bytes[..keep]);
        }
        self.total.bytes += bytes.len() as u64;
        self.total.lines += bytes.iter().filter(|&&b| b == b'\n').count() as u64;
        if let Some(&last) = bytes.last() {
            self.last = Some(last);
        }
        let head = bytes
            .len()
            .min((self.cap.div_ceil(2)).saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..head]);
        let bytes = &bytes[head..];
        let tail_cap = self.cap / 2;
        let kept = bytes.len().min(tail_cap);
        let drop = (self.tail.len() + kept).saturating_sub(tail_cap);
        self.tail.drain(..drop);
        self.tail.extend(&bytes[bytes.len() - kept..]);
        Ok(())
    }

    pub(super) fn total(&self) -> OutputTotal {
        OutputTotal {
            bytes: self.total.bytes,
            lines: self.total.lines + u64::from(self.last.is_some_and(|b| b != b'\n')),
        }
    }

    pub(super) fn is_utf8(&self) -> bool {
        let bytes: Vec<u8> = self.head.iter().chain(&self.tail).copied().collect();
        std::str::from_utf8(&bytes).is_ok()
    }

    pub(super) fn preview(&self, keep: usize) -> String {
        let stored: Vec<u8> = self.head.iter().chain(&self.tail).copied().collect();
        if self.total.bytes <= keep as u64 {
            return String::from_utf8_lossy(&stored).into_owned();
        }
        let head_len = keep.div_ceil(2).min(self.head.len());
        let tail_available = if self.total.bytes <= self.cap as u64 {
            stored.len() - head_len
        } else {
            self.tail.len()
        };
        let tail_len = (keep / 2).min(tail_available);
        let mut head = &stored[..head_len];
        let mut tail = &stored[stored.len() - tail_len..];
        // Drop only the incomplete character at a window edge, before lossy decoding.
        let start = head.len().saturating_sub(4);
        if let Some(offset) = head[start..].iter().rposition(|b| b & 0xc0 != 0x80) {
            let start = start + offset;
            if std::str::from_utf8(&head[start..]).is_err_and(|e| e.error_len().is_none()) {
                head = &head[..start];
            }
        }
        while tail.first().is_some_and(|b| b & 0xc0 == 0x80) {
            tail = &tail[1..];
        }
        let total = self.total();
        let omitted = total.bytes - (head.len() + tail.len()) as u64;
        let shown_lines = head.iter().chain(tail).filter(|&&b| b == b'\n').count() as u64
            + u64::from(tail.last().is_some_and(|b| *b != b'\n'));
        format!(
            "{}\n…[{omitted} bytes, {} lines omitted; {} bytes, {} lines total]…\n{}",
            String::from_utf8_lossy(head),
            total.lines.saturating_sub(shown_lines),
            total.bytes,
            total.lines,
            String::from_utf8_lossy(tail)
        )
    }

    pub(super) fn fitted(&self, budget: usize) -> Result<String, ToolError> {
        let full = self.preview(self.cap);
        if encoded_len(&full) <= budget {
            return Ok(full);
        }
        let mut low = 0;
        let mut high = self.cap.min(self.total.bytes as usize);
        let mut best = None;
        while low <= high {
            let keep = low + (high - low) / 2;
            let text = self.preview(keep);
            if encoded_len(&text) <= budget {
                best = Some(text);
                low = keep + 1;
            } else if keep == 0 {
                break;
            } else {
                high = keep - 1;
            }
        }
        best.ok_or(ToolError::Limit)
    }
}

fn encoded_len(text: &str) -> usize {
    // Serializing a string is infallible; count exactly the bytes inside its JSON quotes.
    serde_json::to_string(text).expect("JSON string").len() - 2
}

pub(super) async fn read_window_into<R: AsyncRead + Unpin>(
    mut reader: R,
    window: &mut Window,
) -> Result<(), ToolError> {
    let mut chunk = [0; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| ToolError::Operation)?;
        if count == 0 {
            return Ok(());
        }
        window.push(&chunk[..count], DRAIN_BYTES)?;
    }
}

pub(super) fn fit_streams(
    stdout: &Window,
    stderr: &Window,
    budget: usize,
    render: impl Fn(&str, &str) -> Result<String, ToolError>,
) -> Result<String, ToolError> {
    let base = render("", "")?;
    let available = budget.checked_sub(base.len()).ok_or(ToolError::Limit)?;
    let out_size = encoded_len(&stdout.preview(stdout.cap));
    let err_size = encoded_len(&stderr.preview(stderr.cap));
    let out_share = if out_size <= available / 2 {
        out_size
    } else if err_size <= available / 2 {
        available - err_size
    } else {
        available.div_ceil(2)
    };
    let out = stdout.fitted(out_share)?;
    let err = stderr.fitted(available - encoded_len(&out))?;
    let output = render(&out, &err)?;
    if output.len() > budget {
        return Err(ToolError::Limit);
    }
    Ok(output)
}

pub(super) fn excerpted(total: OutputTotal, text: &str) -> bool {
    recorded_parts(total, text).is_some_and(|(head, _)| head.len() != text.len())
}

pub(super) fn valid_total(total: OutputTotal, text: &str) -> bool {
    recorded_parts(total, text).is_some()
}

fn recorded_parts(total: OutputTotal, text: &str) -> Option<(&str, &str)> {
    // Lossy decoding turns one to three invalid bytes into one U+FFFD.
    let minimum_bytes = |s: &str| {
        s.chars()
            .map(|c| {
                if c == '\u{fffd}' {
                    1
                } else {
                    c.len_utf8() as u64
                }
            })
            .sum::<u64>()
    };
    let lines = |s: &str| s.bytes().filter(|&b| b == b'\n').count() as u64;
    if total.bytes > DRAIN_BYTES || total.lines > total.bytes {
        return None;
    }
    if (minimum_bytes(text)..=text.len() as u64).contains(&total.bytes)
        && text.lines().count() as u64 == total.lines
    {
        return Some((text, ""));
    }
    // Only the exact Engine marker can account for bytes outside the captured streams.
    for (start, _) in text.match_indices("\n…[") {
        let rest = &text[start + "\n…[".len()..];
        let Some((counts, tail)) = rest.split_once("]…\n") else {
            continue;
        };
        let Some((omitted, counts)) = counts.split_once(" bytes, ") else {
            continue;
        };
        let Some((omitted_lines, totals)) = counts.split_once(" lines omitted; ") else {
            continue;
        };
        let (Ok(omitted), Ok(omitted_lines)) =
            (omitted.parse::<u64>(), omitted_lines.parse::<u64>())
        else {
            continue;
        };
        if totals != format!("{} bytes, {} lines total", total.bytes, total.lines) {
            continue;
        }
        let head = &text[..start];
        let Some(shown_min) = total.bytes.checked_sub(omitted) else {
            continue;
        };
        let shown_lines =
            lines(head) + lines(tail) + u64::from(!tail.is_empty() && !tail.ends_with('\n'));
        if omitted > 0
            && (minimum_bytes(head) + minimum_bytes(tail)..=(head.len() + tail.len()) as u64)
                .contains(&shown_min)
            && omitted_lines == total.lines.saturating_sub(shown_lines)
        {
            return Some((head, tail));
        }
    }
    None
}

#[cfg(any(target_os = "macos", test))]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandWindows {
    pub(super) stdout: Window,
    pub(super) stderr: Window,
}

#[cfg(any(target_os = "macos", test))]
impl CommandWindows {
    #[cfg(target_os = "macos")]
    pub(super) fn send(self) -> Result<(), ToolError> {
        use std::io::Write;
        let mut output = std::io::stdout().lock();
        output
            .write_all(b"WINDOWS\n")
            .map_err(|_| ToolError::Operation)?;
        serde_json::to_writer(&mut output, &self).map_err(|_| ToolError::Operation)?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|_| ToolError::Operation)
    }

    pub(super) fn valid(&self, budget: usize) -> bool {
        [&self.stdout, &self.stderr].into_iter().all(|w| {
            w.cap == budget
                && w.head.len() <= budget.div_ceil(2)
                && w.tail.len() <= budget / 2
                && w.total.bytes <= DRAIN_BYTES
                && w.total.lines <= w.total.bytes
                && (w.head.len() + w.tail.len()) as u64 == w.total.bytes.min(budget as u64)
        })
    }
}

pub(super) fn valid_mcp(output: &str) -> bool {
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str(output) else {
        return false;
    };
    let whole = object.contains_key("structured_content") || object.contains_key("tools");
    let Some(total) = object.get("output_total") else {
        return whole;
    };
    let Ok(total) = serde_json::from_value::<OutputTotal>(total.clone()) else {
        return false;
    };
    !whole
        && matches!(
            object.get("content").and_then(|c| c.as_array()).map(Vec::as_slice),
            Some([item]) if item["type"] == "text" && item["text"].as_str().is_some_and(|text| total.valid_for(text) && text.contains("\n…["))
        )
}

#[cfg(test)]
fn preview_forged() -> &'static str {
    "x\n…[1 bytes, 0 lines omitted; 501 bytes, 1 lines total]…\nx"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_windows_keep_edges_counts_boundaries_and_an_explicit_marker() {
        for bytes in [
            b"head\n123456789\ntail".as_slice(),
            "éé\n中中\n🙂🙂".as_bytes(),
            b"\xff\nbody\n\xfe",
            b"",
            b"one\n",
            b"one",
        ] {
            let mut window = Window::new(64);
            for chunk in bytes.chunks(3) {
                window.push(chunk, DRAIN_BYTES).unwrap();
            }
            let total = window.total();
            assert_eq!(total.bytes, bytes.len() as u64);
            assert_eq!(
                total.lines,
                bytes.split(|&b| b == b'\n').count() as u64
                    - u64::from(bytes.last().is_none_or(|&b| b == b'\n'))
            );
            assert_eq!(window.preview(64), String::from_utf8_lossy(bytes));
            let valid_utf8 = std::str::from_utf8(bytes).is_ok();
            for keep in 0..bytes.len() {
                let preview = window.preview(keep);
                assert!(preview.contains(" lines omitted; "));
                if valid_utf8 {
                    assert!(!preview.contains('\u{fffd}'));
                }
                assert!(
                    total.valid_for(&preview),
                    "keep={keep}, total={total:?}, text={preview:?}"
                );
            }
        }
        for cap in [16, 24, 32, 64] {
            let mut window = Window::new(cap);
            window.push(b"head\nmiddle\ntail", DRAIN_BYTES).unwrap();
            assert_eq!(
                window.preview(12),
                "head\nm\n…[4 bytes, 0 lines omitted; 16 bytes, 3 lines total]…\ne\ntail",
                "cap={cap}"
            );
        }
        let restored = Window::from_recorded(
            "prefixword\n…[11 bytes, 0 lines omitted; 24 bytes, 1 lines total]…\nend",
            OutputTotal {
                bytes: 24,
                lines: 1,
            },
        )
        .unwrap();
        assert_eq!(
            restored.preview(12),
            "prefix\n…[15 bytes, 0 lines omitted; 24 bytes, 1 lines total]…\nend"
        );
        let mut window = Window::new(8);
        window.push(b"head\nmiddle\ntail", DRAIN_BYTES).unwrap();
        assert_eq!(
            window.preview(8),
            "head\n…[8 bytes, 2 lines omitted; 16 bytes, 3 lines total]…\ntail"
        );
        assert!(matches!(
            window.push(b"!", 16),
            Err(ToolError::OutputCeiling)
        ));
        assert_eq!(window.total().bytes, 16);
        assert!(!OutputTotal { bytes: 1, lines: 1 }.valid_for("forged output"));
        let marker = "head\n…[8 bytes, 2 lines omitted; 16 bytes, 3 lines total]…\ntail";
        let total = OutputTotal {
            bytes: 16,
            lines: 3,
        };
        assert!(total.valid_for(marker));
        for (total, text) in [
            (
                OutputTotal {
                    bytes: 1000,
                    lines: 999,
                },
                "ok",
            ),
            (
                OutputTotal {
                    bytes: 16,
                    lines: 3,
                },
                "head\ntail",
            ),
            (
                OutputTotal {
                    bytes: 17,
                    lines: 3,
                },
                marker,
            ),
            (
                OutputTotal {
                    bytes: 16,
                    lines: 4,
                },
                marker,
            ),
            (
                total,
                "head\n…[9 bytes, 2 lines omitted; 16 bytes, 3 lines total]…\ntail",
            ),
            (
                total,
                "head\n…[8 bytes, 1 lines omitted; 16 bytes, 3 lines total]…\ntail",
            ),
            (
                total,
                "head\n…[8 bytes, 2 lines omitted; 16 bytes, 3 lines total]…\nlonger tail",
            ),
            (
                OutputTotal {
                    bytes: 1000,
                    lines: 1,
                },
                "x\n…[8 bytes, 2 lines omitted; 16 bytes, 3 lines total]…\ny",
            ),
        ] {
            assert!(!total.valid_for(text), "{total:?} {text:?}");
        }
    }

    #[test]
    fn mcp_results_require_consistent_totals() {
        let mut window = Window::new(64);
        window.push(&[b'x'; 500], DRAIN_BYTES).unwrap();
        let total = window.total();
        let preview = window.preview(64);
        let good = serde_json::json!({"server":"s","content":[{"type":"text","text":preview}],"output_total":total});
        assert!(valid_mcp(&good.to_string()));
        let mut bad = good.clone();
        bad.as_object_mut().unwrap().remove("output_total");
        assert!(!valid_mcp(&bad.to_string()));
        let mut bad = good.clone();
        bad["output_total"]["bytes"] = 501.into();
        assert!(!valid_mcp(&bad.to_string()));
        let mut bad = good.clone();
        bad["content"][0]["text"] = "x".into();
        assert!(!valid_mcp(&bad.to_string()));
        let mut bad = good;
        bad["content"][0]["text"] = preview_forged().into();
        assert!(!valid_mcp(&bad.to_string()));
        let whole = serde_json::json!({"server":"s","structured_content":null,"content":[]});
        assert!(valid_mcp(&whole.to_string()));
        let mut bad = whole;
        bad["output_total"] = serde_json::json!(total);
        assert!(!valid_mcp(&bad.to_string()));
    }

    #[test]
    fn command_windows_transport_is_bounded_and_rejects_a_wider_capture() {
        let budget = 4096;
        let mut stdout = Window::new(budget);
        let mut stderr = Window::new(budget);
        let bytes: Vec<u8> = (0..=255).cycle().take(16384).collect();
        stdout.push(&bytes, DRAIN_BYTES).unwrap();
        stderr.push(&bytes, DRAIN_BYTES).unwrap();
        let windows = CommandWindows { stdout, stderr };
        let wire = serde_json::to_vec(&windows).unwrap();
        assert!(wire.len() < 8 * budget + 1024);
        let mut decoded: CommandWindows = serde_json::from_slice(&wire).unwrap();
        assert!(decoded.valid(budget));
        assert_eq!(
            decoded.stdout.preview(budget),
            windows.stdout.preview(budget)
        );
        decoded.stderr.cap += 1;
        assert!(!decoded.valid(budget));
    }

    #[test]
    fn rendered_output_fits_escaping_and_donates_a_short_streams_share() {
        for stderr_large in [false, true] {
            let mut large = Window::new(4096);
            let mut short = Window::new(4096);
            let data = b"head\n\x00\"\\\xff".repeat(2048);
            for bytes in data.chunks(137) {
                large.push(bytes, DRAIN_BYTES).unwrap();
            }
            large.push(b"tail", DRAIN_BYTES).unwrap();
            short.push(b"ok\n", DRAIN_BYTES).unwrap();
            let (out, err) = if stderr_large {
                (&short, &large)
            } else {
                (&large, &short)
            };
            let text = super::super::process::fit_command(
                crate::tools::process::CommandRun {
                    exit_code: Some(7),
                    stdout: out,
                    stderr: err,
                },
                Default::default(),
                4096,
                super::super::process::Truncation::Allow,
            )
            .unwrap();
            assert!(text.len() <= 4096);
            let result: super::super::types::CommandResult = serde_json::from_str(&text).unwrap();
            assert!(result.valid());
            assert_eq!(result.exit_code, Some(7));
            let (big, small) = if stderr_large {
                (result.stderr, result.stdout)
            } else {
                (result.stdout, result.stderr)
            };
            assert!(encoded_len(&big) > 3000 && big.starts_with("head\n") && big.ends_with("tail"));
            assert_eq!(small, "ok\n");
        }
    }
}
