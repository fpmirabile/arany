use arany::{AttachedTerminal, Composer, SessionView, ShutdownSignal, TerminalInput};

pub(super) async fn inspect(
    terminal: &mut AttachedTerminal,
    view: &SessionView,
) -> Result<Option<ShutdownSignal>, String> {
    terminal
        .open_agents(view, false)
        .map_err(|error| error.to_string())?;
    loop {
        let input = terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?;
        match input {
            TerminalInput::Shutdown(signal) => {
                terminal.close_agents().map_err(|error| error.to_string())?;
                return Ok(Some(signal));
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
                return Ok(None);
            }
            _ => {
                terminal
                    .agent_input(view, input)
                    .map_err(|error| error.to_string())?;
                if !terminal.agents_open() {
                    return Ok(None);
                }
            }
        }
    }
}
