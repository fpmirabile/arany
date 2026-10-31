use super::{Setting, SettingInput, step};
use crate::terminal::{AttachedTerminal, TerminalInput};
use arany::{GuardMode, GuardSettings, SessionView};

pub(super) const GUARD_MODE: Setting = Setting {
    label: "Guard",
    hint: "auto|required|off",
    input: SettingInput::Choice,
    value: |p| p.guard.label().into(),
    apply: |p, raw| {
        p.guard = GuardMode::ALL
            .into_iter()
            .find(|mode| mode.label() == raw)?;
        Some(())
    },
    cycle: |p, forward| p.guard = step(p.guard, &GuardMode::ALL, forward),
    reset: |p, default| p.guard = default.guard,
    guidance: None,
};

pub(super) const GUARD_FOLDERS: Setting = Setting {
    label: "Guard folders",
    hint: "opens the folders every guarded command may read",
    input: SettingInput::Link,
    value: |_| "opens the folders every guarded command may read".into(),
    apply: |_, _| None,
    cycle: |_, _| {},
    reset: |_, _| {},
    guidance: None,
};

const FOLDERS_TITLE: &str = "Guard folders";
const FOLDERS_NOTICE: &str = "Commands read and run these folders and everything in them. Enter removes the selected folder; Esc closes.";
const LINEAR_FOLDERS_NOTICE: &str = "Commands read and run these folders and everything in them. Type NUMBER to remove one; empty Enter closes.";

// The saved mode, which applies to every later Run; the panel keeps the user's draft until saved.
pub(super) fn saved_mode() -> GuardMode {
    GuardSettings::load().map_or_else(|_| GuardMode::default(), |settings| settings.mode())
}

pub(super) fn save_mode(mode: GuardMode) -> Result<(), String> {
    GuardSettings::update(|settings| {
        settings.set_mode(mode);
        Ok(())
    })
    .map(|_| ())
    .map_err(|_| "Error: guard.json could not be saved; Guard unchanged".into())
}

pub(super) async fn folders(
    terminal: &mut AttachedTerminal,
    view: &SessionView,
) -> Result<(), String> {
    let linear = terminal.is_linear();
    let mut selected = 0;
    let mut answer = String::new();
    let mut notice: Option<String> = None;
    terminal.begin_dialog();
    loop {
        let saved = GuardSettings::load().map_err(|_| "Error: guard.json could not be read")?;
        let rows = saved
            .reads()
            .map(|path| crate::presentation::escape_terminal(&path.display().to_string()))
            .collect::<Vec<_>>();
        let rows = if rows.is_empty() {
            vec!["No saved folders".to_owned()]
        } else {
            rows
        };
        selected = selected.min(rows.len() - 1);
        let guidance = notice.take().unwrap_or_else(|| {
            if linear {
                LINEAR_FOLDERS_NOTICE
            } else {
                FOLDERS_NOTICE
            }
            .to_owned()
        });
        terminal
            .draw_settings(FOLDERS_TITLE, &rows, selected, Some(&guidance), view)
            .map_err(|error| error.to_string())?;
        if terminal
            .arm_dialog()
            .await
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        let input = terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?;
        let remove = match folder_input(input, linear, &mut selected, &mut answer) {
            std::ops::ControlFlow::Continue(remove) => remove,
            std::ops::ControlFlow::Break(result) => return result,
        };
        let Some(path) =
            remove.and_then(|row| saved.reads().nth(row).map(std::path::Path::to_owned))
        else {
            continue;
        };
        notice = Some(remove_folder(&path));
    }
}

fn folder_input(
    input: TerminalInput,
    linear: bool,
    selected: &mut usize,
    answer: &mut String,
) -> std::ops::ControlFlow<Result<(), String>, Option<usize>> {
    std::ops::ControlFlow::Continue(match input {
        TerminalInput::Up => {
            *selected = selected.saturating_sub(1);
            None
        }
        TerminalInput::Down => {
            *selected += 1;
            None
        }
        TerminalInput::Character(character) if linear => {
            answer.push(character);
            None
        }
        TerminalInput::Backspace if linear => {
            answer.pop();
            None
        }
        TerminalInput::Submit if linear => {
            let value = std::mem::take(answer);
            if value.trim().is_empty() || value.trim() == "q" {
                return std::ops::ControlFlow::Break(Ok(()));
            }
            value
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1))
        }
        TerminalInput::Submit => Some(*selected),
        TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
            return std::ops::ControlFlow::Break(Ok(()));
        }
        TerminalInput::Shutdown(signal) => {
            return std::ops::ControlFlow::Break(Err(format!("terminated by {}", signal.name())));
        }
        _ => None,
    })
}

fn remove_folder(path: &std::path::Path) -> String {
    match GuardSettings::update(|settings| {
        settings.remove_read(path);
        Ok(())
    }) {
        Ok(_) => format!(
            "Removed {}; it applies from the next Run",
            crate::presentation::escape_terminal(&path.display().to_string())
        ),
        Err(_) => "Error: guard.json could not be saved; folders unchanged".into(),
    }
}
