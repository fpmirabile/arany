use super::ProviderError;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err("invalid effort"),
        }
    }
}

pub fn resolve_native_effort(
    provider: &str,
    model: &str,
    selected: Option<Effort>,
) -> Result<Effort, ProviderError> {
    const LOW_TO_MAX: &[Effort] = &[
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];
    let (default, allowed): (Effort, &[Effort]) = match (provider, model) {
        ("openai", "gpt-5.4") => (
            Effort::None,
            &[
                Effort::None,
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
            ],
        ),
        ("openai", "gpt-6-astra" | "gpt-6.1-sol") => (Effort::Medium, LOW_TO_MAX),
        ("openai", "gpt-5.6-luna" | "gpt-6-luna") => (Effort::Medium, &Effort::ALL),
        ("anthropic", "claude-sonnet-5" | "claude-fable-5-1" | "claude-sonnet-5-5") => {
            (Effort::High, LOW_TO_MAX)
        }
        ("anthropic", "claude-opus-5-5") => (Effort::Medium, LOW_TO_MAX),
        _ => return Err(ProviderError::Rejected),
    };
    let resolved = selected.unwrap_or(default);
    if allowed.contains(&resolved) {
        Ok(resolved)
    } else {
        Err(ProviderError::Rejected)
    }
}

/// Resolves a native Run choice without treating unknown model metadata as compatibility evidence.
pub fn resolve_native_effort_for_run(
    provider: &str,
    model: &str,
    selected: Option<Effort>,
) -> Result<Effort, ProviderError> {
    if !matches!(provider, "openai" | "anthropic") {
        return Err(ProviderError::Rejected);
    }
    super::validate_native_model_id(model)?;
    if resolve_native_effort(provider, model, None).is_ok() {
        resolve_native_effort(provider, model, selected)
    } else {
        selected.ok_or(ProviderError::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_specific_effort_admission_is_closed() {
        for (provider, model, default, allowed) in [
            (
                "openai",
                "gpt-5.4",
                Effort::None,
                &[
                    Effort::None,
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                ][..],
            ),
            (
                "openai",
                "gpt-6-astra",
                Effort::Medium,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ][..],
            ),
            (
                "openai",
                "gpt-6.1-sol",
                Effort::Medium,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ][..],
            ),
            ("openai", "gpt-6-luna", Effort::Medium, &Effort::ALL[..]),
            ("openai", "gpt-5.6-luna", Effort::Medium, &Effort::ALL[..]),
            (
                "anthropic",
                "claude-sonnet-5",
                Effort::High,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ][..],
            ),
            (
                "anthropic",
                "claude-fable-5-1",
                Effort::High,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ][..],
            ),
            (
                "anthropic",
                "claude-opus-5-5",
                Effort::Medium,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ][..],
            ),
            (
                "anthropic",
                "claude-sonnet-5-5",
                Effort::High,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ][..],
            ),
        ] {
            assert_eq!(
                resolve_native_effort(provider, model, None).unwrap(),
                default
            );
            assert_eq!(
                resolve_native_effort_for_run(provider, model, None).unwrap(),
                default
            );
            for effort in Effort::ALL {
                assert_eq!(
                    resolve_native_effort(provider, model, Some(effort)).is_ok(),
                    allowed.contains(&effort),
                    "{provider}/{model}/{effort}"
                );
                assert_eq!(effort.as_str().parse::<Effort>(), Ok(effort));
                assert_eq!(
                    resolve_native_effort_for_run(provider, model, Some(effort)).is_ok(),
                    allowed.contains(&effort)
                );
            }
        }
        assert!(resolve_native_effort("openai", "unreviewed", None).is_err());
        assert!(resolve_native_effort("anthropic", "unreviewed", None).is_err());
        assert!(resolve_native_effort("openai", "gpt-6-astra", Some(Effort::None)).is_err());
        assert!(resolve_native_effort("anthropic", "claude-opus-5-5", Some(Effort::None)).is_err());
        assert!(resolve_native_effort("custom:local", "model-1", None).is_err());
        assert!("turbo".parse::<Effort>().is_err());
        for provider in ["openai", "anthropic"] {
            assert!(resolve_native_effort_for_run(provider, "future-model", None).is_err());
            for effort in Effort::ALL {
                assert_eq!(
                    resolve_native_effort_for_run(provider, "future-model", Some(effort)).unwrap(),
                    effort
                );
                for model in ["", "bad\nmodel", "bad model", "模型", &"x".repeat(129)] {
                    assert!(resolve_native_effort_for_run(provider, model, Some(effort)).is_err());
                }
            }
        }
        assert!(resolve_native_effort_for_run("custom:local", "model", Some(Effort::Low)).is_err());
    }
}
