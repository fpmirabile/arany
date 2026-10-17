use super::{EventEnvelope, ReplayError};
use sha2::{Digest, Sha256};

pub(crate) struct PrefixHasher(Sha256);

impl PrefixHasher {
    pub(crate) fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"arany-session-prefix-v1\0");
        Self(hasher)
    }

    pub(crate) fn update(&mut self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        self.0.update(envelope.sequence.to_be_bytes());
        self.0.update(envelope.session_id.to_string().as_bytes());
        for scope in [
            envelope.run_id.map(|id| id.to_string()),
            envelope.agent_run_id.map(|id| id.to_string()),
        ] {
            match scope {
                Some(id) => {
                    self.0.update([1]);
                    self.0.update(id.as_bytes());
                }
                None => self.0.update([0]),
            }
        }
        let kind = envelope.event.kind().as_bytes();
        self.0.update(
            u32::try_from(kind.len())
                .map_err(|_| ReplayError::InvalidPayload)?
                .to_be_bytes(),
        );
        self.0.update(kind);
        let payload = envelope.event.payload()?;
        self.0.update(
            u32::try_from(payload.len())
                .map_err(|_| ReplayError::InvalidPayload)?
                .to_be_bytes(),
        );
        self.0.update(payload.as_bytes());
        self.0.update(envelope.created_at_ms.to_be_bytes());
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
