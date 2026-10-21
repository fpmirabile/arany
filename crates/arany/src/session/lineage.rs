use super::{EventEnvelope, ReplayError};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub(crate) struct PrefixHasher(Sha256);

// Stored Event text as written; the digest at write time covers exactly these bytes.
pub(crate) struct PrefixRow<'a> {
    pub(crate) sequence: u64,
    pub(crate) session_id: &'a str,
    pub(crate) run_id: Option<&'a str>,
    pub(crate) agent_run_id: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) payload: &'a str,
    pub(crate) created_at_ms: i64,
}

impl Default for PrefixHasher {
    fn default() -> Self {
        Self::new()
    }
}
impl PrefixHasher {
    pub(crate) fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"arany-session-prefix-v1\0");
        Self(hasher)
    }

    pub(crate) fn update(&mut self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        let (run_id, agent_run_id) = (
            envelope.run_id.map(|id| id.to_string()),
            envelope.agent_run_id.map(|id| id.to_string()),
        );
        self.update_row(&PrefixRow {
            sequence: envelope.sequence,
            session_id: &envelope.session_id.to_string(),
            run_id: run_id.as_deref(),
            agent_run_id: agent_run_id.as_deref(),
            kind: envelope.event.kind(),
            payload: &envelope.event.payload()?,
            created_at_ms: envelope.created_at_ms,
        })
    }

    pub(crate) fn update_row(&mut self, row: &PrefixRow<'_>) -> Result<(), ReplayError> {
        self.0.update(row.sequence.to_be_bytes());
        self.0.update(row.session_id.as_bytes());
        for scope in [row.run_id, row.agent_run_id] {
            match scope {
                Some(id) => {
                    self.0.update([1]);
                    self.0.update(id.as_bytes());
                }
                None => self.0.update([0]),
            }
        }
        for field in [row.kind, row.payload] {
            self.0.update(
                u32::try_from(field.len())
                    .map_err(|_| ReplayError::InvalidPayload)?
                    .to_be_bytes(),
            );
            self.0.update(field.as_bytes());
        }
        self.0.update(row.created_at_ms.to_be_bytes());
        Ok(())
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.0.clone().finalize().into()
    }
}

pub(crate) fn prefix_digest(
    events: &[EventEnvelope],
    source_sequence: u64,
) -> Result<[u8; 32], ReplayError> {
    let mut hasher = PrefixHasher::new();
    let mut covered = false;
    for envelope in events {
        if envelope.sequence > source_sequence {
            break;
        }
        hasher.update(envelope)?;
        covered = envelope.sequence == source_sequence;
    }
    if !covered {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(hasher.digest())
}
