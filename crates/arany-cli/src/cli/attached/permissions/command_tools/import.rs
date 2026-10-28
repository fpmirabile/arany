use super::*;

pub(super) async fn select_import(
    terminal: &mut AttachedTerminal,
    editor: &Editor<'_>,
) -> Result<String, SetupError> {
    let imported = match arany::import_command_tools(editor.state, &editor.admission.workspace) {
        Ok(imported) => imported,
        Err(error) => return Ok(format!("Cannot import user settings: {error}")),
    };
    let notice = format!(
        "User settings proposals; {} commands were not importable; select those executables directly. Selection stays disabled until probe and confirmation.",
        imported.refused
    );
    let rows = imported
        .candidates
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            (
                u8::try_from(index).expect("imports are bounded"),
                format!(
                    "{} {}",
                    crate::presentation::escape_terminal(&tool.selected_path),
                    crate::presentation::escape_terminal(
                        &serde_json::to_string(&tool.args).expect("typed arguments serialize")
                    )
                ),
            )
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(format!("No importable Bash hooks. {notice}"));
    }
    let Some(index) =
        super::super::pick(terminal, "Import from Claude Code settings", &notice, &rows).await?
    else {
        return Ok("Command tools unchanged".into());
    };
    let Some(tool) = imported.candidates.get(usize::from(index)) else {
        return Ok("Command tools unchanged".into());
    };
    saved(CommandTools::edit(editor.state, |record| {
        record.select(tool.clone())
    }))
}
