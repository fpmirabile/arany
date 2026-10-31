use super::custom::{
    CustomRouteContext, add_custom_route, custom_profile_choices, edit_custom_route,
    inspect_custom_route, remove_custom_route,
};
use super::{SetupError, SetupJournal, SetupSelection, choose};
use crate::presentation::escape_terminal;
use crate::terminal::AttachedTerminal;
use arany::{CollaborationPolicy, CustomProfile, Effort, ProviderKind, SessionDefaults, StateRoot};
use std::path::Path;

pub(super) async fn custom_routes_setup(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    journal: &mut SetupJournal,
) -> Result<Option<SetupSelection>, SetupError> {
    loop {
        let state = StateRoot::open_existing(state_dir)
            .or_else(|_| StateRoot::admit(state_dir))
            .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
        let (expected, profiles) =
            CustomProfile::load_all_with_snapshot(&state).map_err(SetupError::recoverable)?;
        let mut context = CustomRouteContext {
            state_dir,
            state: &state,
            expected: expected.as_deref(),
            profiles: &profiles,
            journal,
        };
        match custom_route_selection(terminal, &profiles).await? {
            None => return Ok(None),
            Some(choice) => {
                match apply_custom_route_choice(terminal, &mut context, choice).await? {
                    CustomRouteResult::Continue => {}
                    CustomRouteResult::Leave => return Ok(None),
                    CustomRouteResult::Select(selection) => return Ok(Some(selection)),
                }
            }
        }
    }
}

enum CustomRouteResult {
    Continue,
    Leave,
    Select(SetupSelection),
}

async fn custom_route_selection(
    terminal: &mut AttachedTerminal,
    profiles: &[CustomProfile],
) -> Result<Option<u8>, SetupError> {
    let choices = custom_profile_choices(profiles);
    let rendered = choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    choose(
        terminal,
        1,
        "Custom routes",
        "Declarations are saved privately; each Run admits the selected route",
        &rendered,
    )
    .await
}

async fn apply_custom_route_choice(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
    choice: u8,
) -> Result<CustomRouteResult, SetupError> {
    if choice == b'a' {
        add_custom_route(terminal, context).await?;
        return Ok(CustomRouteResult::Continue);
    }
    if choice == b'b' {
        return Ok(CustomRouteResult::Leave);
    }
    let index = usize::from(choice.saturating_sub(1));
    let profile = context
        .profiles
        .get(index)
        .ok_or_else(|| SetupError::Recoverable("invalid custom route selection".into()))?;
    let Some(action) = custom_route_action(terminal, profile).await? else {
        return Ok(CustomRouteResult::Continue);
    };
    match action {
        b'1' => inspect_custom_route(terminal, profile).await?,
        b'2' => return Ok(CustomRouteResult::Select(select_custom_profile(profile))),
        b'3' => edit_custom_route(terminal, context, index, profile).await?,
        b'4' => remove_custom_route(terminal, context, index, profile).await?,
        b'5' => {}
        _ => return Err("invalid custom route action".into()),
    }
    Ok(CustomRouteResult::Continue)
}

async fn custom_route_action(
    terminal: &mut AttachedTerminal,
    profile: &CustomProfile,
) -> Result<Option<u8>, SetupError> {
    choose(
        terminal,
        1,
        &format!("Custom route {}", escape_terminal(profile.name())),
        &format!(
            "Credential: {} — not checked by listing",
            escape_terminal(profile.credential_env())
        ),
        &[
            (b'1', "Inspect"),
            (b'2', "Select"),
            (b'3', "Edit"),
            (b'4', "Remove"),
            (b'5', "Back"),
        ],
    )
    .await
}

fn select_custom_profile(profile: &CustomProfile) -> SetupSelection {
    let effort = profile
        .efforts()
        .iter()
        .copied()
        .find(|effort| *effort == Effort::Low);
    SetupSelection::Custom {
        defaults: SessionDefaults {
            provider: Some(ProviderKind::custom_profile(profile.name())),
            model: Some(profile.model().to_owned()),
            effort,
            account_id: None,
            policy: CollaborationPolicy::default(),
        },
        notice: "Custom route selected; the next Run admits its exact destination".into(),
    }
}
