use crate::provider::Effort;
use crate::store::{StateRoot, Store, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod reducer;
pub(crate) use reducer::CheckReducer;

pub(crate) const MAX_EVENTS: usize = 48;
pub(crate) const MAX_PAYLOAD: usize = 4096;
pub(crate) const MAX_CHECKS: usize = 1024;
const LIFETIME_MS: u64 = 240_000;
const SETTLEMENT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckId(Uuid);

impl Default for CheckId {
    fn default() -> Self {
        Self::new()
    }
}
impl CheckId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub(crate) fn valid(self) -> bool {
        self.0.get_version_num() == 7
    }
}
impl std::fmt::Display for CheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::str::FromStr for CheckId {
    type Err = StoreError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = Self(Uuid::parse_str(value).map_err(|_| StoreError::InvalidHistory)?);
        if !id.valid() || id.to_string() != value {
            return Err(StoreError::InvalidHistory);
        }
        Ok(id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckTarget {
    Custom {
        name: String,
    },
    Native {
        profile: String,
        account: Option<Uuid>,
        model: String,
        effort: Effort,
        workspace: Option<String>,
    },
    Chatgpt {
        account: Uuid,
        model: String,
        effort: Effort,
    },
}
impl CheckTarget {
    pub(crate) fn key(&self) -> Result<[u8; 32], StoreError> {
        let bytes = serde_json::to_vec(self).map_err(|_| StoreError::InvalidHistory)?;
        Ok(Sha256::digest(bytes).into())
    }
    fn valid(&self) -> bool {
        match self {
            Self::Custom { name } => {
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            }
            Self::Native {
                profile,
                account,
                model,
                workspace,
                ..
            } => {
                matches!(profile.as_str(), "openai" | "anthropic")
                    && valid_model(model)
                    && account.is_none_or(|id| id.get_version_num() == 7)
                    && workspace.as_ref().is_none_or(|value| {
                        profile == "anthropic"
                            && value.len() <= 128
                            && value.starts_with("wrkspc_")
                            && value
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
            }
            Self::Chatgpt { account, model, .. } => {
                account.get_version_num() == 7 && valid_model(model)
            }
        }
    }
}
fn valid_model(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCallKind {
    Catalog,
    Answer,
    Delegate,
    Compaction,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckProbe {
    pub kind: CheckCallKind,
    pub effort: Option<Effort>,
    pub output_tokens: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckStart {
    pub target: CheckTarget,
    pub model: String,
    pub fingerprint: [u8; 32],
    pub destination: [u8; 32],
    pub contract: u32,
    pub cost_consent: bool,
    pub catalog_pages: u8,
    pub probes: Vec<CheckProbe>,
    pub started_at_ms: i64,
    pub deadline_ms: i64,
}
impl CheckStart {
    pub(crate) fn new(
        target: CheckTarget,
        model: String,
        fingerprint: [u8; 32],
        destination: [u8; 32],
        catalog_pages: u8,
        probes: Vec<CheckProbe>,
    ) -> Result<Self, StoreError> {
        let started_at_ms = now_ms()?;
        Ok(Self {
            target,
            model,
            fingerprint,
            destination,
            contract: 1,
            cost_consent: true,
            catalog_pages,
            probes,
            started_at_ms,
            deadline_ms: started_at_ms
                .checked_add(LIFETIME_MS as i64)
                .ok_or(StoreError::InvalidHistory)?,
        })
    }
    fn valid(&self) -> bool {
        let (pages, maximum) = match &self.target {
            CheckTarget::Custom { .. } => (0, 15),
            CheckTarget::Native { profile, .. } => (if profile == "openai" { 1 } else { 16 }, 3),
            CheckTarget::Chatgpt { .. } => (1, 3),
        };
        self.target.valid()
            && valid_model(&self.model)
            && match &self.target {
                CheckTarget::Custom { .. } => true,
                CheckTarget::Native { model, .. } | CheckTarget::Chatgpt { model, .. } => {
                    *model == self.model
                }
            }
            && self.contract == 1
            && self.cost_consent
            && self.catalog_pages == pages
            && self.started_at_ms >= 0
            && self.deadline_ms.checked_sub(self.started_at_ms) == Some(LIFETIME_MS as i64)
            && self.probes.len() >= 3
            && self.probes.len() <= maximum
            && self.probes[0].kind == CheckCallKind::Answer
            && self.probes[1].kind == CheckCallKind::Delegate
            && self.probes[2].kind == CheckCallKind::Compaction
            && self.probes.iter().all(|p| {
                p.kind != CheckCallKind::Catalog && p.output_tokens > 0 && p.output_tokens <= 4096
            })
            && match &self.target {
                CheckTarget::Custom { .. } => {
                    self.probes[..3].iter().all(|probe| probe.effort.is_none())
                        && self.probes.iter().enumerate().all(|(index, probe)| {
                            probe.output_tokens == if index == 0 { 4096 } else { 128 }
                        })
                        && self.probes[3..]
                            .chunks(2)
                            .filter_map(|pair| pair.first().and_then(|probe| probe.effort))
                            .map(|effort| Effort::ALL.iter().position(|value| *value == effort))
                            .collect::<Vec<_>>()
                            .windows(2)
                            .all(|pair| pair[0] < pair[1])
                }
                CheckTarget::Native { effort, .. } | CheckTarget::Chatgpt { effort, .. } => self
                    .probes
                    .iter()
                    .all(|probe| probe.effort == Some(*effort) && probe.output_tokens == 1024),
            }
            && self.probes[3..].chunks(2).all(|pair| {
                pair.len() == 2
                    && pair[0].kind == CheckCallKind::Answer
                    && pair[1].kind == CheckCallKind::Compaction
                    && pair[0].effort.is_some()
                    && pair[0].effort == pair[1].effort
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckDisposition {
    Succeeded,
    Rejected,
    Failed,
    Cancelled,
    Uncertain,
}
impl std::fmt::Display for CheckDisposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Succeeded => "succeeded",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", deny_unknown_fields)]
pub enum CheckEvent {
    ProviderCheckStarted {
        start: CheckStart,
    },
    ProviderCheckEvidenceCleared,
    ProviderCheckCallStarted {
        ordinal: u8,
        kind: CheckCallKind,
        destination: [u8; 32],
        calls_remaining: u8,
        output_remaining: u32,
        time_remaining_ms: u64,
    },
    ProviderCheckCallFinished {
        ordinal: u8,
        disposition: CheckDisposition,
        usage: Option<CheckUsage>,
        catalog_complete: bool,
    },
    ProviderCheckEvidenceStarted {
        digest: [u8; 32],
        snapshot: [u8; 32],
    },
    ProviderCheckFinished {
        disposition: CheckDisposition,
        evidence: Option<[u8; 32]>,
    },
}
impl CheckEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ProviderCheckStarted { .. } => "ProviderCheckStarted",
            Self::ProviderCheckEvidenceCleared => "ProviderCheckEvidenceCleared",
            Self::ProviderCheckCallStarted { .. } => "ProviderCheckCallStarted",
            Self::ProviderCheckCallFinished { .. } => "ProviderCheckCallFinished",
            Self::ProviderCheckEvidenceStarted { .. } => "ProviderCheckEvidenceStarted",
            Self::ProviderCheckFinished { .. } => "ProviderCheckFinished",
        }
    }
    pub fn version(&self) -> i64 {
        CHECK_FORMATS
            .iter()
            .find(|(kind, _, _)| *kind == self.kind())
            .expect("every check Event kind has a payload version")
            .2
    }
}
pub const CHECK_FORMATS: &[(&str, i64, i64)] = &[
    ("ProviderCheckStarted", 1, 1),
    ("ProviderCheckEvidenceCleared", 1, 1),
    ("ProviderCheckCallStarted", 1, 1),
    ("ProviderCheckCallFinished", 1, 1),
    ("ProviderCheckEvidenceStarted", 1, 1),
    ("ProviderCheckFinished", 1, 1),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckView {
    pub id: CheckId,
    pub start: CheckStart,
    pub events: usize,
    pub completed_calls: u8,
    pub pending: Option<(u8, CheckCallKind)>,
    pub disposition: Option<CheckDisposition>,
    pub(crate) cleared: bool,
    pub(crate) catalog_pages: u8,
    pub(crate) catalog_complete: bool,
    pub(crate) probes: usize,
    pub(crate) failed: Option<CheckDisposition>,
    pub(crate) publication: Option<[u8; 32]>,
    pub(crate) last_time_remaining_ms: u64,
}
impl CheckView {
    pub fn replay(id: CheckId, events: &[CheckEvent]) -> Result<Option<Self>, StoreError> {
        events.iter().try_fold(None, |view, event| {
            CheckReducer::apply(id, view, event).map(Some)
        })
    }

    pub fn replay_disposition(&self) -> CheckDisposition {
        self.disposition.unwrap_or(CheckDisposition::Uncertain)
    }
    pub(crate) fn calls_remaining(&self) -> u8 {
        self.start.probes.len().saturating_sub(self.probes) as u8
    }
    pub(crate) fn output_remaining(&self) -> u32 {
        self.start.probes[self.probes..]
            .iter()
            .map(|p| p.output_tokens)
            .sum()
    }
}

pub(crate) struct CheckJournal {
    store: Store,
    _lease: crate::store::ProviderCheckLock,
    view: CheckView,
    deadline: tokio::time::Instant,
    confirmed: bool,
}

pub(crate) enum CheckOp {
    #[cfg(test)]
    Seed(crate::provider::evidence::EvidenceRecord),
    Start {
        id: CheckId,
        start: CheckStart,
    },
    Append {
        id: CheckId,
        event: CheckEvent,
    },
    PublishProvider {
        id: CheckId,
        record: crate::provider::evidence::ProviderEvidenceRecord,
    },
    PublishNative {
        id: CheckId,
        record: crate::provider::evidence::NativeEvidenceRecord,
    },
    Recover,
    Load(CheckId),
}

impl CheckJournal {
    pub(crate) async fn start(root: StateRoot, start: CheckStart) -> Result<Self, CheckOpenError> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(LIFETIME_MS);
        let lease = root.lock_provider_check()?;
        let store = Store::open(root)?;
        tokio::time::timeout_at(deadline, store.check(CheckOp::Recover))
            .await
            .map_err(|_| StoreError::Closed)??;
        let id = CheckId::new();
        let view = tokio::time::timeout_at(deadline, store.check(CheckOp::Start { id, start }))
            .await
            .map_err(|_| StoreError::Closed)
            .and_then(|result| result)
            .and_then(|view| view.ok_or(StoreError::InvalidHistory))
            .map_err(|source| {
                let ambiguous = matches!(source, StoreError::Closed | StoreError::Sqlite(_));
                CheckOpenError {
                    source,
                    receipt: ambiguous.then_some(CheckReceipt {
                        id,
                        disposition: None,
                    }),
                }
            })?;
        Ok(Self {
            store,
            _lease: lease,
            view,
            deadline,
            confirmed: true,
        })
    }
    #[cfg(test)]
    pub(crate) fn view(&self) -> &CheckView {
        &self.view
    }
    pub(crate) fn request_deadline(&self) -> tokio::time::Instant {
        self.deadline - Duration::from_millis(SETTLEMENT_MS)
    }
    async fn write(&mut self, op: CheckOp) -> Result<(), StoreError> {
        if !self.confirmed {
            return Err(StoreError::Closed);
        }
        self.confirmed = false;
        self.view = tokio::time::timeout_at(self.deadline, self.store.check(op))
            .await
            .map_err(|_| StoreError::Closed)??
            .ok_or(StoreError::InvalidHistory)?;
        self.confirmed = true;
        Ok(())
    }
    pub(crate) async fn append(&mut self, event: CheckEvent) -> Result<(), StoreError> {
        self.write(CheckOp::Append {
            id: self.view.id,
            event,
        })
        .await
    }
    pub(crate) async fn call_started(
        &mut self,
        kind: CheckCallKind,
    ) -> Result<CheckCallToken, StoreError> {
        let remaining = self
            .request_deadline()
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(StoreError::InvalidHistory)?;
        let ordinal = self.view.completed_calls;
        self.append(CheckEvent::ProviderCheckCallStarted {
            ordinal,
            kind,
            destination: self.view.start.destination,
            calls_remaining: self.view.calls_remaining(),
            output_remaining: self.view.output_remaining(),
            time_remaining_ms: remaining
                .as_millis()
                .min((LIFETIME_MS - SETTLEMENT_MS) as u128) as u64,
        })
        .await?;
        Ok(CheckCallToken {
            id: self.view.id,
            ordinal,
        })
    }
    pub(crate) async fn call_finished(
        &mut self,
        token: CheckCallToken,
        disposition: CheckDisposition,
        usage: Option<CheckUsage>,
        catalog_complete: bool,
    ) -> Result<(), StoreError> {
        if token.id != self.view.id {
            return Err(StoreError::InvalidHistory);
        }
        self.append(CheckEvent::ProviderCheckCallFinished {
            ordinal: token.ordinal,
            disposition,
            usage,
            catalog_complete,
        })
        .await
    }
    pub(crate) async fn evidence_started(
        &mut self,
        digest: [u8; 32],
        snapshot: [u8; 32],
    ) -> Result<(), StoreError> {
        self.append(CheckEvent::ProviderCheckEvidenceStarted { digest, snapshot })
            .await
    }
    pub(crate) async fn publish_provider(
        &mut self,
        record: crate::provider::evidence::ProviderEvidenceRecord,
    ) -> Result<(), StoreError> {
        self.write(CheckOp::PublishProvider {
            id: self.view.id,
            record,
        })
        .await
    }
    pub(crate) async fn publish_native(
        &mut self,
        record: crate::provider::evidence::NativeEvidenceRecord,
    ) -> Result<(), StoreError> {
        self.write(CheckOp::PublishNative {
            id: self.view.id,
            record,
        })
        .await
    }
    pub(crate) async fn reject_pending(
        &mut self,
        disposition: CheckDisposition,
    ) -> Result<(), StoreError> {
        if let Some((ordinal, _)) = self.view.pending {
            self.append(CheckEvent::ProviderCheckCallFinished {
                ordinal,
                disposition,
                usage: None,
                catalog_complete: false,
            })
            .await?;
        }
        Ok(())
    }
    pub(crate) fn receipt(&self) -> CheckReceipt {
        CheckReceipt {
            id: self.view.id,
            disposition: self.confirmed.then_some(self.view.disposition).flatten(),
        }
    }
    pub(crate) async fn finish(
        &mut self,
        mut disposition: CheckDisposition,
    ) -> Result<(), StoreError> {
        if let Some((ordinal, _)) = self.view.pending {
            disposition = CheckDisposition::Uncertain;
            self.append(CheckEvent::ProviderCheckCallFinished {
                ordinal,
                disposition,
                usage: None,
                catalog_complete: false,
            })
            .await?;
        }
        if self.view.publication.is_some() || self.view.failed == Some(CheckDisposition::Uncertain)
        {
            disposition = CheckDisposition::Uncertain;
        }
        self.append(CheckEvent::ProviderCheckFinished {
            disposition,
            evidence: None,
        })
        .await
    }
    pub(crate) async fn close(self) -> Result<CheckView, StoreError> {
        self.store.close().await?;
        Ok(self.view)
    }
}

#[derive(Debug)]
pub struct CheckOpenError {
    pub source: StoreError,
    pub receipt: Option<CheckReceipt>,
}
impl From<StoreError> for CheckOpenError {
    fn from(source: StoreError) -> Self {
        Self {
            source,
            receipt: None,
        }
    }
}
impl std::fmt::Display for CheckOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)?;
        if let Some(receipt) = self.receipt {
            write!(f, "\n{receipt}")?;
        }
        Ok(())
    }
}
impl std::error::Error for CheckOpenError {}

#[derive(Clone, Copy, Debug)]
pub struct CheckReceipt {
    pub id: CheckId,
    pub disposition: Option<CheckDisposition>,
}
impl std::fmt::Display for CheckReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Check: {}\nDisposition: ", self.id)?;
        match self.disposition {
            Some(value) => value.fmt(f),
            None => f.write_str("unconfirmed"),
        }
    }
}
pub(crate) struct CheckCallToken {
    id: CheckId,
    ordinal: u8,
}
pub(crate) fn now_ms() -> Result<i64, StoreError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::InvalidHistory)?
            .as_millis(),
    )
    .map_err(|_| StoreError::InvalidHistory)
}

#[cfg(test)]
pub(crate) mod tests;
