pub(crate) mod roster;
pub(crate) mod routing;
pub(crate) mod select;

use super::{CommandOutput, state_dir};
use crate::accounts::{chatgpt, credentials};
use crate::presentation::{escape_terminal, format_tokens};
use arany::{
    CustomProfile, ModelEntry, NativeApiCredentials, ProviderKind, StateRoot,
    list_native_models_with_credentials,
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
}

pub(crate) async fn run(args: ProviderArgs) -> Result<CommandOutput, String> {
    match args.action {
        ProviderAction::Logout { profile } => logout(profile).await,
        ProviderAction::Models {
            state_dir,
            saved_account,
            profile,
        } => models(state_dir, saved_account, profile).await,
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
                saved_native_credentials(&profile).await?.0
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

async fn saved_native_credentials(
    profile: &str,
) -> Result<(NativeApiCredentials, uuid::Uuid), String> {
    let account = credentials::load()
        .await
        .map_err(|error| error.to_string())?
        .ok_or("no saved native API account")?;
    if account.provider.profile() != profile {
        return Err("saved API account belongs to a different Provider".into());
    }
    let id = account.id;
    let credentials = account
        .into_credentials()
        .map_err(|error| error.to_string())?;
    Ok((credentials, id))
}

fn render_chatgpt_models(account_id: uuid::Uuid, models: &[chatgpt::ChatGptModel]) -> String {
    let mut output = format!(
        "Provider: chatgpt\nAccount: {}\nModels: {}\n",
        account_id,
        models.len()
    );
    for model in models {
        output.push_str(&format!(
            "{} · {} · context window {} · availability only\n",
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
            output.push_str(&format!("{} · exact profile; effort {efforts}\n", model.id,));
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
                "{} · availability only; choose effort\n",
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
    fn chatgpt_window_output_uses_human_token_units() {
        let id = Uuid::now_v7();
        for (window, expected) in [
            (None, "unknown"),
            (Some(32_768), "32.8k tokens"),
            (Some(272_000), "272k tokens"),
            (Some(1_048_576), "1M tokens"),
            (Some(2_000_000), "2M tokens"),
        ] {
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
                    context_window_tokens: None,
                    runnable: true,
                    efforts: vec![arany::Effort::None, arany::Effort::Low],
                },
                ModelEntry {
                    id: "embedding-only".into(),
                    context_window_tokens: None,
                    runnable: false,
                    efforts: vec![],
                },
            ],
            false,
        );
        assert!(output.contains("gpt-5.4 · reviewed metadata; effort none, low"));
        assert!(output.contains("embedding-only · availability only; choose effort"));
        assert!(
            render_models(
                "custom:local",
                &[ModelEntry::exact_custom("x".into())],
                true
            )
            .contains("exact profile")
        );
        let mut custom = ModelEntry::exact_custom("model-1".into());
        custom.efforts = vec![arany::Effort::Low, arany::Effort::High];
        assert!(
            render_models("custom:local", &[custom], true)
                .contains("model-1 · exact profile; effort provider default, low, high")
        );
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
                "model-one · First model · context window 272k tokens · availability only"
            )
        );
        assert!(output.contains(
            "model-two · Second\\u{001b}[2J model · context window unknown · availability only"
        ));
        assert!(!output.contains('\u{1b}'));
    }
}
