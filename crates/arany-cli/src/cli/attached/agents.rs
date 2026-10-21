use crate::terminal::{AttachedTerminal, Composer, ShutdownSignal, TerminalInput};
use arany::SessionView;

// Closes with an optional notice, or ends on a signal; Enter may leave an agent chat open.
pub(super) async fn inspect(
    terminal: &mut AttachedTerminal,
    view: &SessionView,
) -> Result<Result<Option<&'static str>, ShutdownSignal>, String> {
    terminal
        .open_agents(view, false)
        .await
        .map_err(|error| error.to_string())?;
    loop {
        let input = terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?;
        match input {
            TerminalInput::Shutdown(signal) => {
                terminal.close_agents().map_err(|error| error.to_string())?;
                return Ok(Err(signal));
            }
            TerminalInput::Suspend => {
                terminal
                    .suspend_and_resume(0)
                    .map_err(|error| error.to_string())?;
                terminal
                    .draw(view, &Composer::default(), None)
                    .map_err(|error| error.to_string())?;
            }
            TerminalInput::Interrupt => {
                terminal.close_agents().map_err(|error| error.to_string())?;
                return Ok(Ok(None));
            }
            _ => {
                let notice = terminal
                    .agent_input(view, input)
                    .await
                    .map_err(|error| error.to_string())?;
                if !terminal.agents_open() {
                    return Ok(Ok(notice));
                }
            }
        }
    }
}
