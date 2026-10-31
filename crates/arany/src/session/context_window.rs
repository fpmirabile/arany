use crate::provider::{MAX_CONTEXT_WINDOW_TOKENS, MIN_CONTEXT_WINDOW_TOKENS};
use std::fmt;
use std::str::FromStr;

pub const MIN_CONTEXT_WINDOW_PERCENT: u8 = 10;
const FULL_CONTEXT_WINDOW_PERCENT: u8 = 100;
const TOKEN_SUFFIXES: [(char, u32); 2] = [('k', 1_000), ('m', 1_000_000)];

/// The share of the model's context window a Run budgets against.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ContextWindow {
    Percent(u8),
    Tokens(u32),
}

impl Default for ContextWindow {
    fn default() -> Self {
        Self::Percent(FULL_CONTEXT_WINDOW_PERCENT)
    }
}

impl ContextWindow {
    #[must_use]
    pub fn valid(self) -> bool {
        match self {
            Self::Percent(percent) => {
                (MIN_CONTEXT_WINDOW_PERCENT..=FULL_CONTEXT_WINDOW_PERCENT).contains(&percent)
            }
            Self::Tokens(tokens) => {
                (MIN_CONTEXT_WINDOW_TOKENS..=MAX_CONTEXT_WINDOW_TOKENS).contains(&tokens)
            }
        }
    }

    // An unknown model window keeps byte budgets; a larger setting is clamped to the window.
    pub(crate) fn resolve(self, model_window: Option<u32>) -> Option<u32> {
        let window = model_window?;
        let requested = match self {
            Self::Percent(percent) => {
                u64::from(window) * u64::from(percent) / u64::from(FULL_CONTEXT_WINDOW_PERCENT)
            }
            Self::Tokens(tokens) => u64::from(tokens),
        };
        Some(
            u32::try_from(requested)
                .unwrap_or(u32::MAX)
                .clamp(MIN_CONTEXT_WINDOW_TOKENS.min(window), window),
        )
    }
}

impl FromStr for ContextWindow {
    type Err = &'static str;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let invalid = "context window is a percentage (e.g. 80%) or tokens (e.g. 500k)";
        let raw = raw.trim();
        let value = if let Some(percent) = raw.strip_suffix('%') {
            Self::Percent(percent.trim_end().parse().map_err(|_| invalid)?)
        } else {
            let (digits, scale) = TOKEN_SUFFIXES
                .iter()
                .find_map(|(suffix, scale)| {
                    raw.strip_suffix(*suffix)
                        .or_else(|| raw.strip_suffix(suffix.to_ascii_uppercase()))
                        .map(|digits| (digits.trim_end(), *scale))
                })
                .unwrap_or((raw, 1));
            if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid);
            }
            let tokens: u32 = digits.parse().map_err(|_| invalid)?;
            Self::Tokens(tokens.checked_mul(scale).ok_or(invalid)?)
        };
        value.valid().then_some(value).ok_or(invalid)
    }
}

impl fmt::Display for ContextWindow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Percent(percent) => write!(formatter, "{percent}%"),
            Self::Tokens(tokens) => match TOKEN_SUFFIXES
                .iter()
                .rev()
                .find(|(_, scale)| tokens % scale == 0)
            {
                Some((suffix, scale)) => write!(formatter, "{}{suffix}", tokens / scale),
                None => write!(formatter, "{tokens}"),
            },
        }
    }
}

impl TryFrom<String> for ContextWindow {
    type Error = &'static str;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<ContextWindow> for String {
    fn from(value: ContextWindow) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_percentages_and_token_counts_within_named_bounds() {
        for (raw, expected) in [
            ("80%", Some(ContextWindow::Percent(80))),
            (" 10 % ", Some(ContextWindow::Percent(10))),
            ("100%", Some(ContextWindow::Percent(100))),
            ("9%", None),
            ("101%", None),
            ("500k", Some(ContextWindow::Tokens(500_000))),
            ("500K", Some(ContextWindow::Tokens(500_000))),
            ("500000", Some(ContextWindow::Tokens(500_000))),
            ("1m", Some(ContextWindow::Tokens(1_000_000))),
            (
                "32768",
                Some(ContextWindow::Tokens(MIN_CONTEXT_WINDOW_TOKENS)),
            ),
            ("32767", None),
            ("2000001", None),
            ("-5k", None),
            ("k", None),
            ("", None),
        ] {
            assert_eq!(raw.parse::<ContextWindow>().ok(), expected, "{raw}");
        }
        for value in [
            ContextWindow::default(),
            ContextWindow::Tokens(500_000),
            ContextWindow::Tokens(1_000_000),
            ContextWindow::Tokens(123_456),
        ] {
            assert_eq!(value.to_string().parse(), Ok(value));
            assert_eq!(
                serde_json::from_value::<ContextWindow>(serde_json::to_value(value).unwrap())
                    .unwrap(),
                value
            );
        }
        assert!(serde_json::from_value::<ContextWindow>(serde_json::json!("5%")).is_err());
    }

    #[test]
    fn effective_window_resolves_against_the_model_window_and_clamps() {
        for (setting, model, expected) in [
            (ContextWindow::default(), Some(272_000), Some(272_000)),
            (ContextWindow::Percent(80), Some(272_000), Some(217_600)),
            (
                ContextWindow::Tokens(500_000),
                Some(1_000_000),
                Some(500_000),
            ),
            (ContextWindow::Tokens(500_000), Some(272_000), Some(272_000)),
            (
                ContextWindow::Percent(10),
                Some(200_000),
                Some(MIN_CONTEXT_WINDOW_TOKENS),
            ),
            (ContextWindow::Tokens(500_000), None, None),
            (ContextWindow::Percent(50), None, None),
        ] {
            assert_eq!(setting.resolve(model), expected, "{setting} of {model:?}");
        }
    }
}
