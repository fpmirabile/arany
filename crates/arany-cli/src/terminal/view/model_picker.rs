use super::*;

pub(in crate::terminal) struct ModelCatalogFrame<'a> {
    pub picker: &'a super::super::ModelPicker<'a>,
    pub armed: bool,
    pub composer: &'a Composer,
    pub model: &'a PresentationModel,
    pub hints: ComposerHints<'a>,
}

pub(in crate::terminal) fn draw_model_catalog_frame(
    frame: &mut Frame,
    state: ModelCatalogFrame<'_>,
) {
    let ModelCatalogFrame {
        picker,
        armed,
        composer,
        model,
        hints,
    } = state;
    let palette = hints.palette;
    let full = frame.area();
    if !dialog_fits(full) {
        frame.render_widget(Paragraph::new("Resize or Esc").style(palette.error()), full);
        return;
    }
    let draft_height = composer_height(composer, full).min(full.height - 5);
    let composer_area = Rect::new(
        full.x,
        full.bottom() - 1 - draft_height,
        full.width,
        draft_height,
    );
    frame.render_widget(ratatui::widgets::Clear, composer_area);
    draw_composer(
        frame,
        model,
        composer,
        composer_area,
        ComposerHints {
            picker_focused: true,
            ..hints
        },
    );
    let available = full.height.saturating_sub(1 + draft_height);
    let height = available.min(12);
    let area = Rect::new(
        full.x,
        full.y + available.saturating_sub(height),
        full.width,
        height,
    );
    if area.width == 0 || area.height < 4 {
        return;
    }
    frame.render_widget(ratatui::widgets::Clear, area);
    let header = catalog_header(picker, area.width);
    frame.render_widget(
        Paragraph::new(safe_truncate(&header, usize::from(area.width))).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if picker.catalog_state != super::super::ModelCatalogState::Current {
        draw_catalog_status(frame, full, picker, palette);
    }
    draw_catalog_rows(frame, area, picker, armed);
    let (navigation, action) = catalog_hints(picker, area.width);
    for (offset, text) in [(2, navigation), (1, action)].into_iter().filter(|_| armed) {
        frame.render_widget(
            Paragraph::new(safe_truncate(text, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - offset, area.width, 1),
        );
    }
}

fn catalog_header(picker: &super::super::ModelPicker<'_>, width: u16) -> String {
    let label = picker.items.get(picker.selected).map_or("models", |item| {
        if picker.exact_custom {
            "exact"
        } else if item.runnable {
            "reviewed"
        } else {
            "listed"
        }
    });
    if picker.filter.is_empty() {
        format!("Models · {label} · {} available", picker.items.len())
    } else {
        let suffix = if picker.filter.len() >= 64 {
            " · 64 max"
        } else {
            ""
        };
        format!(
            "Find {}{suffix}",
            model_id_preview(
                picker.filter,
                usize::from(width).saturating_sub(5 + suffix.len())
            )
        )
    }
}

fn draw_catalog_status(
    frame: &mut Frame,
    full: Rect,
    picker: &super::super::ModelPicker<'_>,
    palette: Palette,
) {
    let status = match picker.catalog_state {
        super::super::ModelCatalogState::Refreshing if full.width >= 50 => {
            "Refreshing models · cached choices remain selectable".into()
        }
        super::super::ModelCatalogState::Refreshing => "Refreshing".into(),
        super::super::ModelCatalogState::Updated if full.width >= 50 => {
            "Model list refreshed".into()
        }
        super::super::ModelCatalogState::Updated => "Refreshed".into(),
        _ if full.width >= 50 => match picker.refresh_error {
            Some(error) => format!("Refresh failed: {error} · cached choices kept"),
            None => "Refresh failed · cached choices kept · Ctrl+R retries".into(),
        },
        _ => "Refresh failed".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(safe_truncate(&status, usize::from(full.width))).style(
            if picker.catalog_state == super::super::ModelCatalogState::RefreshFailed {
                palette.error()
            } else {
                palette.selection()
            },
        ),
        Rect::new(full.x, full.bottom() - 1, full.width, 1),
    );
}

fn draw_catalog_rows(
    frame: &mut Frame,
    area: Rect,
    picker: &super::super::ModelPicker<'_>,
    armed: bool,
) {
    let selected = picker.selected;
    let rows = usize::from(area.height - 3);
    let start = window_start(Some(selected), rows);
    if picker.items.is_empty() {
        frame.render_widget(
            Paragraph::new(if picker.filter.is_empty() {
                "No models available"
            } else {
                "No matches"
            }),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
    for (row, item) in picker.items.iter().skip(start).take(rows).enumerate() {
        let index = start + row;
        let focused = armed && index == selected;
        let prefix = numbered_label(index, "", focused).width();
        let suffix = if focused {
            format!(
                " <{}>",
                picker.efforts.get(index).copied().flatten().map_or(
                    if area.width < 24 { "def" } else { "default" },
                    arany::Effort::as_str
                )
            )
        } else {
            String::new()
        };
        let id = model_id_preview(
            &item.id,
            usize::from(area.width).saturating_sub(prefix + suffix.len()),
        );
        frame.render_widget(
            Paragraph::new(numbered_line(
                index,
                &format!("{id}{suffix}"),
                armed.then_some(selected),
                area.width,
            )),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
}

fn catalog_hints(
    picker: &super::super::ModelPicker<'_>,
    width: u16,
) -> (&'static str, &'static str) {
    let navigation = if width < FULL_LAYOUT_WIDTH {
        "Up/Dn L/R effort"
    } else {
        "Up/Down model · Left/Right reasoning"
    };
    let action = if picker.items.is_empty() {
        "Type/BS · Esc"
    } else if width < FULL_LAYOUT_WIDTH {
        "Enter pick Esc"
    } else if width < 60 || picker.exact_custom {
        "Enter selects both · Esc · Type/BS filter"
    } else if width < 84 {
        "Enter selects both · Esc · Type/BS filter · Ctrl+R refresh"
    } else {
        "Enter selects both · Esc keeps draft · Type/BS filter · PgUp/PgDn · Ctrl+R refresh"
    };
    (navigation, action)
}
