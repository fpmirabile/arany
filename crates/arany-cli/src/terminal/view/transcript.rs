use super::*;

pub(in crate::terminal) fn draw_history_rows(
    frame: &mut Frame,
    area: Rect,
    history_rows: &[HistoryRow],
    palette: Palette,
) {
    for (row, line) in history_rows
        .iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        let style = history_row_style(line, palette);
        let spans = history_row_spans(line, style, area.width, palette);
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x, area.y + row as u16, area.width, 1),
        );
        // Inverse video marks a selection without relying on color.
        if let Some((from, to)) = line.selected {
            let from = u16::try_from(from).unwrap_or(u16::MAX).min(area.width);
            let to = u16::try_from(to).unwrap_or(u16::MAX).min(area.width);
            frame.buffer_mut().set_style(
                Rect::new(
                    area.x + from,
                    area.y + row as u16,
                    to.saturating_sub(from),
                    1,
                ),
                Style::default().add_modifier(Modifier::REVERSED),
            );
        }
    }
}

fn history_row_style(line: &HistoryRow, palette: Palette) -> Style {
    if line.matched {
        palette.selection()
    } else if line.speaker == MessageKind::Supervisor {
        Style::default().add_modifier(Modifier::DIM)
    } else if line.heading {
        match line.speaker {
            MessageKind::User | MessageKind::Assignment => {
                Style::default().add_modifier(Modifier::BOLD)
            }
            MessageKind::Assistant
            | MessageKind::AgentResult
            | MessageKind::Preview
            | MessageKind::Incomplete => palette.selection(),
            MessageKind::Tool | MessageKind::ToolDetails | MessageKind::Commentary => {
                palette.accent()
            }
            MessageKind::Supervisor => Style::default().add_modifier(Modifier::DIM),
            MessageKind::Notice | MessageKind::SteeringNote => {
                palette.accent().add_modifier(Modifier::BOLD)
            }
            MessageKind::Error => palette.error().add_modifier(Modifier::BOLD),
        }
    } else {
        Style::default()
    }
}

// User rows carry a quote marker and Assistant headings a bullet; other rows keep their spans.
fn history_row_spans<'a>(
    line: &'a HistoryRow,
    style: Style,
    width: u16,
    palette: Palette,
) -> Vec<Span<'a>> {
    let marker = match (line.speaker, line.heading) {
        (MessageKind::User | MessageKind::Assignment, true) => Some("› "),
        (MessageKind::Assistant | MessageKind::AgentResult, true) => Some("● "),
        (MessageKind::User | MessageKind::Assignment, false) if !line.text.is_empty() => Some("│ "),
        _ => None,
    };
    if let Some(marker) = marker {
        let width = usize::from(width).min(MAX_ROW_CELLS);
        let text = if line.heading {
            line.text.as_str()
        } else {
            line.text.strip_prefix("  ").unwrap_or(&line.text)
        };
        let mut spans = vec![Span::styled(safe_truncate(marker, width), palette.accent())];
        spans.extend(slash_spans(
            safe_truncate(text, width.saturating_sub(2)),
            line.slash_cells,
            style,
            palette,
        ));
        spans
    } else {
        if line.spans.is_empty() {
            vec![Span::styled(line.text.as_str(), style)]
        } else {
            line.spans
                .iter()
                .cloned()
                .map(|span| {
                    let merged = span.style.patch(style);
                    span.style(merged)
                })
                .collect()
        }
    }
}

// The transcript pager: History above one footer row of keys, the find prompt or a transient cue.
pub(in crate::terminal) fn draw_pager_frame(
    frame: &mut Frame,
    history_rows: &[HistoryRow],
    footer: &str,
    palette: Palette,
) -> Rect {
    let area = frame.area();
    let history_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    draw_history_rows(frame, history_area, history_rows, palette);
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new(accent_lead(
                safe_truncate(footer, usize::from(area.width)),
                palette,
            )),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    history_area
}

pub(in crate::terminal) fn native_line(
    row: &HistoryRow,
    width: u16,
    palette: Palette,
) -> Line<'static> {
    if !row.spans.is_empty() {
        return Line::from(row.spans.clone());
    }
    let quoted = matches!(row.speaker, MessageKind::User | MessageKind::Assignment);
    let marker = if quoted && !row.heading && row.text.is_empty() {
        ""
    } else {
        native_marker(row.speaker, row.heading)
    };
    let text = if quoted && !row.heading {
        row.text.strip_prefix("  ").unwrap_or(&row.text)
    } else {
        &row.text
    };
    let mut spans = vec![Span::styled(marker, palette.accent())];
    spans.extend(slash_spans(
        safe_truncate(text, usize::from(width).saturating_sub(marker.width())),
        row.slash_cells,
        if row.heading {
            palette.selection()
        } else {
            Style::default()
        },
        palette,
    ));
    Line::from(spans)
}
