use crate::store::RecordKind;

const MAX_DEPTH: usize = 32;

pub struct RecordFormat {
    pub kind: RecordKind,
    pub field: &'static str,
    pub current: u64,
    // The owner's current decoder; a record it refuses at the current version is damaged, not archived.
    pub accepts: fn(&[u8]) -> bool,
}

// Formats of the records the library owns; the CLI owns the others.
pub const RECORD_FORMATS: [&RecordFormat; 7] = [
    &crate::tools::config::TOOLS_CONFIG,
    &crate::tools::WORKSPACE_PERMISSIONS,
    &crate::tools::unguarded_rules::UNGUARDED_COMMANDS,
    &crate::tools::command_environment::COMMAND_ENVIRONMENT,
    &crate::tools::command_tools::COMMAND_TOOLS,
    &crate::provider::PROVIDER_PROFILES,
    &crate::tools::MCP_OAUTH,
];

impl RecordFormat {
    pub fn accepts_current(&self, record: &[u8]) -> bool {
        (self.accepts)(record)
    }

    // Only a readable version other than the current one; a malformed record stays with its owner.
    pub fn mismatched(&self, record: &[u8]) -> bool {
        let limit = self.kind.limit();
        // The bounded parser also refuses duplicate keys.
        crate::tools::parse_json_bounded(record, limit, limit, MAX_DEPTH)
            .ok()
            .and_then(|value| value.get(self.field)?.as_u64())
            .is_some_and(|version| version != self.current)
    }
}
