use crate::store::{RecordKind, StateRoot, StoreError};
use serde_json::Value;

#[cfg(test)]
mod tests;

const MAX_DEPTH: usize = 32;

pub struct RecordStep {
    pub from: u64,
    // Returns the record at `from + 1`, or `None` when the old record cannot prove a field it needs.
    pub transform: fn(Value) -> Option<Value>,
}

pub struct RecordFormat {
    pub kind: RecordKind,
    pub field: &'static str,
    // Records below the floor are refused unchanged, never upgraded.
    pub floor: u64,
    pub current: u64,
    pub steps: &'static [RecordStep],
    // The owner's current decoder; an upgrade it refuses is never written.
    pub accepts: fn(&[u8]) -> bool,
}

// Formats of the records the library owns; the CLI owns the others.
pub const RECORD_FORMATS: [&RecordFormat; 4] = [
    &crate::tools::config::TOOLS_CONFIG,
    &crate::tools::WORKSPACE_PERMISSIONS,
    &crate::tools::unguarded_rules::UNGUARDED_COMMANDS,
    &crate::provider::PROVIDER_PROFILES,
];

impl RecordFormat {
    pub fn accepts_current(&self, record: &[u8]) -> bool {
        (self.accepts)(record)
    }

    pub fn step(&self, from: u64) -> Option<&RecordStep> {
        self.steps.iter().find(|step| step.from == from)
    }

    // `None` keeps the record as it is: current, unsupported, malformed or refused.
    pub fn upgrade(&self, record: &[u8]) -> Option<Vec<u8>> {
        let mut value = self.behind(record)?;
        for from in self.version(&value)?..self.current {
            value = (self.step(from)?.transform)(value)?;
            if self.version(&value) != Some(from + 1) {
                return None;
            }
        }
        let upgraded = serde_json::to_vec(&value).ok()?;
        (upgraded.len() <= self.kind.limit() && (self.accepts)(&upgraded)).then_some(upgraded)
    }

    fn behind(&self, record: &[u8]) -> Option<Value> {
        if self.floor == self.current {
            return None;
        }
        let limit = self.kind.limit();
        // The bounded parser also refuses duplicate keys, so a transform never repairs them away.
        let value = crate::tools::parse_json_bounded(record, limit, limit, MAX_DEPTH).ok()?;
        (self.floor..self.current)
            .contains(&self.version(&value)?)
            .then_some(value)
    }

    fn version(&self, value: &Value) -> Option<u64> {
        value.get(self.field)?.as_u64()
    }
}

impl StateRoot {
    pub fn read_current(&self, format: &RecordFormat) -> Result<Option<Vec<u8>>, StoreError> {
        let record = self.record(format.kind);
        match record.read()? {
            Some(bytes) if format.upgrade(&bytes).is_some() => {}
            bytes => return Ok(bytes),
        }
        self.with_record_upgrade_lock(|| {
            let Some(bytes) = record.read()? else {
                return Ok(None);
            };
            let Some(upgraded) = format.upgrade(&bytes) else {
                return Ok(Some(bytes));
            };
            record.replace_upgraded(&upgraded)?;
            Ok(Some(upgraded))
        })
    }
}
