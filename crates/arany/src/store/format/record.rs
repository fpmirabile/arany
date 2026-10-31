use crate::store::RecordKind;

// A strict record: its owner's decoder refuses unknown or missing fields, and a refused record is archived intact.
pub struct RecordFormat {
    pub kind: RecordKind,
    pub accepts: fn(&[u8]) -> bool,
}

// Strict records the library owns; the CLI owns the others.
pub const RECORD_FORMATS: [&RecordFormat; 9] = [
    &crate::tools::config::TOOLS_CONFIG,
    &crate::tools::WORKSPACE_PERMISSIONS,
    &crate::tools::permissions::PERMISSION_SETTINGS,
    &crate::tools::unguarded_rules::UNGUARDED_COMMANDS,
    &crate::tools::command_environment::COMMAND_ENVIRONMENT,
    &crate::tools::command_tools::COMMAND_TOOLS,
    &crate::provider::PROVIDER_PROFILES,
    &crate::tools::MCP_OAUTH,
    &crate::tools::guard_settings::GUARD_SETTINGS,
];
