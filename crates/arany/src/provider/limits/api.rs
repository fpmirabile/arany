use super::{LimitFailure, LimitSource, LimitUnavailable, MAX_LIMIT_TIMESTAMP};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiLimitDialect {
    OpenAi,
    Anthropic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiLimitDimension {
    Requests,
    Tokens,
    ProjectTokens,
    InputTokens,
    OutputTokens,
    PriorityInputTokens,
    PriorityOutputTokens,
}

impl ApiLimitDimension {
    pub const ALL: &[Self] = &[
        Self::Requests,
        Self::Tokens,
        Self::ProjectTokens,
        Self::InputTokens,
        Self::OutputTokens,
        Self::PriorityInputTokens,
        Self::PriorityOutputTokens,
    ];
}

impl ApiLimitDialect {
    pub fn admits(self, dimension: ApiLimitDimension) -> bool {
        use ApiLimitDimension::*;
        match self {
            Self::OpenAi => matches!(dimension, Requests | Tokens | ProjectTokens),
            Self::Anthropic => matches!(
                dimension,
                Requests
                    | Tokens
                    | InputTokens
                    | OutputTokens
                    | PriorityInputTokens
                    | PriorityOutputTokens
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiLimit {
    pub dimension: ApiLimitDimension,
    pub source: LimitSource,
    pub observed_at: i64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub limit: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub remaining: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub reset_at: Option<i64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub unavailable: Option<LimitUnavailable>,
}

impl ApiLimit {
    pub fn valid(&self) -> bool {
        self.source == LimitSource::Headers
            && (0..=MAX_LIMIT_TIMESTAMP).contains(&self.observed_at)
            && self
                .reset_at
                .is_none_or(|at| (0..=MAX_LIMIT_TIMESTAMP).contains(&at))
            && if self.unavailable.is_some() {
                self.limit.is_none() && self.remaining.is_none() && self.reset_at.is_none()
            } else {
                self.limit.is_some() || self.remaining.is_some() || self.reset_at.is_some()
            }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiLimits {
    pub dialect: ApiLimitDialect,
    pub rows: Vec<ApiLimit>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub unavailable: Option<LimitFailure>,
}

impl ApiLimits {
    pub fn valid(&self) -> bool {
        self.rows.len() <= super::MAX_API_LIMIT_ROWS
            && (!self.rows.is_empty() || self.unavailable.is_some())
            && self.unavailable.is_none_or(|failure| {
                failure.source == LimitSource::Headers
                    && (0..=MAX_LIMIT_TIMESTAMP).contains(&failure.observed_at)
            })
            && self.rows.iter().enumerate().all(|(index, row)| {
                row.valid()
                    && self.dialect.admits(row.dimension)
                    && !self.rows[..index]
                        .iter()
                        .any(|old| old.dimension == row.dimension)
            })
    }
}
