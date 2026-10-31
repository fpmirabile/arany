use super::TerminalInput;
use super::agent_chat::MAX_CHOICE_BYTES;
use crate::presentation::{
    AgentFocus, AgentInspectorModel, AgentQuery, AgentRow, MAX_AGENT_SEARCH,
};
use arany::{AgentRunId, RunId};

const SEARCH_LIMIT: &str = "Search takes up to 64 ASCII characters";
pub(super) const INVALID_AGENT_PANEL_CHOICE: &str = "invalid agent selection";
const NOTHING_FOCUSED: &str = "No agent focused; choose a number first";

#[derive(Default)]
pub(super) struct AgentPanel {
    pub(super) query: AgentQuery,
    pub(super) scroll: usize,
    pub(super) locked: bool,
    // Linear numbers bind to the rows last printed, numbered from this offset plus one.
    pub(super) printed: Option<(usize, Vec<AgentRow>)>,
    pub(super) notice: Option<&'static str>,
    choice: String,
    invalid: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum PanelUpdate {
    Unchanged,
    Redraw,
    // The rows changed under a user action that needs a fresh input barrier.
    Rearm,
    Closed,
    Open(RunId, AgentRunId),
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
        model: &AgentInspectorModel,
        linear: bool,
    ) -> PanelUpdate {
        if matches!(input, TerminalInput::Escape | TerminalInput::EndOfInput) {
            return PanelUpdate::Closed;
        }
        if input == TerminalInput::Resize {
            return PanelUpdate::Rearm;
        }
        if linear {
            return self.handle_linear(input, model);
        }
        let cleared = self.notice.take().is_some();
        let last = model.rows.len().checked_sub(1);
        let update = match (input, last) {
            (TerminalInput::Up, Some(_)) => self.focus(
                model,
                model.selected.map_or(0, |index| index.saturating_sub(1)),
            ),
            (TerminalInput::Down, Some(last)) => self.focus(
                model,
                model.selected.map_or(0, |index| (index + 1).min(last)),
            ),
            (TerminalInput::Home, Some(_)) => self.focus(model, 0),
            (TerminalInput::End, Some(last)) => self.focus(model, last),
            (TerminalInput::PageUp, _) => {
                self.scroll = self.scroll.saturating_sub(5);
                PanelUpdate::Redraw
            }
            (TerminalInput::PageDown, _) => {
                self.scroll = self.scroll.saturating_add(5);
                PanelUpdate::Redraw
            }
            (TerminalInput::Character(character), _) => self.type_search(character),
            (TerminalInput::Backspace, _) if self.query.filter.pop().is_some() => {
                self.refocus_first()
            }
            (TerminalInput::Submit, _) => self.activate(model),
            _ => PanelUpdate::Unchanged,
        };
        if cleared && update == PanelUpdate::Unchanged {
            PanelUpdate::Redraw
        } else {
            update
        }
    }

    pub(super) fn focus(&mut self, model: &AgentInspectorModel, index: usize) -> PanelUpdate {
        let Some(row) = model.rows.get(index) else {
            return PanelUpdate::Unchanged;
        };
        if self.query.focus == AgentFocus::Row(*row) {
            return PanelUpdate::Unchanged;
        }
        self.query.focus = AgentFocus::Row(*row);
        self.scroll = 0;
        PanelUpdate::Redraw
    }

    fn refocus_first(&mut self) -> PanelUpdate {
        self.query.focus = AgentFocus::First;
        self.scroll = 0;
        PanelUpdate::Redraw
    }

    fn type_search(&mut self, character: char) -> PanelUpdate {
        if !(character.is_ascii_graphic() || character == ' ')
            || self.query.filter.len() >= MAX_AGENT_SEARCH
        {
            self.notice = Some(SEARCH_LIMIT);
            return PanelUpdate::Redraw;
        }
        self.query.filter.push(character);
        self.refocus_first()
    }

    // Only the focused row acts; a collapsed group expands in place and hidden rows never open.
    fn activate(&mut self, model: &AgentInspectorModel) -> PanelUpdate {
        match model.selected.map(|index| model.rows[index]) {
            Some(AgentRow::Agent(run, agent)) => PanelUpdate::Open(run, agent),
            Some(AgentRow::Idle) => {
                self.query.expanded = true;
                self.query.focus = model
                    .first_hidden
                    .map_or(AgentFocus::First, AgentFocus::Row);
                self.scroll = 0;
                PanelUpdate::Rearm
            }
            None => PanelUpdate::Unchanged,
        }
    }

    // Every reprint renumbers the choices, so it waits behind a fresh barrier.
    fn handle_linear(&mut self, input: TerminalInput, model: &AgentInspectorModel) -> PanelUpdate {
        match input {
            TerminalInput::Character(character) => {
                let search =
                    self.choice.starts_with('/') || (self.choice.is_empty() && character == '/');
                let limit = if search {
                    1 + MAX_AGENT_SEARCH
                } else {
                    MAX_CHOICE_BYTES
                };
                if (character.is_ascii_graphic() || (search && character == ' '))
                    && self.choice.len() < limit
                {
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
                let choice = std::mem::take(&mut self.choice);
                let update = if std::mem::take(&mut self.invalid) {
                    None
                } else {
                    self.linear_choice(&choice, model)
                };
                match update {
                    Some(PanelUpdate::Unchanged) if choice == "o" => {
                        self.notice = Some(NOTHING_FOCUSED);
                        self.reprint()
                    }
                    Some(PanelUpdate::Rearm | PanelUpdate::Redraw) => self.reprint(),
                    Some(update) => update,
                    None => {
                        self.notice = Some(INVALID_AGENT_PANEL_CHOICE);
                        self.reprint()
                    }
                }
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                self.choice.clear();
                self.invalid = false;
                self.notice = Some(INVALID_AGENT_PANEL_CHOICE);
                self.reprint()
            }
            _ => PanelUpdate::Unchanged,
        }
    }

    fn reprint(&mut self) -> PanelUpdate {
        self.printed = None;
        PanelUpdate::Rearm
    }

    fn linear_choice(&mut self, choice: &str, model: &AgentInspectorModel) -> Option<PanelUpdate> {
        if let Some(filter) = choice.strip_prefix('/') {
            filter.clone_into(&mut self.query.filter);
            return Some(self.refocus_first());
        }
        let last = model.rows.len().checked_sub(1);
        match (choice, last) {
            ("q", _) => Some(PanelUpdate::Closed),
            ("o", _) => Some(self.activate(model)),
            ("n", Some(last)) => {
                self.focus(
                    model,
                    model.selected.map_or(0, |index| (index + 1).min(last)),
                );
                Some(PanelUpdate::Redraw)
            }
            ("p", Some(_)) => {
                self.focus(
                    model,
                    model.selected.map_or(0, |index| index.saturating_sub(1)),
                );
                Some(PanelUpdate::Redraw)
            }
            (number, _) => {
                let (start, rows) = self.printed.as_ref()?;
                let row = *number
                    .parse::<usize>()
                    .ok()?
                    .checked_sub(start + 1)
                    .and_then(|index| rows.get(index))?;
                if row == AgentRow::Idle {
                    self.query.expanded = true;
                    self.query.focus = model
                        .first_hidden
                        .map_or(AgentFocus::First, AgentFocus::Row);
                } else {
                    self.query.focus = AgentFocus::Row(row);
                }
                self.scroll = 0;
                Some(PanelUpdate::Redraw)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TerminalInput::{Backspace, Character, Down, End, Escape, Home, Resize, Submit, Up};

    fn agent() -> (RunId, AgentRunId) {
        (RunId::new(), AgentRunId::new())
    }

    fn row((run, agent): (RunId, AgentRunId)) -> AgentRow {
        AgentRow::Agent(run, agent)
    }

    fn model(
        rows: &[AgentRow],
        selected: Option<usize>,
        first_hidden: Option<AgentRow>,
    ) -> AgentInspectorModel {
        AgentInspectorModel {
            selected,
            choices: rows.iter().map(|_| String::new()).collect(),
            rows: rows.to_vec(),
            first_hidden,
            search: String::new(),
            header: 0,
            lines: Vec::new(),
        }
    }

    fn line(panel: &mut AgentPanel, text: &str, model: &AgentInspectorModel) -> PanelUpdate {
        for character in text.chars() {
            assert_eq!(
                panel.handle(Character(character), model, true),
                PanelUpdate::Unchanged
            );
        }
        panel.handle(Submit, model, true)
    }

    #[test]
    fn inline_focus_search_and_activation_follow_identity_not_position() {
        let (a, b, c) = (agent(), agent(), agent());
        let mut panel = AgentPanel::default();
        let shown = model(&[row(a), row(b), row(c)], Some(0), None);
        assert_eq!(panel.handle(Down, &shown, false), PanelUpdate::Redraw);
        assert_eq!(panel.query.focus, AgentFocus::Row(row(b)));
        assert_eq!(panel.handle(End, &shown, false), PanelUpdate::Redraw);
        assert_eq!(
            panel.handle(
                Down,
                &model(&[row(a), row(b), row(c)], Some(2), None),
                false
            ),
            PanelUpdate::Unchanged
        );
        assert_eq!(panel.handle(Home, &shown, false), PanelUpdate::Redraw);
        assert_eq!(panel.handle(Down, &shown, false), PanelUpdate::Redraw);
        // A committed change moves the focused row; Enter still opens it and Up moves from its new place.
        let changed = model(&[row(c), row(agent()), row(b)], Some(2), None);
        assert_eq!(
            panel.handle(Submit, &changed, false),
            PanelUpdate::Open(b.0, b.1)
        );
        assert_eq!(panel.handle(Up, &changed, false), PanelUpdate::Redraw);
        assert_eq!(panel.query.focus, AgentFocus::Row(changed.rows[1]));
        // Typed text, digits included, is search text that refocuses the first match.
        for character in "child 2".chars() {
            assert_eq!(
                panel.handle(Character(character), &shown, false),
                PanelUpdate::Redraw
            );
        }
        assert_eq!(panel.query.filter, "child 2");
        assert_eq!(panel.query.focus, AgentFocus::First);
        assert_eq!(panel.handle(Backspace, &shown, false), PanelUpdate::Redraw);
        assert_eq!(panel.query.filter, "child ");
        // The limit and non-ASCII text leave the search unchanged with feedback the next key clears.
        panel.query.filter = "x".repeat(MAX_AGENT_SEARCH);
        assert_eq!(
            panel.handle(Character('y'), &shown, false),
            PanelUpdate::Redraw
        );
        assert_eq!(panel.notice, Some(SEARCH_LIMIT));
        assert_eq!(panel.query.filter.len(), MAX_AGENT_SEARCH);
        assert_eq!(
            panel.handle(TerminalInput::Left, &shown, false),
            PanelUpdate::Redraw
        );
        assert_eq!(panel.notice, None);
        panel.query.filter.clear();
        assert_eq!(
            panel.handle(Character('é'), &shown, false),
            PanelUpdate::Redraw
        );
        assert!(panel.query.filter.is_empty());
        // No match activates nothing.
        let empty = model(&[], None, None);
        panel.notice = None;
        for input in [Submit, Up, Down, Home, End, Backspace] {
            assert_eq!(panel.handle(input, &empty, false), PanelUpdate::Unchanged);
        }
        // Hidden rows open only after Enter expands their group, which focuses the first of them.
        let hidden = agent();
        let grouped = model(&[row(a), AgentRow::Idle], Some(1), Some(row(hidden)));
        assert_eq!(panel.handle(Submit, &grouped, false), PanelUpdate::Rearm);
        assert!(panel.query.expanded);
        assert_eq!(panel.query.focus, AgentFocus::Row(row(hidden)));
        assert_eq!(panel.handle(Resize, &grouped, false), PanelUpdate::Rearm);
        assert_eq!(panel.handle(Escape, &grouped, false), PanelUpdate::Closed);
    }

    #[test]
    fn linear_numbers_bind_to_the_printed_rows_and_every_reprint_rearms() {
        let (a, b, c) = (agent(), agent(), agent());
        let mut panel = AgentPanel {
            printed: Some((0, vec![row(a), row(b)])),
            ..AgentPanel::default()
        };
        // A row committed above after printing does not shift the printed numbers.
        let shown = model(&[row(c), row(a), row(b)], Some(0), None);
        assert_eq!(line(&mut panel, "2", &shown), PanelUpdate::Rearm);
        assert_eq!(panel.query.focus, AgentFocus::Row(row(b)));
        assert_eq!(panel.printed, None);
        panel.printed = Some((10, vec![row(a)]));
        assert_eq!(line(&mut panel, "11", &shown), PanelUpdate::Rearm);
        assert_eq!(panel.query.focus, AgentFocus::Row(row(a)));
        for invalid in ["1", "12", "0", "x", "-1", &"9".repeat(17)] {
            panel.printed = Some((10, vec![row(a)]));
            assert_eq!(
                line(&mut panel, invalid, &shown),
                PanelUpdate::Rearm,
                "{invalid}"
            );
            assert_eq!(panel.notice, Some(INVALID_AGENT_PANEL_CHOICE), "{invalid}");
            panel.notice = None;
        }
        assert_eq!(line(&mut panel, "/child 2", &shown), PanelUpdate::Rearm);
        assert_eq!(panel.query.filter, "child 2");
        assert_eq!(panel.query.focus, AgentFocus::First);
        assert_eq!(
            line(
                &mut panel,
                &format!("/{}", "z".repeat(MAX_AGENT_SEARCH)),
                &shown
            ),
            PanelUpdate::Rearm
        );
        assert_eq!(panel.query.filter.len(), MAX_AGENT_SEARCH);
        assert_eq!(
            line(
                &mut panel,
                &format!("/{}", "z".repeat(MAX_AGENT_SEARCH + 1)),
                &shown
            ),
            PanelUpdate::Rearm
        );
        assert_eq!(panel.notice, Some(INVALID_AGENT_PANEL_CHOICE));
        assert_eq!(panel.query.filter.len(), MAX_AGENT_SEARCH);
        assert_eq!(line(&mut panel, "/", &shown), PanelUpdate::Rearm);
        assert!(panel.query.filter.is_empty());
        assert_eq!(line(&mut panel, "o", &shown), PanelUpdate::Open(c.0, c.1));
        assert_eq!(
            line(&mut panel, "o", &model(&[], None, None)),
            PanelUpdate::Rearm
        );
        assert_eq!(panel.notice, Some(NOTHING_FOCUSED));
        panel.query.focus = AgentFocus::Row(row(c));
        assert_eq!(line(&mut panel, "n", &shown), PanelUpdate::Rearm);
        assert_eq!(panel.query.focus, AgentFocus::Row(row(a)));
        assert_eq!(
            line(&mut panel, "p", &model(&[row(c), row(a)], Some(1), None)),
            PanelUpdate::Rearm
        );
        assert_eq!(panel.query.focus, AgentFocus::Row(row(c)));
        let hidden = agent();
        panel.printed = Some((0, vec![row(a), AgentRow::Idle]));
        assert_eq!(
            line(
                &mut panel,
                "2",
                &model(&[row(a), AgentRow::Idle], None, Some(row(hidden)))
            ),
            PanelUpdate::Rearm
        );
        assert!(panel.query.expanded);
        assert_eq!(panel.query.focus, AgentFocus::Row(row(hidden)));
        assert_eq!(
            panel.handle(TerminalInput::LineRejected, &shown, true),
            PanelUpdate::Rearm
        );
        assert_eq!(panel.notice, Some(INVALID_AGENT_PANEL_CHOICE));
        assert_eq!(line(&mut panel, "q", &shown), PanelUpdate::Closed);
    }
}
