use super::{SetupError, SetupJournal, await_setup_ack, custom_profile_form, review_warning};
use crate::presentation::escape_terminal;
use crate::terminal::AttachedTerminal;
use arany::{
    CustomProfile, CustomProfileDraft, CustomProfileError, ProviderSetupDisposition,
    ProviderSetupTargetKind, StateRoot,
};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) struct CustomRouteContext<'a> {
    pub(super) state_dir: &'a Path,
    pub(super) state: &'a StateRoot,
    pub(super) expected: Option<&'a [u8]>,
    pub(super) profiles: &'a [CustomProfile],
    pub(super) journal: &'a mut SetupJournal,
}

pub(super) async fn edit_custom_route(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
    index: usize,
    profile: &CustomProfile,
) -> Result<(), SetupError> {
    let Some(draft) = custom_profile_form(terminal, Some(&profile.draft())).await? else {
        return Ok(());
    };
    if draft.name != profile.name() {
        await_setup_ack(terminal, "Edit custom route", "Route names cannot change in place; add the new route, then remove the old one after updating references").await?;
        return Ok(());
    }
    let mut drafts = context
        .profiles
        .iter()
        .map(CustomProfile::draft)
        .collect::<Vec<_>>();
    drafts[index] = draft.clone();
    CustomProfile::validate_drafts(&drafts)
        .map_err(|_| SetupError::Recoverable("custom route fields are invalid".into()))?;
    let summary = format!(
        "Name: {}\nDestination: {}\nModel: {}\nChanging this declaration clears its check evidence.",
        escape_terminal(&draft.name),
        escape_terminal(&draft.endpoint),
        escape_terminal(&draft.model),
    );
    if review_warning(
        terminal,
        "Save custom route?",
        &summary,
        "Save this declaration?",
    )
    .await?
    {
        write_custom_profiles(context, &drafts, &draft.name).await?;
        await_setup_ack(terminal, "Custom route", "Saved; check it again before use").await?;
    }
    Ok(())
}

pub(super) async fn remove_custom_route(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
    index: usize,
    profile: &CustomProfile,
) -> Result<(), SetupError> {
    if custom_route_references(context.state_dir, profile.name())? {
        await_setup_ack(terminal, "Remove custom route", "This route is referenced by local routing or roster configuration; update those records first").await?;
        return Ok(());
    }
    let summary = format!(
        "Remove {} from this State directory. Its environment variable is unchanged; its check evidence will be cleared.",
        escape_terminal(profile.name()),
    );
    if review_warning(
        terminal,
        "Remove custom route?",
        &summary,
        "Remove this route?",
    )
    .await?
    {
        let mut drafts = context
            .profiles
            .iter()
            .map(CustomProfile::draft)
            .collect::<Vec<_>>();
        drafts.remove(index);
        write_custom_profiles(context, &drafts, profile.name()).await?;
        await_setup_ack(terminal, "Custom route", "Removed").await?;
    }
    Ok(())
}

pub(super) async fn add_custom_route(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
) -> Result<(), SetupError> {
    let Some(draft) = custom_profile_form(terminal, None).await? else {
        return Ok(());
    };
    if context
        .profiles
        .iter()
        .any(|profile| profile.name() == draft.name)
    {
        await_setup_ack(
            terminal,
            "Custom route",
            "A route with that name already exists",
        )
        .await?;
        return Ok(());
    }
    let mut drafts = context
        .profiles
        .iter()
        .map(CustomProfile::draft)
        .collect::<Vec<_>>();
    drafts.push(draft.clone());
    CustomProfile::validate_drafts(&drafts)
        .map_err(|_| SetupError::Recoverable("custom route fields are invalid".into()))?;
    let summary = format!(
        "Name: {}\nDestination: {}\nModel: {}\nCredential reference: {}\nSaving does not call the Provider or verify credentials.",
        escape_terminal(&draft.name),
        escape_terminal(&draft.endpoint),
        escape_terminal(&draft.model),
        escape_terminal(&draft.credential_env),
    );
    if review_warning(
        terminal,
        "Save custom route?",
        &summary,
        "Save this declaration?",
    )
    .await?
    {
        write_custom_profiles(context, &drafts, &draft.name).await?;
        await_setup_ack(
            terminal,
            "Custom route",
            "Saved; run Check before using this route",
        )
        .await?;
    }
    Ok(())
}

async fn write_custom_profiles(
    context: &mut CustomRouteContext<'_>,
    drafts: &[CustomProfileDraft],
    identity: &str,
) -> Result<(), SetupError> {
    let operation = context
        .journal
        .start(
            ProviderSetupTargetKind::CustomProfile,
            identity.to_owned(),
            context.expected.map(|bytes| Sha256::digest(bytes).into()),
        )
        .await?;
    match CustomProfile::write_profiles(context.state, context.expected, drafts).await {
        Ok(bytes) => {
            context
                .journal
                .finish(
                    operation,
                    Some(Sha256::digest(bytes).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await
        }
        Err(error) => {
            let disposition = if matches!(error, CustomProfileError::PublicationUncertain) {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            context.journal.finish(operation, None, disposition).await?;
            Err(SetupError::custom_write(error))
        }
    }
}

fn custom_route_references(state_dir: &Path, name: &str) -> Result<bool, SetupError> {
    Ok(
        crate::cli::provider::routing::references_custom_profile(state_dir, name)
            .map_err(SetupError::recoverable)?
            || crate::cli::provider::roster::references_custom_profile(state_dir, name)
                .map_err(SetupError::recoverable)?,
    )
}
