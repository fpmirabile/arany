use super::{
    SetupError, SetupJournal, await_setup_ack, choose_focused, read_optional_setup_value,
    read_setup_value, review_warning,
};
use crate::presentation::{escape_terminal, format_duration};
use crate::terminal::{AttachedTerminal, TerminalInput};
use arany::{
    CustomCheckStatus, CustomProfile, CustomProfileDraft, CustomProfileError, CustomProtocol,
    Effort, MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES, MAX_CUSTOM_PROFILE_EFFORTS_BYTES,
    MAX_CUSTOM_PROFILE_ENDPOINT_BYTES, MAX_CUSTOM_PROFILE_MODEL_BYTES,
    MAX_CUSTOM_PROFILE_NAME_BYTES, MessagesAuth, ProviderSetupDisposition, ProviderSetupTargetKind,
    RunCancellation, StateRoot,
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

pub(super) fn custom_profile_choices(
    profiles: &[CustomProfile],
    statuses: &[(String, CustomCheckStatus)],
) -> Vec<(u8, String)> {
    let mut choices: Vec<_> = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let status = custom_status_label(statuses, profile.name());
            (
                u8::try_from(index + 1).unwrap_or(u8::MAX),
                format!("{} · {status}", escape_terminal(profile.name())),
            )
        })
        .collect();
    choices.push((b'a', "Add custom route".into()));
    choices.push((b'b', "Back".into()));
    choices
}

pub(super) fn custom_status_label(statuses: &[(String, CustomCheckStatus)], name: &str) -> String {
    statuses
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map_or("unchecked".to_owned(), |(_, status)| match status {
            CustomCheckStatus::Unchecked => "unchecked".into(),
            CustomCheckStatus::Stale => "stale".into(),
            CustomCheckStatus::VerifiedUntil(expires) => format_verified_until(*expires),
        })
}

fn format_verified_until(expires_at_ms: i64) -> String {
    let now = i64::try_from(arany::now_ms()).unwrap_or(i64::MAX);
    let remaining_ms = u64::try_from(expires_at_ms.saturating_sub(now)).unwrap_or(0);
    format!("verified; {} left", format_duration(remaining_ms))
}

pub(super) async fn inspect_custom_route(
    terminal: &mut AttachedTerminal,
    profile: &CustomProfile,
    status: Option<CustomCheckStatus>,
) -> Result<(), SetupError> {
    let status = match status.unwrap_or(CustomCheckStatus::Unchecked) {
        CustomCheckStatus::Unchecked => "not checked".to_owned(),
        CustomCheckStatus::Stale => "stale; check again".to_owned(),
        CustomCheckStatus::VerifiedUntil(expires) => format_verified_until(expires),
    };
    let details = format!(
        "Protocol: {}\nDestination: {}\nModel: {}\nCredential: {} — not checked by listing\nCheck: {status}",
        match profile.draft().protocol {
            CustomProtocol::OpenaiResponses => "OpenAI Responses",
            CustomProtocol::AnthropicMessages => "Anthropic Messages",
            CustomProtocol::OpenaiChatCompletions => "OpenAI Chat Completions",
        },
        escape_terminal(&profile.check_destination()),
        escape_terminal(profile.model()),
        escape_terminal(profile.credential_env()),
    );
    await_setup_ack(terminal, "Custom route", &details).await?;
    Ok(())
}

pub(super) async fn check_custom_route(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    profile: &CustomProfile,
) -> Result<bool, SetupError> {
    let (calls, duration) = profile.check_limits();
    let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if !review_warning(
        terminal,
        "Check custom route?",
        &format!(
            "Destination: {}\nModel: {}\nAt most {calls} synthetic calls in {}; the Provider may bill for them. No chat or Workspace content is sent.",
            escape_terminal(&profile.check_destination()),
            escape_terminal(profile.model()),
            format_duration(duration_ms),
        ),
        "Authorize this conformance check?",
    )
    .await?
    {
        return Ok(false);
    }
    let report = await_custom_check(terminal, state, profile).await?;
    let (status, uncertain) = custom_check_status(report, profile);
    await_setup_ack(terminal, "Custom route check", &status).await?;
    Ok(uncertain)
}

async fn await_custom_check(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    profile: &CustomProfile,
) -> Result<Result<arany::CustomProfileCheck, CustomProfileError>, SetupError> {
    terminal.draw_setup(
        1,
        "Checking custom route",
        "Contacting the exact destination with synthetic data",
        0,
        None,
    )?;
    let cancellation = RunCancellation::new();
    let check = arany::check_custom_profile_value_with_cancellation(
        StateRoot::open_existing(state.path())
            .map_err(|_| "state directory unavailable or unsafe")?,
        profile.clone(),
        cancellation.clone(),
    );
    tokio::pin!(check);
    let report = loop {
        tokio::select! {
            result = &mut check => break result,
            input = terminal.next_input() => match input {
                Ok(TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput) => {
                    cancellation.cancel();
                    break check.await;
                }
                Ok(TerminalInput::Shutdown(signal)) => {
                    cancellation.cancel();
                    let _ = check.await;
                    return Err(SetupError::Shutdown(signal));
                }
                Ok(_) => {}
                Err(error) => {
                    cancellation.cancel();
                    let _ = check.await;
                    return Err(SetupError::from(error));
                }
            }
        }
    };
    Ok(report)
}

fn custom_check_status(
    report: Result<arany::CustomProfileCheck, CustomProfileError>,
    profile: &CustomProfile,
) -> (String, bool) {
    let uncertain = matches!(
        &report,
        Err(CustomProfileError::Check { receipt, .. })
            if receipt.disposition.is_none_or(|value| value == arany::CheckDisposition::Uncertain)
    ) || matches!(
        &report,
        Err(CustomProfileError::Journal(error)) if error.receipt.is_some()
    );
    let status = match report {
        Ok(report) => format!(
            "Check: {}\nDisposition: {}\nStatus: {}",
            report.view().id,
            report.view().replay_disposition(),
            format_verified_until(report.expires_at_ms()),
        ),
        Err(CustomProfileError::Check { source, receipt }) => {
            let reason = if matches!(*source, CustomProfileError::CredentialNotSet) {
                format!(
                    "Credential: {} — not set\nExport it in the launching shell, then restart Arany",
                    escape_terminal(profile.credential_env())
                )
            } else {
                format!(
                    "Check could not be completed: {}",
                    safe_custom_check_error(&source)
                )
            };
            format!(
                "{reason}\nCheck: {}\nDisposition: {}",
                receipt.id,
                receipt
                    .disposition
                    .map_or("unconfirmed".into(), |value| value.to_string())
            )
        }
        Err(error) => format!("Check unavailable: {}", safe_custom_check_error(&error)),
    };
    (status, uncertain)
}

fn safe_custom_check_error(error: &CustomProfileError) -> &'static str {
    match error {
        CustomProfileError::CredentialNotSet => "selected credential is not set",
        CustomProfileError::CredentialUnavailable => {
            "selected credential is unavailable or invalid"
        }
        CustomProfileError::CheckBusy => "another Provider check is active",
        CustomProfileError::PublicationUncertain => {
            "record publication is uncertain; inspect the saved route before continuing"
        }
        CustomProfileError::Changed => {
            "route changed during the operation; inspect it before continuing"
        }
        CustomProfileError::ConformanceFailed => "synthetic conformance failed",
        CustomProfileError::Journal(_) | CustomProfileError::EvidenceUnavailable => {
            "check result could not be confirmed"
        }
        _ => "destination or route was refused",
    }
}

async fn custom_profile_form(
    terminal: &mut AttachedTerminal,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<CustomProfileDraft>, SetupError> {
    let Some(protocol) = choose_custom_protocol(terminal, initial).await? else {
        return Ok(None);
    };
    let Some((name, endpoint, model, credential_env)) =
        read_custom_profile_fields(terminal, protocol, initial).await?
    else {
        return Ok(None);
    };
    let Some(efforts) = read_custom_efforts(terminal, initial).await? else {
        return Ok(None);
    };
    let messages_auth = if protocol == CustomProtocol::AnthropicMessages {
        let Some(choice) = choose_focused(
            terminal,
            1,
            "Messages authentication",
            "Choose the one declared credential header",
            &[(b'1', "Bearer"), (b'2', "x-api-key")],
            initial.map(|draft| match draft.messages_auth {
                MessagesAuth::Bearer => b'1',
                MessagesAuth::ApiKey => b'2',
            }),
        )
        .await?
        else {
            return Ok(None);
        };
        if choice == b'1' {
            MessagesAuth::Bearer
        } else {
            MessagesAuth::ApiKey
        }
    } else if protocol == CustomProtocol::OpenaiChatCompletions {
        MessagesAuth::Bearer
    } else {
        MessagesAuth::ApiKey
    };
    Ok(Some(CustomProfileDraft {
        name,
        protocol,
        endpoint,
        model,
        credential_env,
        messages_auth,
        efforts,
    }))
}

async fn choose_custom_protocol(
    terminal: &mut AttachedTerminal,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<CustomProtocol>, SetupError> {
    let selected = choose_focused(
        terminal,
        1,
        "Custom route protocol",
        "Choose one closed request format",
        &[
            (b'1', "OpenAI Responses"),
            (b'2', "Anthropic Messages"),
            (b'3', "OpenAI Chat Completions"),
        ],
        initial.map(|draft| match draft.protocol {
            CustomProtocol::OpenaiResponses => b'1',
            CustomProtocol::AnthropicMessages => b'2',
            CustomProtocol::OpenaiChatCompletions => b'3',
        }),
    )
    .await?;
    Ok(selected.map(|choice| {
        if choice == b'1' {
            CustomProtocol::OpenaiResponses
        } else if choice == b'2' {
            CustomProtocol::AnthropicMessages
        } else {
            CustomProtocol::OpenaiChatCompletions
        }
    }))
}

async fn read_custom_profile_fields(
    terminal: &mut AttachedTerminal,
    protocol: CustomProtocol,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<(String, String, String, String)>, SetupError> {
    let Some(name) = read_setup_value(
        terminal,
        "Custom route name",
        &format!("Route name; at most {MAX_CUSTOM_PROFILE_NAME_BYTES} bytes"),
        initial.map_or("", |draft| &draft.name),
        MAX_CUSTOM_PROFILE_NAME_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let endpoint_prompt = match protocol {
        CustomProtocol::OpenaiResponses => {
            "Exact canonical /responses URL; HTTPS, or loopback HTTP for local fixtures"
        }
        CustomProtocol::AnthropicMessages => {
            "Canonical HTTPS base URL, or loopback HTTP; Arany adds /v1/messages"
        }
        CustomProtocol::OpenaiChatCompletions => {
            "Canonical HTTPS base URL, or loopback HTTP; Arany adds /chat/completions"
        }
    };
    let Some(endpoint) = read_setup_value(
        terminal,
        "Custom route destination",
        endpoint_prompt,
        initial
            .filter(|draft| draft.protocol == protocol)
            .map_or("", |draft| &draft.endpoint),
        MAX_CUSTOM_PROFILE_ENDPOINT_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(model) = read_setup_value(
        terminal,
        "Custom route model",
        &format!("Model identifier; at most {MAX_CUSTOM_PROFILE_MODEL_BYTES} bytes"),
        initial.map_or("", |draft| &draft.model),
        MAX_CUSTOM_PROFILE_MODEL_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(credential_env) = read_setup_value(
        terminal,
        "Credential environment name",
        &format!(
            "Credential reference only; at most {MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES} bytes"
        ),
        initial.map_or("", |draft| &draft.credential_env),
        MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some((name, endpoint, model, credential_env)))
}

async fn read_custom_efforts(
    terminal: &mut AttachedTerminal,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<Vec<Effort>>, SetupError> {
    let initial = initial.map_or_else(String::new, |draft| {
        draft
            .efforts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    });
    let Some(value) = read_optional_setup_value(
        terminal,
        "Declared efforts",
        "Comma-separated effort names, or empty for no effort override",
        &initial,
        MAX_CUSTOM_PROFILE_EFFORTS_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    parse_efforts(&value)
        .map(Some)
        .ok_or_else(|| SetupError::Recoverable("invalid declared effort list".into()))
}

fn parse_efforts(value: &str) -> Option<Vec<Effort>> {
    if value.trim().is_empty() {
        return Some(Vec::new());
    }
    let requested: Vec<_> = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<Effort>)
        .collect::<Result<_, _>>()
        .ok()?;
    if requested.len() > Effort::ALL.len() {
        return None;
    }
    let efforts: Vec<_> = Effort::ALL
        .iter()
        .copied()
        .filter(|effort| requested.contains(effort))
        .collect();
    (efforts.len() == requested.len()).then_some(efforts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_efforts_create_and_clear_the_no_override_state() {
        assert_eq!(parse_efforts(""), Some(Vec::new()));
        assert_eq!(
            parse_efforts("low,high"),
            Some(vec![Effort::Low, Effort::High])
        );
        assert_eq!(parse_efforts("unknown"), None);
    }
}
