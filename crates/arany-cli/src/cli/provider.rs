pub(crate) mod roster;
pub(crate) mod routing;
pub(crate) mod select;

use super::{CommandOutput, state_dir};
use crate::accounts::{chatgpt, credentials};
use crate::presentation::{escape_terminal, format_tokens};
use arany::{
    CustomProfile, CustomProfileError, Effort, ModelEntry, NativeApiCredentials, NativeCheckError,
    NativeCheckRequest, ProviderKind, RunCancellation, StateRoot,
    check_custom_profile_with_cancellation, check_native_model,
    list_native_models_with_credentials, validate_native_model_id,
};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct ProviderArgs {
    #[command(subcommand)]
    action: ProviderAction,
}

#[derive(Subcommand)]
enum ProviderAction {
    Logout {
        profile: String,
    },
    Models {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(
            long,
            help = "List models visible to the selected OS-stored API account"
        )]
        saved_account: bool,
        profile: String,
    },
    Check {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        profile: String,
        model: Option<String>,
        #[arg(long)]
        effort: Option<Effort>,
        #[arg(
            long,
            help = "Authorize a synthetic check; ChatGPT has no remote output-token cap"
        )]
        accept_cost: bool,
        #[arg(
            long,
            help = "Use the selected OS-stored API account instead of an environment key"
        )]
        saved_account: bool,
    },
}

struct CheckArgs {
    state_dir: Option<PathBuf>,
    profile: String,
    model: Option<String>,
    effort: Option<Effort>,
    accept_cost: bool,
    saved_account: bool,
}

pub(crate) async fn run(args: ProviderArgs) -> Result<CommandOutput, String> {
    match args.action {
        ProviderAction::Logout { profile } => logout(profile).await,
        ProviderAction::Models {
            state_dir,
            saved_account,
            profile,
        } => models(state_dir, saved_account, profile).await,
        ProviderAction::Check {
            state_dir,
            profile,
            model,
            effort,
            accept_cost,
            saved_account,
        } => {
            check(CheckArgs {
                state_dir,
                profile,
                model,
                effort,
                accept_cost,
                saved_account,
            })
            .await
        }
    }
}

async fn logout(profile: String) -> Result<CommandOutput, String> {
    if ProviderKind::parse(&profile) != Some(ProviderKind::ChatGpt) {
        return Err("logout is available only for the selected ChatGPT account".into());
    }
    let workspace = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| "Workspace unavailable".to_owned())?;
    let outcome = chatgpt::sign_out_selected(workspace)
        .await
        .map_err(|error| error.to_string())?;
    let mut stderr = String::new();
    if !outcome.remote_confirmed {
        stderr.push_str(
            "warning: remote revocation was not confirmed; disconnect Arany in ChatGPT Settings\n",
        );
    }
    if !outcome.local_cleared {
        stderr.push_str("warning: the ChatGPT token may remain in the OS credential store; unlock it and retry arany provider logout chatgpt\n");
    }
    Ok(CommandOutput {
        stdout: if outcome.local_cleared {
            "ChatGPT account disconnected locally\n".into()
        } else {
            "ChatGPT account blocked locally\n".into()
        },
        stderr,
        success: outcome.local_cleared && outcome.remote_confirmed,
    })
}

async fn models(
    selected_state_dir: Option<PathBuf>,
    saved_account: bool,
    profile: String,
) -> Result<CommandOutput, String> {
    if ProviderKind::parse(&profile) == Some(ProviderKind::ChatGpt) {
        if saved_account || selected_state_dir.is_some() {
            return Err(
                        "ChatGPT models use the selected user account; omit --saved-account and --state-dir"
                            .into(),
                    );
        }
        let workspace = std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .map_err(|_| "Workspace unavailable".to_owned())?;
        let (account_id, models) = chatgpt::selected_models(workspace, None)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(CommandOutput {
            stdout: render_chatgpt_models(account_id, &models),
            stderr: String::new(),
            success: true,
        });
    }
    let (models, exact_custom) = match ProviderKind::parse(&profile) {
        Some(ProviderKind::OpenAi | ProviderKind::Anthropic) => {
            let key = if saved_account {
                let account = credentials::load()
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or("no saved native API account")?;
                if account.provider != profile {
                    return Err("saved API account belongs to a different Provider".into());
                }
                account
                    .into_credentials()
                    .map_err(|error| error.to_string())?
            } else {
                NativeApiCredentials::from_env(&profile).map_err(|error| error.to_string())?
            };
            let models = list_native_models_with_credentials(&profile, &key)
                .await
                .map_err(|error| error.to_string())?;
            (models, false)
        }
        Some(ProviderKind::ChatGpt | ProviderKind::Custom) | None => {
            if saved_account {
                return Err("--saved-account requires a native Provider".into());
            }
            let Some(name) = ProviderKind::custom_name(&profile) else {
                return Err("Provider must be openai, anthropic, chatgpt, or custom:NAME".into());
            };
            let state_dir = state_dir(selected_state_dir).map_err(str::to_owned)?;
            let workspace = std::env::current_dir()
                .and_then(std::fs::canonicalize)
                .map_err(|_| "Workspace unavailable".to_owned())?;
            if state_dir.starts_with(&workspace) {
                return Err("state directory overlaps the Workspace".into());
            }
            let root = StateRoot::open_existing(&state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let custom =
                CustomProfile::load_named(&root, name).map_err(|error| error.to_string())?;
            let mut entry = ModelEntry::exact_custom(custom.model().to_owned());
            entry.efforts = custom.efforts().to_vec();
            (vec![entry], true)
        }
    };
    Ok(CommandOutput {
        stdout: render_models(&profile, &models, exact_custom),
        stderr: String::new(),
        success: true,
    })
}

async fn check(args: CheckArgs) -> Result<CommandOutput, String> {
    let CheckArgs {
        state_dir: selected_state_dir,
        profile,
        model,
        effort,
        accept_cost,
        saved_account,
    } = args;
    if ProviderKind::parse(&profile) == Some(ProviderKind::ChatGpt) {
        return check_chatgpt(
            selected_state_dir,
            model,
            effort,
            accept_cost,
            saved_account,
        )
        .await;
    }
    let native = ProviderKind::parse(&profile).is_some_and(ProviderKind::is_native_api);
    validate_check_options(native, model.as_deref(), effort, accept_cost, saved_account)?;
    let state_dir = state_dir(selected_state_dir).map_err(str::to_owned)?;
    let workspace = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| "Workspace unavailable".to_owned())?;
    if state_dir.starts_with(&workspace) {
        return Err("state directory overlaps the Workspace".into());
    }
    if native {
        check_native(state_dir, &profile, model, effort, saved_account).await
    } else {
        check_custom(state_dir, &profile).await
    }
}

fn validate_check_options(
    native: bool,
    model: Option<&str>,
    effort: Option<Effort>,
    accept_cost: bool,
    saved_account: bool,
) -> Result<(), String> {
    if native && !accept_cost {
        return Err("native checks may bill up to 3 inference calls and 3,072 generated tokens, plus bounded catalog/input usage; rerun with --accept-cost".into());
    }
    if native && (model.is_none() || effort.is_none()) {
        return Err("native checks require MODEL and --effort LEVEL".into());
    }
    if native && model.is_some_and(|model| validate_native_model_id(model).is_err()) {
        return Err("native Provider, model, or effort is invalid".into());
    }
    if !native && (model.is_some() || effort.is_some() || accept_cost || saved_account) {
        return Err("custom checks accept only PROFILE and --state-dir".into());
    }
    Ok(())
}

async fn check_chatgpt(
    selected_state_dir: Option<PathBuf>,
    model: Option<String>,
    effort: Option<Effort>,
    accept_cost: bool,
    saved_account: bool,
) -> Result<CommandOutput, String> {
    if saved_account || selected_state_dir.is_some() {
        return Err(
            "ChatGPT checks use the selected user account; omit --saved-account and --state-dir"
                .into(),
        );
    }
    let model = model.ok_or("ChatGPT checks require MODEL and --effort LEVEL")?;
    let effort = effort.ok_or("ChatGPT checks require MODEL and --effort LEVEL")?;
    validate_native_model_id(&model).map_err(|_| "ChatGPT model is invalid")?;
    if !accept_cost {
        return Err("ChatGPT checks may consume plan usage in three synthetic calls. Each has a 1,024-token local acceptance cap, but no remote output-token cap; rerun with --accept-cost".into());
    }
    let workspace = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| "Workspace unavailable".to_owned())?;
    let (account_id, context_window_tokens) =
        chatgpt::check_selected_model(workspace, None, model.clone(), effort)
            .await
            .map_err(|error| error.to_string())?;
    Ok(CommandOutput {
        stdout: render_chatgpt_check(account_id, &model, effort, context_window_tokens),
        stderr: String::new(),
        success: true,
    })
}

async fn check_native(
    state_dir: PathBuf,
    profile: &str,
    model: Option<String>,
    effort: Option<Effort>,
    saved_account: bool,
) -> Result<CommandOutput, String> {
    let root = StateRoot::admit(&state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let model = model.expect("native model checked above");
    let effort = effort.expect("native effort checked above");
    let (key, account_id) = native_credentials(profile, saved_account).await?;
    let cancellation = RunCancellation::new();
    let view = match check_signalled(
        check_native_model(
            root,
            NativeCheckRequest {
                profile: profile.to_owned(),
                model: model.clone(),
                effort,
                credentials: key,
                account_id,
            },
            cancellation.clone(),
            revalidate_native_credentials(profile.to_owned(), account_id),
        ),
        cancellation,
    )
    .await?
    {
        Ok(view) => view,
        Err(NativeCheckError::Check { source, receipt }) => {
            return failed_check(*source, Some(receipt));
        }
        Err(NativeCheckError::Journal(error)) => return failed_check_journal(*error),
        Err(error) => return Err(error.to_string()),
    };
    Ok(CommandOutput {
        stdout: format!(
            "Provider: {profile}\nModel: {model}\nEffort: {effort}\nCredential: {}\nStatus: native synthetic conformance passed\nCheck: {}\nDisposition: {}\n",
            if saved_account {
                "saved API account"
            } else {
                "environment API key"
            },
            view.id,
            view.replay_disposition()
        ),
        stderr: String::new(),
        success: true,
    })
}

async fn native_credentials(
    profile: &str,
    saved_account: bool,
) -> Result<(NativeApiCredentials, Option<uuid::Uuid>), String> {
    if !saved_account {
        return NativeApiCredentials::from_env(profile)
            .map(|credentials| (credentials, None))
            .map_err(|_| NativeCheckError::CredentialUnavailable.to_string());
    }
    let account = credentials::load()
        .await
        .map_err(|error| error.to_string())?
        .ok_or("no saved native API account")?;
    if account.provider != profile {
        return Err("saved API account belongs to a different Provider".into());
    }
    let id = account.id;
    let credentials = account
        .into_credentials()
        .map_err(|error| error.to_string())?;
    Ok((credentials, Some(id)))
}

async fn revalidate_native_credentials(
    profile: String,
    account_id: Option<uuid::Uuid>,
) -> Result<NativeApiCredentials, NativeCheckError> {
    if let Some(id) = account_id {
        let account = credentials::load()
            .await
            .map_err(|_| NativeCheckError::CredentialUnavailable)?
            .ok_or(NativeCheckError::CredentialUnavailable)?;
        if account.id != id || account.provider != profile {
            return Err(NativeCheckError::EvidenceUnavailable);
        }
        account
            .into_credentials()
            .map_err(|_| NativeCheckError::CredentialUnavailable)
    } else {
        NativeApiCredentials::from_env(&profile)
            .map_err(|_| NativeCheckError::CredentialUnavailable)
    }
}

async fn check_custom(state_dir: PathBuf, profile: &str) -> Result<CommandOutput, String> {
    let root = StateRoot::open_existing(&state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let cancellation = RunCancellation::new();
    let report = match check_signalled(
        check_custom_profile_with_cancellation(
            root,
            ProviderKind::custom_name(profile).unwrap_or(profile),
            cancellation.clone(),
        ),
        cancellation,
    )
    .await?
    {
        Ok(report) => report,
        Err(CustomProfileError::Check { source, receipt }) => {
            return failed_check(*source, Some(receipt));
        }
        Err(CustomProfileError::Journal(error)) => return failed_check_journal(*error),
        Err(error) => return Err(error.to_string()),
    };
    Ok(CommandOutput {
        stdout: format!(
            "Provider: {}\nStatus: custom verified\n{}\n",
            report.profile_name(),
            report.receipt()
        ),
        stderr: String::new(),
        success: true,
    })
}

async fn check_signalled<T, E>(
    check: impl std::future::Future<Output = Result<T, E>>,
    cancellation: RunCancellation,
) -> Result<Result<T, E>, String> {
    let mut signals = super::run_outcome::StopSignals::install()
        .map_err(|_| "signal handling unavailable".to_owned())?;
    tokio::pin!(check);
    Ok(tokio::select! {
        biased;
        _ = signals.next() => { cancellation.cancel(); check.await },
        result = &mut check => result,
    })
}

fn failed_check_journal(error: arany::CheckOpenError) -> Result<CommandOutput, String> {
    let reason = if error.receipt.is_some() {
        "Provider check journal failed"
    } else {
        "Provider check could not start"
    };
    failed_check(format!("{reason}: {}", error.source), error.receipt)
}

fn failed_check(
    reason: impl std::fmt::Display,
    receipt: Option<arany::CheckReceipt>,
) -> Result<CommandOutput, String> {
    let reason = reason.to_string();
    let Some(receipt) = receipt else {
        return Err(reason);
    };
    Ok(CommandOutput {
        stdout: String::new(),
        stderr: format!("error: {}\n{receipt}\n", escape_terminal(&reason)),
        success: false,
    })
}

fn render_chatgpt_models(account_id: uuid::Uuid, models: &[chatgpt::ChatGptModel]) -> String {
    let mut output = format!(
        "Provider: chatgpt\nAccount: {}\nModels: {}\n",
        account_id,
        models.len()
    );
    for model in models {
        output.push_str(&format!(
            "{} · {} · context window {} · availability only; compatibility check optional\n",
            model.slug,
            escape_terminal(&model.display_name),
            render_context_window(model.context_window_tokens),
        ));
    }
    output
}

fn render_context_window(tokens: Option<u32>) -> String {
    tokens.map_or_else(|| "unknown".into(), format_tokens)
}

fn render_chatgpt_check(
    account_id: uuid::Uuid,
    model: &str,
    effort: Effort,
    context_window_tokens: Option<u32>,
) -> String {
    let window = render_context_window(context_window_tokens);
    format!(
        "Provider: chatgpt\nAccount: {account_id}\nModel: {model}\nEffort: {effort}\nContext window: {window}\nStatus: synthetic conformance passed; optional diagnostic, not Run authorization\n"
    )
}

fn render_models(profile: &str, models: &[ModelEntry], exact_custom: bool) -> String {
    let mut output = format!("Provider: {profile}\nModels: {}\n", models.len());
    for model in models {
        if exact_custom {
            let efforts = if model.efforts.is_empty() {
                "provider default".to_owned()
            } else {
                format!(
                    "provider default, {}",
                    model
                        .efforts
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            output.push_str(&format!(
                "{} · exact profile; Run requires current conformance; effort {efforts}\n",
                model.id,
            ));
        } else if model.runnable {
            let efforts = model
                .efforts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!(
                "{} · reviewed metadata; effort {efforts}\n",
                model.id
            ));
        } else {
            output.push_str(&format!(
                "{} · availability only; choose effort; compatibility check optional\n",
                model.id
            ));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn failed_check_receipts_preserve_layout_and_escape_the_reason() {
        let id = "01900000-0000-7000-8000-000000000001".parse().unwrap();
        for disposition in [Some(arany::CheckDisposition::Failed), None] {
            let output = failed_check(
                "synthetic\x1b[2J\nfailure",
                Some(arany::CheckReceipt { id, disposition }),
            )
            .unwrap();
            let status = disposition.map_or("unconfirmed".to_owned(), |value| value.to_string());
            assert!(!output.success);
            assert!(output.stdout.is_empty());
            assert_eq!(
                output.stderr,
                format!(
                    "error: synthetic\\u{{001b}}[2J\\u{{000a}}failure\nCheck: {id}\nDisposition: {status}\n"
                )
            );
        }
    }

    #[test]
    fn successful_chatgpt_check_receipt_describes_run_admission() {
        let id = Uuid::now_v7();
        assert_eq!(
            render_chatgpt_check(id, "model-1", Effort::High, Some(272_000)),
            format!(
                "Provider: chatgpt\nAccount: {id}\nModel: model-1\nEffort: high\nContext window: 272k tokens\nStatus: synthetic conformance passed; optional diagnostic, not Run authorization\n"
            )
        );
    }

    #[test]
    fn chatgpt_window_output_uses_human_token_units() {
        let id = Uuid::now_v7();
        for (window, expected) in [
            (None, "unknown"),
            (Some(32_768), "32.8k tokens"),
            (Some(272_000), "272k tokens"),
            (Some(1_048_576), "1M tokens"),
            (Some(2_000_000), "2M tokens"),
        ] {
            let output = render_chatgpt_check(id, "model-one", Effort::High, window);
            assert!(output.contains(&format!("Context window: {expected}\n")));
            let models = render_chatgpt_models(
                id,
                &[chatgpt::ChatGptModel {
                    slug: "model-one".into(),
                    display_name: "First model".into(),
                    context_window_tokens: window,
                }],
            );
            assert!(models.contains(&format!("context window {expected} ·")));
        }
    }

    #[test]
    fn catalog_output_does_not_imply_availability_is_run_support() {
        let output = render_models(
            "openai",
            &[
                ModelEntry {
                    id: "gpt-5.4".into(),
                    runnable: true,
                    efforts: vec![arany::Effort::None, arany::Effort::Low],
                },
                ModelEntry {
                    id: "embedding-only".into(),
                    runnable: false,
                    efforts: vec![],
                },
            ],
            false,
        );
        assert!(output.contains("gpt-5.4 · reviewed metadata; effort none, low"));
        assert!(output.contains(
            "embedding-only · availability only; choose effort; compatibility check optional"
        ));
        assert!(
            render_models(
                "custom:local",
                &[ModelEntry::exact_custom("x".into())],
                true
            )
            .contains("exact profile; Run requires current conformance")
        );
        let mut custom = ModelEntry::exact_custom("model-1".into());
        custom.efforts = vec![arany::Effort::Low, arany::Effort::High];
        assert!(render_models("custom:local", &[custom], true).contains(
            "model-1 · exact profile; Run requires current conformance; effort provider default, low, high"
        ));
    }

    #[test]
    fn chatgpt_catalog_output_keeps_every_slug_inert_without_claiming_support() {
        let account_id = Uuid::now_v7();
        let output = render_chatgpt_models(
            account_id,
            &[
                chatgpt::ChatGptModel {
                    slug: "model-one".into(),
                    display_name: "First model".into(),
                    context_window_tokens: Some(272_000),
                },
                chatgpt::ChatGptModel {
                    slug: "model-two".into(),
                    display_name: "Second\u{1b}[2J model".into(),
                    context_window_tokens: None,
                },
            ],
        );
        assert!(output.starts_with(&format!(
            "Provider: chatgpt\nAccount: {}\nModels: 2\n",
            account_id
        )));
        assert!(
            output.contains(
                "model-one · First model · context window 272k tokens · availability only; compatibility check optional"
            )
        );
        assert!(output.contains(
            "model-two · Second\\u{001b}[2J model · context window unknown · availability only; compatibility check optional"
        ));
        assert!(!output.contains('\u{1b}'));
    }
}
