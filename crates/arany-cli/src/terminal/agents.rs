use super::TerminalInput;

const MAX_CHOICE_BYTES: usize = 16;

#[derive(Default)]
pub(super) struct AgentPanel {
    pub(super) selected: usize,
    pub(super) scroll: usize,
    pub(super) locked: bool,
    pub(super) last_linear: Option<(u64, usize)>,
    pub(super) invalid_notice: bool,
    choice: String,
    invalid: bool,
}

pub(super) enum PanelUpdate {
    Unchanged,
    Redraw,
    Closed,
}

impl AgentPanel {
    pub(super) fn new(locked: bool) -> Self {
        Self {
            locked,
            ..Self::default()
        }
    }

    pub(super) fn handle(
        &mut self,
        input: TerminalInput,
        count: usize,
        linear: bool,
    ) -> PanelUpdate {
        if matches!(input, TerminalInput::Escape | TerminalInput::EndOfInput) {
            return PanelUpdate::Closed;
        }
        if input == TerminalInput::Resize {
            return PanelUpdate::Redraw;
        }
        if linear {
            return self.handle_linear(input, count);
        }
        let old = self.selected;
        self.selected = match input {
            TerminalInput::Up => self.selected.saturating_sub(1),
            TerminalInput::Down => self.selected.saturating_add(1),
            TerminalInput::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
                return PanelUpdate::Redraw;
            }
            TerminalInput::PageDown => {
                self.scroll = self.scroll.saturating_add(5);
                return PanelUpdate::Redraw;
            }
            TerminalInput::Home => 0,
            TerminalInput::End => count.saturating_sub(1),
            TerminalInput::Submit => return PanelUpdate::Closed,
            _ => return PanelUpdate::Unchanged,
        }
        .min(count.saturating_sub(1));
        if self.selected != old {
            self.scroll = 0;
        }
        if self.selected == old {
            PanelUpdate::Unchanged
        } else {
            PanelUpdate::Redraw
        }
    }

    pub(super) fn select(&mut self, index: usize, count: usize) -> PanelUpdate {
        let next = index.min(count.saturating_sub(1));
        if self.selected == next {
            return PanelUpdate::Unchanged;
        }
        self.selected = next;
        self.scroll = 0;
        PanelUpdate::Redraw
    }

    fn handle_linear(&mut self, input: TerminalInput, count: usize) -> PanelUpdate {
        match input {
            TerminalInput::Character(character) => {
                if character.is_ascii_graphic() && self.choice.len() < MAX_CHOICE_BYTES {
                    self.choice.push(character);
                } else {
                    self.invalid = true;
                }
                PanelUpdate::Unchanged
            }
            TerminalInput::Backspace => {
                self.choice.pop();
                PanelUpdate::Unchanged
            }
            TerminalInput::Submit => {
                let choice = self.choice.as_str();
                if !self.invalid {
                    if choice == "q" {
                        return PanelUpdate::Closed;
                    }
                    let next = match choice {
                        "n" => Some(self.selected.saturating_add(1)),
                        "p" => Some(self.selected.saturating_sub(1)),
                        _ => choice
                            .parse::<usize>()
                            .ok()
                            .filter(|number| (1..=count).contains(number))
                            .map(|number| number - 1),
                    };
                    if let Some(next) = next {
                        self.selected = next.min(count.saturating_sub(1));
                        self.choice.clear();
                        self.invalid_notice = false;
                        self.last_linear = None;
                        return PanelUpdate::Redraw;
                    }
                }
                self.choice.clear();
                self.invalid = false;
                self.invalid_notice = true;
                self.last_linear = None;
                PanelUpdate::Redraw
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                self.choice.clear();
                self.invalid = false;
                self.invalid_notice = true;
                self.last_linear = None;
                PanelUpdate::Redraw
            }
            _ => PanelUpdate::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspector_navigation_is_bounded_and_linear_choices_do_not_submit_work() {
        let mut panel = AgentPanel::default();
        assert!(matches!(
            panel.handle(TerminalInput::End, 3, false),
            PanelUpdate::Redraw
        ));
        assert_eq!(panel.selected, 2);
        assert!(matches!(
            panel.handle(TerminalInput::Down, 3, false),
            PanelUpdate::Unchanged
        ));
        assert!(matches!(
            panel.handle(TerminalInput::PageDown, 3, false),
            PanelUpdate::Redraw
        ));
        assert_eq!(panel.scroll, 5);
        assert!(matches!(
            panel.handle(TerminalInput::Home, 3, false),
            PanelUpdate::Redraw
        ));
        assert_eq!(panel.scroll, 0);
        assert!(matches!(
            panel.handle(TerminalInput::Resize, 3, false),
            PanelUpdate::Redraw
        ));
        assert!(matches!(panel.select(1, 3), PanelUpdate::Redraw));
        assert_eq!(panel.selected, 1);
        assert!(matches!(panel.select(99, 3), PanelUpdate::Redraw));
        assert_eq!(panel.selected, 2);
        panel.handle(TerminalInput::Character('2'), 3, true);
        assert!(matches!(
            panel.handle(TerminalInput::Submit, 3, true),
            PanelUpdate::Redraw
        ));
        assert_eq!(panel.selected, 1);
        panel.handle(TerminalInput::Character('!'), 3, true);
        assert!(matches!(
            panel.handle(TerminalInput::Submit, 3, true),
            PanelUpdate::Redraw
        ));
        assert!(panel.invalid_notice);
        panel.handle(TerminalInput::Character('q'), 3, true);
        assert!(matches!(
            panel.handle(TerminalInput::Submit, 3, true),
            PanelUpdate::Closed
        ));
    }
}
