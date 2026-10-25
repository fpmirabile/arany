use crate::{
    store::{AGGREGATE_BYTES, MAX_ARCHIVE_BYTES},
    tools::{
        FRAME_BYTES,
        permissions::MAX_CONFIG_BYTES,
        toolchains::MAX_RUNTIME_GRANTS,
        types::{
            COMMAND_PATH_METADATA_BYTES, COMMAND_SCAN_ENTRIES, COMMAND_SNAPSHOT_BYTES,
            COMMAND_SNAPSHOT_DEPTH, COMMAND_SNAPSHOT_FILES, DEFAULT_COMMAND_TIMEOUT_MS,
            MAX_BUILD_CACHE_BYTES, MAX_BUILD_MEMORY_BYTES, MAX_BUILD_PROCESSES_OR_THREADS,
            MAX_BUILD_PROJECT_BYTES, MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES,
            MAX_COMMAND_TIMEOUT_MS, MAX_FILE_BYTES, MAX_ORDINARY_TOOL_ARGUMENT_BYTES,
            MAX_PROCESSES_OR_THREADS, MAX_READ_LINE_CHARS, MAX_READ_PAGE_LINES, MAX_RUNTIME_BYTES,
            MAX_SKILLS, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES, MAX_TOOL_ARGUMENT_BYTES,
            MAX_TOOL_CATALOG_BYTES, MAX_TOOL_RESULT_BYTES, MCP_TIMEOUT_MS, MEMORY_BYTES,
            SCRATCH_BYTES,
        },
    },
};

pub struct Limit {
    pub name: &'static str,
    pub value: u64,
    pub unit: &'static str,
    pub owner: &'static str,
}

pub struct LimitsTable {
    pub rows: &'static [Limit],
}

pub const LIMITS: LimitsTable = LimitsTable {
    rows: &[
        Limit {
            name: "MAX_TOOL_ARGUMENT_BYTES",
            value: MAX_TOOL_ARGUMENT_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_ORDINARY_TOOL_ARGUMENT_BYTES",
            value: MAX_ORDINARY_TOOL_ARGUMENT_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES",
            value: MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_TOOL_RESULT_BYTES",
            value: MAX_TOOL_RESULT_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_READ_PAGE_LINES",
            value: MAX_READ_PAGE_LINES as u64,
            unit: "lines",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_READ_LINE_CHARS",
            value: MAX_READ_LINE_CHARS as u64,
            unit: "characters",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_CONFIG_BYTES",
            value: MAX_CONFIG_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/permissions.rs",
        },
        Limit {
            name: "MAX_SKILLS",
            value: MAX_SKILLS as u64,
            unit: "Skills",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_TOOL_CATALOG_BYTES",
            value: MAX_TOOL_CATALOG_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_FILE_BYTES",
            value: MAX_FILE_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_SNAPSHOT_BYTES",
            value: MAX_SNAPSHOT_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_SNAPSHOT_FILES",
            value: MAX_SNAPSHOT_FILES as u64,
            unit: "files",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "COMMAND_SNAPSHOT_BYTES",
            value: COMMAND_SNAPSHOT_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "COMMAND_SNAPSHOT_FILES",
            value: COMMAND_SNAPSHOT_FILES as u64,
            unit: "files",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "COMMAND_SCAN_ENTRIES",
            value: COMMAND_SCAN_ENTRIES as u64,
            unit: "entries",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "COMMAND_SNAPSHOT_DEPTH",
            value: COMMAND_SNAPSHOT_DEPTH as u64,
            unit: "levels",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "COMMAND_PATH_METADATA_BYTES",
            value: COMMAND_PATH_METADATA_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_RUNTIME_BYTES",
            value: MAX_RUNTIME_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_RUNTIME_GRANTS",
            value: MAX_RUNTIME_GRANTS as u64,
            unit: "roots",
            owner: "crates/arany/src/tools/toolchains.rs",
        },
        Limit {
            name: "SCRATCH_BYTES",
            value: SCRATCH_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MEMORY_BYTES",
            value: MEMORY_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_PROCESSES_OR_THREADS",
            value: MAX_PROCESSES_OR_THREADS as u64,
            unit: "processes or threads",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_BUILD_MEMORY_BYTES",
            value: MAX_BUILD_MEMORY_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_BUILD_PROJECT_BYTES",
            value: MAX_BUILD_PROJECT_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_BUILD_CACHE_BYTES",
            value: MAX_BUILD_CACHE_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_ARCHIVE_BYTES",
            value: MAX_ARCHIVE_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/store/state/build_cache.rs",
        },
        Limit {
            name: "AGGREGATE_BYTES",
            value: AGGREGATE_BYTES,
            unit: "bytes",
            owner: "crates/arany/src/store/state/build_cache.rs",
        },
        Limit {
            name: "MAX_BUILD_PROCESSES_OR_THREADS",
            value: MAX_BUILD_PROCESSES_OR_THREADS as u64,
            unit: "processes or threads",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "MAX_COMMAND_TIMEOUT_MS",
            value: MAX_COMMAND_TIMEOUT_MS as u64,
            unit: "ms",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "DEFAULT_COMMAND_TIMEOUT_MS",
            value: DEFAULT_COMMAND_TIMEOUT_MS as u64,
            unit: "ms",
            owner: "crates/arany/src/tools/types.rs",
        },
        Limit {
            name: "FRAME_BYTES",
            value: FRAME_BYTES as u64,
            unit: "bytes",
            owner: "crates/arany/src/tools/mcp.rs",
        },
        Limit {
            name: "MCP_TIMEOUT_MS",
            value: MCP_TIMEOUT_MS as u64,
            unit: "ms",
            owner: "crates/arany/src/tools/types.rs",
        },
    ],
};
