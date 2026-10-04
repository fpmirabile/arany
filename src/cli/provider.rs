use super::{CommandOutput, chatgpt, credentials, state_dir};
use arany::{
    CustomProfile, Effort, ModelEntry, NativeApiCredentials, StateRoot, check_custom_profile,
    check_native_model_from_env, check_native_model_with_credentials, escape_terminal,
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

pub(crate) async fn run(args: ProviderArgs) -> Result<CommandOutput, String> {
    match args.action {
        ProviderAction::Logout { profile } => {
            if profile != "chatgpt" {
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
                stderr.push_str("warning: remote revocation was not confirmed; disconnect Arany in ChatGPT Settings\n");
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
        ProviderAction::Models {
            state_dir: selected_state_dir,
            saved_account,
            profile,
        } => {
            if profile == "chatgpt" {
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
            let (models, exact_custom) = match profile.as_str() {
                "openai" | "anthropic" => {
                    let key = if saved_account {
                        let workspace = std::env::current_dir()
                            .and_then(std::fs::canonicalize)
                            .map_err(|_| "Workspace unavailable".to_owned())?;
                        let account = credentials::load(&workspace)
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
                        NativeApiCredentials::from_env(&profile)
                            .map_err(|error| error.to_string())?
                    };
                    let models = list_native_models_with_credentials(&profile, &key)
                        .await
                        .map_err(|error| error.to_string())?;
                    (models, false)
                }
                _ => {
                    if saved_account {
                        return Err("--saved-account requires a native Provider".into());
                    }
                    let Some(name) = profile.strip_prefix("custom:") else {
                        return Err(
                            "Provider must be openai, anthropic, chatgpt, or custom:NAME".into(),
                        );
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
                    let custom = CustomProfile::load_named(&root, name)
                        .map_err(|error| error.to_string())?;
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
        ProviderAction::Check {
            state_dir: selected_state_dir,
            profile,
            model,
            effort,
            accept_cost,
            saved_account,
        } => {
            if profile == "chatgpt" {
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
                let account_id =
                    chatgpt::check_selected_model(workspace, None, model.clone(), effort)
                        .await
                        .map_err(|error| error.to_string())?;
                return Ok(CommandOutput {
                    stdout: render_chatgpt_check(account_id, &model, effort),
                    stderr: String::new(),
                    success: true,
                });
            }
            let native = matches!(profile.as_str(), "openai" | "anthropic");
            if native && !accept_cost {
                return Err("native checks may bill up to 3 inference calls and 3,072 generated tokens, plus bounded catalog/input usage; rerun with --accept-cost".into());
            }
            if native && (model.is_none() || effort.is_none()) {
                return Err("native checks require MODEL and --effort LEVEL".into());
            }
            if native
                && model
                    .as_deref()
                    .is_some_and(|model| validate_native_model_id(model).is_err())
            {
                return Err("native Provider, model, or effort is invalid".into());
            }
            if !native && (model.is_some() || effort.is_some() || accept_cost || saved_account) {
                return Err("custom checks accept only PROFILE and --state-dir".into());
            }
            let state_dir = state_dir(selected_state_dir).map_err(str::to_owned)?;
            let workspace = std::env::current_dir()
                .and_then(std::fs::canonicalize)
                .map_err(|_| "Workspace unavailable".to_owned())?;
            if state_dir.starts_with(&workspace) {
                return Err("state directory overlaps the Workspace".into());
            }
            if native {
                let root = StateRoot::admit(&state_dir)
                    .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
                let model = model.expect("native model checked above");
                let effort = effort.expect("native effort checked above");
                if saved_account {
                    let account = credentials::load(&workspace)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or("no saved native API account")?;
                    if account.provider != profile {
                        return Err("saved API account belongs to a different Provider".into());
                    }
                    let account_id = account.id;
                    check_native_model_with_credentials(
                        root,
                        &profile,
                        &model,
                        effort,
                        account
                            .into_credentials()
                            .map_err(|error| error.to_string())?,
                        Some(account_id),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                } else {
                    check_native_model_from_env(root, &profile, &model, effort)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                return Ok(CommandOutput {
                    stdout: format!(
                        "Provider: {profile}\nModel: {model}\nEffort: {effort}\nCredential: {}\nStatus: native synthetic conformance passed\n",
                        if saved_account {
                            "saved API account"
                        } else {
                            "environment API key"
                        }
                    ),
                    stderr: String::new(),
                    success: true,
                });
            }
            let root = StateRoot::open_existing(&state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let report = check_custom_profile(root, &profile)
                .await
                .map_err(|error| error.to_string())?;
            Ok(CommandOutput {
                stdout: format!(
                    "Provider: {}\nStatus: custom verified\n",
                    report.profile_name()
                ),
                stderr: String::new(),
                success: true,
            })
        }
    }
}

fn render_chatgpt_models(account_id: uuid::Uuid, models: &[chatgpt::ChatGptModel]) -> String {
    let mut output = format!(
        "Provider: chatgpt\nAccount: {}\nModels: {}\n",
        account_id,
        models.len()
    );
    for model in models {
        output.push_str(&format!(
            "{} · {} · availability only; compatibility check optional\n",
            model.slug,
            escape_terminal(&model.display_name)
        ));
    }
    output
}

fn render_chatgpt_check(account_id: uuid::Uuid, model: &str, effort: Effort) -> String {
    format!(
        "Provider: chatgpt\nAccount: {account_id}\nModel: {model}\nEffort: {effort}\nStatus: synthetic conformance passed; optional diagnostic, not Run authorization\n"
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
    fn successful_chatgpt_check_receipt_describes_run_admission() {
        let id = Uuid::now_v7();
        assert_eq!(
            render_chatgpt_check(id, "model-1", Effort::High),
            format!(
                "Provider: chatgpt\nAccount: {id}\nModel: model-1\nEffort: high\nStatus: synthetic conformance passed; optional diagnostic, not Run authorization\n"
            )
        );
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
                },
                chatgpt::ChatGptModel {
                    slug: "model-two".into(),
                    display_name: "Second\u{1b}[2J model".into(),
                },
            ],
        );
        assert!(output.starts_with(&format!(
            "Provider: chatgpt\nAccount: {}\nModels: 2\n",
            account_id
        )));
        assert!(
            output.contains(
                "model-one · First model · availability only; compatibility check optional"
            )
        );
        assert!(output.contains(
            "model-two · Second\\u{001b}[2J model · availability only; compatibility check optional"
        ));
        assert!(!output.contains('\u{1b}'));
    }
}
