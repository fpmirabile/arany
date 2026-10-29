use super::{
    Admission, DefaultsError, LoopError, load_view, open_state, recover_defaults_change,
    update_defaults,
};
use crate::accounts::chatgpt;
use crate::cli::provider::select::ProviderArg;
use crate::presentation::{format_bytes, format_duration};
use crate::terminal::{
    AttachedTerminal, CommandAvailability, CommandParseError, CompletionLayout, Composer,
    InteractiveCommand, NewlineShortcut, PointerSettings, Submission, TerminalInput, TerminalMode,
    transcript_agents,
};
use arany::{BuildCacheSize, RecordFormat, RecordKind};
use arany::{
    CollaborationPolicy, CustomProfile, Effort, ProviderKind, SessionId, SessionView, StateRoot,
    fork_session, rename_session, resume_session, validate_native_model_id,
};
use serde_json::Value;
use std::str::FromStr;

const CACHE_USAGE: &str = "Usage: /cache clear [all]";
const CACHE_BUSY: &str = "Build cache is in use; retry after its command finishes";
const CACHE_CLEAR_FAILED: &str = "Error: private build cache could not be cleared";
const BUILD_CACHE_LABEL: &str = "Build cache";
mod build_cpu;
mod build_jobs;

const UI_PREFERENCES_VERSION: u8 = 9;
const SIZE_UNITS: &[(&str, u64)] = &[
    ("B", 1),
    ("KB", 1024),
    ("MB", 1024_u64.pow(2)),
    ("GB", 1024_u64.pow(3)),
    ("TB", 1024_u64.pow(4)),
];

pub(crate) const UI_PREFERENCES: RecordFormat = RecordFormat {
    kind: RecordKind::UiPreferences,
    field: "version",
    current: UI_PREFERENCES_VERSION as u64,
    accepts: |bytes| decode_settings(bytes).is_ok(),
};

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UiPreferences {
    version: u8,
    // Only keys that differ from the binary's defaults; every other key follows them at read time.
    preferences: serde_json::Map<String, Value>,
}

// Read first so an older record is refused by version rather than by its missing fields.
#[derive(serde::Deserialize)]
struct RecordVersion {
    version: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preferences {
    pub completion: CompletionLayout,
    pub view: TerminalMode,
    pub goal_budget: arany::GoalBudget,
    pub resources: arany::ToolResourceSettings,
    pub copy_on_select: bool,
    pub newline: NewlineShortcut,
    pub context_window: arany::ContextWindow,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            completion: CompletionLayout::default(),
            view: TerminalMode::default(),
            goal_budget: arany::GoalBudget::default(),
            resources: arany::ToolResourceSettings {
                build_jobs: None,
                build_cpu_cores: None,
                ..Default::default()
            },
            copy_on_select: PointerSettings::default().copy_on_select,
            newline: NewlineShortcut::default(),
            context_window: arany::ContextWindow::default(),
        }
    }
}

impl Preferences {
    pub(super) fn pointer(self) -> PointerSettings {
        PointerSettings {
            copy_on_select: self.copy_on_select,
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingInput {
    Choice,
    Typed,
}

struct Setting {
    label: &'static str,
    hint: &'static str,
    input: SettingInput,
    value: fn(&Preferences) -> String,
    apply: fn(&mut Preferences, &str) -> Option<()>,
    cycle: fn(&mut Preferences, bool),
    reset: fn(&mut Preferences, &Preferences),
    guidance: Option<fn(&AttachedTerminal) -> String>,
}

const SETTINGS: &[Setting] = &[
    Setting {
        label: "Slash menu",
        hint: "tabs|combined",
        input: SettingInput::Choice,
        value: |p| p.completion.label().into(),
        apply: |p, raw| {
            p.completion = [CompletionLayout::Tabs, CompletionLayout::Combined]
                .into_iter()
                .find(|value| value.label() == raw)?;
            Some(())
        },
        cycle: |p, _| {
            p.completion = p.completion.toggled();
        },
        reset: |p, default| p.completion = default.completion,
        guidance: None,
    },
    Setting {
        label: "Chat view",
        hint: "fullscreen|classic",
        input: SettingInput::Choice,
        value: |p| p.view.label().into(),
        apply: |p, raw| {
            p.view = [TerminalMode::Fullscreen, TerminalMode::Classic]
                .into_iter()
                .find(|value| value.label() == raw)?;
            Some(())
        },
        cycle: |p, _| {
            p.view = p.view.toggled();
        },
        reset: |p, default| p.view = default.view,
        guidance: None,
    },
    Setting {
        label: "Provider calls per task",
        hint: "number or unlimited",
        input: SettingInput::Typed,
        value: |p| {
            show_limit(p.goal_budget.max_provider_calls.map(u64::from), |value| {
                value.to_string()
            })
        },
        apply: |p, raw| {
            p.goal_budget.max_provider_calls = parse_limit(raw, |value| value.parse().ok())?
                .map(u32::try_from)
                .transpose()
                .ok()?;
            Some(())
        },
        cycle: |p, forward| {
            p.goal_budget.max_provider_calls = step(
                p.goal_budget.max_provider_calls,
                &[None, Some(256), Some(64), Some(16)],
                forward,
            );
        },
        reset: |p, default| {
            p.goal_budget.max_provider_calls = default.goal_budget.max_provider_calls
        },
        guidance: None,
    },
    Setting {
        label: "Tokens per task",
        hint: "number or unlimited",
        input: SettingInput::Typed,
        value: |p| show_limit(p.goal_budget.max_tokens, |value| value.to_string()),
        apply: |p, raw| {
            p.goal_budget.max_tokens = parse_limit(raw, |value| value.parse().ok())?;
            Some(())
        },
        cycle: |p, forward| {
            p.goal_budget.max_tokens = step(
                p.goal_budget.max_tokens,
                &[None, Some(1_000_000), Some(100_000)],
                forward,
            );
        },
        reset: |p, default| p.goal_budget.max_tokens = default.goal_budget.max_tokens,
        guidance: None,
    },
    Setting {
        label: "Active time per task",
        hint: "duration or unlimited",
        input: SettingInput::Typed,
        value: |p| show_limit(p.goal_budget.max_active_ms, format_duration),
        apply: |p, raw| {
            p.goal_budget.max_active_ms = parse_limit(raw, parse_duration)?;
            Some(())
        },
        cycle: |p, forward| {
            p.goal_budget.max_active_ms = step(
                p.goal_budget.max_active_ms,
                &[
                    None,
                    Some(arany::MAX_GOAL_ACTIVE_MS),
                    Some(60 * 60 * 1000),
                    Some(15 * 60 * 1000),
                ],
                forward,
            );
        },
        reset: |p, default| p.goal_budget.max_active_ms = default.goal_budget.max_active_ms,
        guidance: None,
    },
    Setting {
        label: "Default command timeout",
        hint: "duration, e.g. 30s",
        input: SettingInput::Typed,
        value: |p| format_duration(u64::from(p.resources.default_command_timeout_ms)),
        apply: |p, raw| {
            p.resources.default_command_timeout_ms = parse_duration(raw)?.try_into().ok()?;
            Some(())
        },
        cycle: |p, forward| {
            p.resources.default_command_timeout_ms = step(
                p.resources.default_command_timeout_ms,
                &[120_000, 60_000, 30_000],
                forward,
            );
        },
        reset: |p, default| {
            p.resources.default_command_timeout_ms = default.resources.default_command_timeout_ms;
        },
        guidance: None,
    },
    Setting {
        label: "Maximum command timeout",
        hint: "duration, e.g. 5m",
        input: SettingInput::Typed,
        value: |p| format_duration(u64::from(p.resources.max_command_timeout_ms)),
        apply: |p, raw| {
            p.resources.max_command_timeout_ms = parse_duration(raw)?.try_into().ok()?;
            Some(())
        },
        cycle: |p, forward| {
            p.resources.max_command_timeout_ms = step(
                p.resources.max_command_timeout_ms,
                &[600_000, 300_000, 120_000],
                forward,
            );
        },
        reset: |p, default| {
            p.resources.max_command_timeout_ms = default.resources.max_command_timeout_ms;
        },
        guidance: None,
    },
    Setting {
        label: "Build memory",
        hint: "size, e.g. 512MB",
        input: SettingInput::Typed,
        value: |p| format_setting_size(p.resources.build_memory_bytes),
        apply: |p, raw| {
            p.resources.build_memory_bytes = parse_size(raw)?;
            Some(())
        },
        cycle: |p, forward| {
            p.resources.build_memory_bytes = step(
                p.resources.build_memory_bytes,
                &[
                    4 * 1024_u64.pow(3),
                    2 * 1024_u64.pow(3),
                    1024_u64.pow(3),
                    512 * 1024 * 1024,
                ],
                forward,
            );
        },
        reset: |p, default| p.resources.build_memory_bytes = default.resources.build_memory_bytes,
        guidance: None,
    },
    Setting {
        label: "Build processes/threads",
        hint: "number",
        input: SettingInput::Typed,
        value: |p| p.resources.build_processes.to_string(),
        apply: |p, raw| {
            p.resources.build_processes = raw.parse().ok()?;
            Some(())
        },
        cycle: |p, forward| {
            p.resources.build_processes =
                step(p.resources.build_processes, &[256, 128, 64, 32], forward);
        },
        reset: |p, default| p.resources.build_processes = default.resources.build_processes,
        guidance: None,
    },
    Setting {
        label: "Build writable project",
        hint: "size, e.g. 1GB",
        input: SettingInput::Typed,
        value: |p| format_setting_size(p.resources.build_project_bytes),
        apply: |p, raw| {
            p.resources.build_project_bytes = parse_size(raw)?;
            Some(())
        },
        cycle: |p, forward| {
            p.resources.build_project_bytes = step(
                p.resources.build_project_bytes,
                &[
                    1024_u64.pow(3),
                    512 * 1024 * 1024,
                    256 * 1024 * 1024,
                    64 * 1024 * 1024,
                ],
                forward,
            );
        },
        reset: |p, default| p.resources.build_project_bytes = default.resources.build_project_bytes,
        guidance: None,
    },
    Setting {
        label: BUILD_CACHE_LABEL,
        hint: "size or unlimited, e.g. 8GB",
        input: SettingInput::Typed,
        value: |p| match p.resources.build_cache_bytes {
            BuildCacheSize::Unlimited => "unlimited".into(),
            BuildCacheSize::Bytes(bytes) => format_setting_size(bytes),
        },
        apply: |p, raw| {
            p.resources.build_cache_bytes = match parse_limit(raw, parse_size)? {
                None => BuildCacheSize::Unlimited,
                Some(bytes) => BuildCacheSize::Bytes(bytes),
            };
            Some(())
        },
        cycle: |p, forward| {
            p.resources.build_cache_bytes = step(
                p.resources.build_cache_bytes,
                &[
                    BuildCacheSize::Unlimited,
                    BuildCacheSize::Bytes(8 * 1024_u64.pow(3)),
                    BuildCacheSize::Bytes(4 * 1024_u64.pow(3)),
                    BuildCacheSize::Bytes(1024_u64.pow(3)),
                    BuildCacheSize::DISABLED,
                ],
                forward,
            );
        },
        reset: |p, default| p.resources.build_cache_bytes = default.resources.build_cache_bytes,
        guidance: None,
    },
    Setting {
        label: "Copy on select",
        hint: "on|off",
        input: SettingInput::Choice,
        value: |p| on_off(p.copy_on_select).into(),
        apply: |p, raw| {
            p.copy_on_select = [true, false]
                .into_iter()
                .find(|value| on_off(*value) == raw)?;
            Some(())
        },
        cycle: |p, _| {
            p.copy_on_select = !p.copy_on_select;
        },
        reset: |p, default| p.copy_on_select = default.copy_on_select,
        guidance: None,
    },
    Setting {
        label: "Newline shortcut",
        hint: "shift+enter|alt+enter|ctrl+enter",
        input: SettingInput::Choice,
        value: |p| p.newline.label().into(),
        apply: |p, raw| {
            p.newline = NewlineShortcut::ALL
                .into_iter()
                .find(|value| value.label().eq_ignore_ascii_case(raw))?;
            Some(())
        },
        cycle: |p, forward| {
            p.newline = step(p.newline, &NewlineShortcut::ALL, forward);
        },
        reset: |p, default| p.newline = default.newline,
        guidance: Some(AttachedTerminal::newline_shortcut_guidance),
    },
    build_jobs::SETTING,
    build_cpu::SETTING,
    Setting {
        label: "Context window",
        hint: "percent or tokens, e.g. 80% or 500k",
        input: SettingInput::Typed,
        value: |p| p.context_window.to_string(),
        apply: |p, raw| {
            p.context_window = raw.parse().ok()?;
            Some(())
        },
        cycle: |p, forward| {
            p.context_window = step(p.context_window, &CONTEXT_WINDOW_PRESETS, forward);
        },
        reset: |p, default| p.context_window = default.context_window,
        guidance: None,
    },
];

const CONTEXT_WINDOW_PRESETS: [arany::ContextWindow; 7] = {
    use arany::ContextWindow::{Percent, Tokens};
    [
        Percent(100),
        Percent(75),
        Percent(50),
        Tokens(1_000_000),
        Tokens(500_000),
        Tokens(256_000),
        Tokens(128_000),
    ]
};
const MAX_SETTING_VALUE_BYTES: usize = 64;

fn fits_model_window(window: arany::ContextWindow, model_window: Option<u32>) -> bool {
    match window {
        arany::ContextWindow::Percent(_) => true,
        arany::ContextWindow::Tokens(tokens) => model_window.is_none_or(|limit| tokens <= limit),
    }
}

fn show_limit(value: Option<u64>, format: impl FnOnce(u64) -> String) -> String {
    value.map_or_else(|| "unlimited".into(), format)
}

fn parse_limit(raw: &str, parse: fn(&str) -> Option<u64>) -> Option<Option<u64>> {
    if raw.eq_ignore_ascii_case("unlimited") {
        Some(None)
    } else {
        parse(raw).map(Some)
    }
}

fn parse_quantity<'a>(raw: &'a str, units: &[(&str, u64)]) -> Option<(u64, u64, &'a str)> {
    let raw = raw.trim_start();
    let end = raw.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (number, rest) = raw.split_at(end);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
        || number.ends_with('.')
    {
        return None;
    }
    let rest = rest.trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (unit, rest) = rest.split_at(end);
    let scale = units
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(unit))?
        .1;
    let denominator = 10_u128.checked_pow(fraction.len().try_into().ok()?)?;
    let whole = whole.parse::<u64>().ok()?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    let mut common = u128::from(scale);
    let mut remaining = denominator;
    while remaining != 0 {
        (common, remaining) = (remaining, common % remaining);
    }
    let scaled_fraction = fraction.checked_mul(u128::from(scale) / common)?;
    let denominator = denominator / common;
    if scaled_fraction % denominator != 0 {
        return None;
    }
    let value = whole
        .checked_mul(scale)?
        .checked_add((scaled_fraction / denominator).try_into().ok()?)?;
    Some((value, scale, rest.trim_start()))
}

fn parse_duration(mut raw: &str) -> Option<u64> {
    let mut total = 0_u64;
    let mut previous_scale = u64::MAX;
    loop {
        let (value, scale, rest) = parse_quantity(
            raw,
            &[
                ("s", 1000),
                ("m", 60_000),
                ("min", 60_000),
                ("h", 3_600_000),
            ],
        )?;
        if scale >= previous_scale {
            return None;
        }
        total = total.checked_add(value)?;
        if rest.is_empty() {
            return Some(total);
        }
        previous_scale = scale;
        raw = rest;
    }
}

fn parse_size(raw: &str) -> Option<u64> {
    let (value, _, rest) = parse_quantity(raw, SIZE_UNITS)?;
    rest.is_empty().then_some(value)
}

fn format_setting_size(bytes: u64) -> String {
    let display = format_bytes(bytes);
    if parse_size(&display) == Some(bytes) {
        return display;
    }
    let (unit, scale) = SIZE_UNITS
        .iter()
        .rev()
        .find(|(_, scale)| bytes >= *scale)
        .copied()
        .unwrap_or(("B", 1));
    let mut exact = (bytes / scale).to_string();
    let mut remainder = bytes % scale;
    if remainder != 0 {
        exact.push('.');
        while remainder != 0 {
            remainder *= 10;
            exact.push(char::from(b'0' + (remainder / scale) as u8));
            remainder %= scale;
        }
    }
    format!("{exact} {unit}")
}

// Linear rows take `NUMBER VALUE`, numbered by the same table that renders them.
fn apply_setting(preferences: &mut Preferences, value: &str) -> bool {
    let Some((number, raw)) = value.split_once(' ') else {
        return false;
    };
    number
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .is_some_and(|row| apply_row(preferences, row, raw))
}

// Linear `NUMBER VALUE` and the visual value field share this parser.
fn apply_row(preferences: &mut Preferences, row: usize, raw: &str) -> bool {
    let Some(setting) = SETTINGS.get(row) else {
        return false;
    };
    let mut chosen = *preferences;
    let raw = raw.trim();
    if raw == "default" {
        (setting.reset)(&mut chosen, &Preferences::default());
    } else if (setting.apply)(&mut chosen, raw).is_none() {
        return false;
    }
    if chosen.goal_budget.validate().is_err() || !chosen.resources.valid() {
        return false;
    }
    *preferences = chosen;
    true
}

fn setting_value(setting: &Setting, preferences: &Preferences) -> String {
    let mut reset = *preferences;
    (setting.reset)(&mut reset, &Preferences::default());
    let value = (setting.value)(preferences);
    if reset == *preferences {
        format!("{value} (default)")
    } else {
        value
    }
}

// `cache_used` is the private build caches' current size, shown beside the Build cache row.
fn setting_value_used(
    setting: &Setting,
    preferences: &Preferences,
    cache_used: Option<u64>,
) -> String {
    let value = setting_value(setting, preferences);
    match cache_used {
        Some(bytes) if setting.label == BUILD_CACHE_LABEL => {
            format!("{value} · {} used", format_bytes(bytes))
        }
        _ => value,
    }
}

fn setting_rows(preferences: Preferences, cache_used: Option<u64>) -> Vec<String> {
    SETTINGS
        .iter()
        .map(|setting| {
            format!(
                "{}: {}",
                setting.label,
                setting_value_used(setting, &preferences, cache_used)
            )
        })
        .collect()
}

fn linear_setting_rows(preferences: Preferences, cache_used: Option<u64>) -> Vec<String> {
    SETTINGS
        .iter()
        .map(|setting| {
            format!(
                "{}: {} ({})",
                setting.label,
                setting_value_used(setting, &preferences, cache_used),
                setting.hint
            )
        })
        .collect()
}

fn step<T: Copy + Eq>(current: T, values: &[T], forward: bool) -> T {
    let index = values
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0);
    values[if forward {
        (index + 1) % values.len()
    } else {
        (index + values.len() - 1) % values.len()
    }]
}

fn reset_setting(preferences: &mut Preferences, row: usize) {
    if let Some(setting) = SETTINGS.get(row) {
        (setting.reset)(preferences, &Preferences::default());
    }
}

fn cycle_setting(
    preferences: &mut Preferences,
    row: usize,
    forward: bool,
    model_window: Option<u32>,
) {
    if let Some(setting) = SETTINGS.get(row) {
        let mut chosen = *preferences;
        (setting.cycle)(&mut chosen, forward);
        // Token presets larger than the known model window are skipped; percentages always fit.
        for _ in 0..CONTEXT_WINDOW_PRESETS.len() {
            if chosen.context_window == preferences.context_window
                || fits_model_window(chosen.context_window, model_window)
            {
                break;
            }
            (setting.cycle)(&mut chosen, forward);
        }
        if chosen.goal_budget.validate().is_ok() && chosen.resources.valid() {
            *preferences = chosen;
        }
    }
}

pub(super) async fn restore_settings(admission: &Admission) -> Result<Preferences, String> {
    restore_settings_at(&admission.state_dir).await
}

pub(super) async fn restore_settings_at(
    state_dir: &std::path::Path,
) -> Result<Preferences, String> {
    Ok(saved_settings(state_dir).await??)
}

pub(in crate::cli) async fn execution_limits(
    state_dir: &std::path::Path,
) -> Result<
    (
        arany::GoalBudget,
        arany::ToolResourceSettings,
        arany::ContextWindow,
    ),
    String,
> {
    let settings = restore_settings_at(state_dir).await?;
    Ok((
        settings.goal_budget,
        settings.resources,
        settings.context_window,
    ))
}

// An unsupported or invalid record leaves defaults in force until /settings replaces it.
pub(super) async fn settings_or_defaults(admission: &Admission) -> Result<Preferences, String> {
    Ok(saved_settings(&admission.state_dir)
        .await?
        .unwrap_or_default())
}

async fn saved_settings(
    state_dir: &std::path::Path,
) -> Result<Result<Preferences, &'static str>, String> {
    let path = state_dir.to_owned();
    tokio::task::spawn_blocking(move || match StateRoot::open_optional(&path) {
        Ok(Some(root)) => read_settings(&root),
        Ok(None) => Ok(Ok(Preferences::default())),
        Err(_) => Err("display settings unavailable or unsafe".into()),
    })
    .await
    .map_err(|_| "display settings unavailable".to_owned())?
}

fn read_settings(root: &StateRoot) -> Result<Result<Preferences, &'static str>, String> {
    let Some(record) = root
        .record(UI_PREFERENCES.kind)
        .read()
        .map_err(|_| "display settings unavailable or unsafe")?
    else {
        return Ok(Ok(Preferences::default()));
    };
    Ok(decode_settings(&record))
}

fn decode_settings(record: &[u8]) -> Result<Preferences, &'static str> {
    let invalid = "invalid display settings; use /settings to replace them";
    let unsupported = "unsupported settings; use /settings to replace them";
    let version: RecordVersion = serde_json::from_slice(record).map_err(|_| invalid)?;
    if version.version != UI_PREFERENCES_VERSION {
        return Err(unsupported);
    }
    let record: UiPreferences = serde_json::from_slice(record).map_err(|_| invalid)?;
    let mut resolved = serde_json::to_value(Preferences::default()).map_err(|_| invalid)?;
    overlay(&mut resolved, Value::Object(record.preferences)).ok_or(invalid)?;
    let preferences: Preferences = serde_json::from_value(resolved).map_err(|_| invalid)?;
    if preferences.goal_budget.validate().is_err() || !preferences.resources.valid() {
        return Err(unsupported);
    }
    Ok(preferences)
}

// Saved keys replace defaults; a key the defaults lack is refused.
fn overlay(resolved: &mut Value, saved: Value) -> Option<()> {
    match (resolved, saved) {
        (Value::Object(resolved), Value::Object(saved)) => {
            for (key, value) in saved {
                overlay(resolved.get_mut(&key)?, value)?;
            }
            Some(())
        }
        (resolved, saved) => {
            *resolved = saved;
            Some(())
        }
    }
}

// The keys of `chosen` that differ from `default`, so unchanged keys keep following the binary.
fn changed(chosen: Value, default: &Value) -> Option<Value> {
    let (Value::Object(chosen), Value::Object(default)) = (&chosen, default) else {
        return (chosen != *default).then_some(chosen);
    };
    let fields: serde_json::Map<String, Value> = chosen
        .iter()
        .filter_map(|(key, value)| {
            let changed = changed(value.clone(), default.get(key).unwrap_or(&Value::Null))?;
            Some((key.clone(), changed))
        })
        .collect();
    (!fields.is_empty()).then_some(Value::Object(fields))
}

fn encode_settings(preferences: Preferences) -> Option<Vec<u8>> {
    let default = serde_json::to_value(Preferences::default()).ok()?;
    let preferences = match changed(serde_json::to_value(preferences).ok()?, &default) {
        Some(Value::Object(fields)) => fields,
        _ => serde_json::Map::new(),
    };
    serde_json::to_vec(&UiPreferences {
        version: UI_PREFERENCES_VERSION,
        preferences,
    })
    .ok()
}

#[derive(Debug, Eq, PartialEq)]
enum SaveError {
    Unconfirmed,
    Failed(&'static str),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unconfirmed => "settings change not confirmed; check /settings",
            Self::Failed(message) => message,
        })
    }
}

fn save_failure_notice(error: &SaveError) -> String {
    match error {
        SaveError::Unconfirmed => format!("Error: {error}"),
        SaveError::Failed(_) => format!("Error: {error}; current settings kept"),
    }
}

async fn save_settings(admission: &Admission, preferences: Preferences) -> Result<(), SaveError> {
    if preferences.goal_budget.validate().is_err() || !preferences.resources.valid() {
        return Err(SaveError::Failed("invalid settings"));
    }
    let path = admission.state_dir.clone();
    let workspace = admission.workspace.clone();
    tokio::task::spawn_blocking(move || {
        let root = StateRoot::admit(&path)
            .map_err(|_| SaveError::Failed("display settings unavailable or unsafe"))?;
        let record = encode_settings(preferences)
            .ok_or(SaveError::Failed("display settings unavailable"))?;
        root.with_account_replacement_lock(&workspace, || {
            root.record(RecordKind::UiPreferences).replace(&record)
        })
        .map_err(|_| SaveError::Failed("display settings busy or unsafe"))?
        .map_err(|error| match error {
            arany::StoreError::ReplaceUnconfirmed => SaveError::Unconfirmed,
            _ => SaveError::Failed("could not save display settings"),
        })
    })
    .await
    .map_err(|_| SaveError::Failed("display settings unavailable"))?
}

fn settings_notice(
    terminal: &AttachedTerminal,
    selected_row: usize,
    invalid: &mut bool,
    linear_guidance: &mut bool,
) -> Option<String> {
    let linear = terminal.is_linear();
    if std::mem::take(invalid) {
        Some(
            if linear {
                "Type NUMBER VALUE from the list; q cancels"
            } else {
                "The selected settings could not be saved"
            }
            .to_owned(),
        )
    } else if std::mem::take(linear_guidance) {
        Some("Modified newline shortcuts apply only to visual sessions; use backslash then Enter here".to_owned())
    } else {
        SETTINGS
            .get(selected_row)
            .and_then(|setting| setting.guidance)
            .map(|guidance| guidance(terminal))
    }
}

// The last Run on the selected model tells which token presets fit; without one all are offered.
fn model_window(view: &SessionView) -> Option<u32> {
    view.runs
        .iter()
        .rev()
        .filter_map(|run| run.config.as_ref())
        .find(|config| view.defaults.model.as_ref() == Some(&config.model))
        .and_then(|config| config.context_window_tokens)
}

enum FieldKey {
    Edited,
    Cancel,
    Submit,
    Pass,
}

fn field_key(text: &mut String, input: TerminalInput) -> FieldKey {
    match input {
        TerminalInput::Character(character) => {
            if text.len() + character.len_utf8() <= MAX_SETTING_VALUE_BYTES {
                text.push(character);
            }
            FieldKey::Edited
        }
        TerminalInput::Backspace | TerminalInput::Delete => {
            text.pop();
            FieldKey::Edited
        }
        TerminalInput::Escape => FieldKey::Cancel,
        TerminalInput::Submit => FieldKey::Submit,
        TerminalInput::Interrupt
        | TerminalInput::EndOfInput
        | TerminalInput::Suspend
        | TerminalInput::Shutdown(_) => FieldKey::Pass,
        _ => FieldKey::Edited,
    }
}

fn field_notice(setting: &Setting, invalid: &mut bool) -> String {
    let lead = if std::mem::take(invalid) {
        "Invalid value; type"
    } else {
        "Type"
    };
    format!(
        "{lead} {}; Enter applies, empty Enter saves, Esc returns to the list",
        setting.hint
    )
}

fn visual_rows(
    chosen: Preferences,
    cache_used: Option<u64>,
    selected_row: usize,
    field: Option<&str>,
) -> Vec<String> {
    let mut rows = setting_rows(chosen, cache_used);
    if let (Some(text), Some(row), Some(setting)) = (
        field,
        rows.get_mut(selected_row),
        SETTINGS.get(selected_row),
    ) {
        *row = format!("{}: {text}▏", setting.label);
    }
    rows
}

async fn choose_settings(
    terminal: &mut AttachedTerminal,
    mut chosen: Preferences,
    cache_used: Option<u64>,
    view: &SessionView,
) -> Result<Option<Preferences>, String> {
    let linear = terminal.is_linear();
    let model_window = model_window(view);
    let mut selected_row = 0usize;
    let mut answer = Composer::default();
    let mut field: Option<String> = None;
    let mut invalid = false;
    let mut linear_guidance = linear;
    terminal.begin_dialog();
    loop {
        let typed_row = SETTINGS[selected_row].input == SettingInput::Typed;
        let notice = match &field {
            Some(_) => Some(field_notice(&SETTINGS[selected_row], &mut invalid)),
            None if typed_row && !linear && !invalid => Some(format!(
                "Enter types a value: {}",
                SETTINGS[selected_row].hint
            )),
            None => settings_notice(terminal, selected_row, &mut invalid, &mut linear_guidance),
        };
        let rows = if linear {
            linear_setting_rows(chosen, cache_used)
        } else {
            visual_rows(chosen, cache_used, selected_row, field.as_deref())
        };
        terminal
            .draw_settings(&rows, selected_row, notice.as_deref(), view)
            .map_err(|error| error.to_string())?;
        if terminal
            .arm_dialog()
            .await
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        let input = terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(text) = field.as_mut() {
            match field_key(text, input) {
                FieldKey::Edited => continue,
                FieldKey::Cancel => {
                    field = None;
                    continue;
                }
                FieldKey::Submit if text.trim().is_empty() => return Ok(Some(chosen)),
                FieldKey::Submit => {
                    if apply_row(&mut chosen, selected_row, &text.to_ascii_lowercase()) {
                        field = None;
                    } else {
                        invalid = true;
                    }
                    continue;
                }
                FieldKey::Pass => {}
            }
        }
        match input {
            TerminalInput::Up if !linear => selected_row = selected_row.saturating_sub(1),
            TerminalInput::Down if !linear => selected_row = (selected_row + 1).min(rows.len() - 1),
            TerminalInput::Left | TerminalInput::Right if !linear => cycle_setting(
                &mut chosen,
                selected_row,
                input == TerminalInput::Right,
                model_window,
            ),
            TerminalInput::Backspace | TerminalInput::Delete if !linear => {
                reset_setting(&mut chosen, selected_row);
            }
            TerminalInput::Character(_) if !linear => {
                selected_row = input.choice_index(rows.len()).unwrap_or(selected_row);
            }
            TerminalInput::Character(character) => {
                answer.apply(TerminalInput::Character(character));
            }
            TerminalInput::Backspace if linear => {
                answer.apply(TerminalInput::Backspace);
            }
            TerminalInput::Submit if linear => {
                let value = answer.take().trim().to_ascii_lowercase();
                match value.as_str() {
                    "" => return Ok(Some(chosen)),
                    "q" => return Ok(None),
                    value => invalid = !apply_setting(&mut chosen, value),
                }
            }
            TerminalInput::Submit if typed_row => field = Some(String::new()),
            TerminalInput::Submit => return Ok(Some(chosen)),
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(None);
            }
            TerminalInput::Suspend => terminal
                .suspend_and_resume(answer.input_capacity())
                .map_err(|error| error.to_string())?,
            TerminalInput::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
            TerminalInput::LineRejected => {
                answer.clear();
                invalid = true;
            }
            _ => {}
        }
    }
}

pub(super) async fn settings(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    composer: &mut Composer,
    saved_view: &mut TerminalMode,
    view: &SessionView,
) -> Result<String, String> {
    let linear = terminal.is_linear();
    let mut current = settings_or_defaults(admission).await?;
    current.completion = composer.completion_layout();
    current.newline = composer.newline_shortcut();
    current.view = *saved_view;
    let cache_used = build_cache_used(admission).await;
    let notice = match choose_settings(terminal, current, cache_used, view).await? {
        None => "Settings unchanged".to_owned(),
        Some(chosen) => match save_settings(admission, chosen).await {
            Ok(()) => {
                let view_changed = chosen.view != *saved_view;
                *saved_view = chosen.view;
                composer.set_completion_layout(chosen.completion);
                composer.set_newline_shortcut(chosen.newline);
                terminal.set_newline_shortcut(chosen.newline);
                terminal.set_pointer(chosen.pointer());
                if !linear && view_changed {
                    terminal
                        .set_mode(chosen.view, composer.input_capacity())
                        .map_err(|error| error.to_string())?;
                }
                format!(
                    "Settings saved · calls {} · active {} · command max {}{}",
                    chosen
                        .goal_budget
                        .max_provider_calls
                        .map_or_else(|| "unlimited".into(), |value| value.to_string()),
                    chosen
                        .goal_budget
                        .max_active_ms
                        .map_or_else(|| "unlimited".into(), format_duration),
                    format_duration(u64::from(chosen.resources.max_command_timeout_ms)),
                    if linear {
                        " · visual sessions"
                    } else if chosen.view != terminal.mode() {
                        " · sessions without --classic"
                    } else {
                        ""
                    },
                )
            }
            Err(error) => save_failure_notice(&error),
        },
    };
    terminal
        .restore_draft_input(composer.input_capacity())
        .map_err(|error| error.to_string())?;
    Ok(notice)
}

fn parse_model_argument(argument: &str) -> Result<(&str, Option<Option<Effort>>), &'static str> {
    let mut parts = argument.split_ascii_whitespace();
    let model = parts.next().ok_or("Usage: /model MODEL [EFFORT|default]")?;
    if validate_native_model_id(model).is_err() {
        return Err("invalid native model");
    }
    let effort = match parts.next() {
        None => None,
        Some("default") => Some(None),
        Some(value) => Some(Some(
            Effort::from_str(value)
                .map_err(|_| "Effort unavailable for the selected Provider/model")?,
        )),
    };
    if parts.next().is_some() {
        return Err("Usage: /model MODEL [EFFORT|default]");
    }
    Ok((model, effort))
}

fn typed_model_selection<'a>(
    view: &SessionView,
    argument: &'a str,
) -> Result<(&'a str, Option<Effort>), &'static str> {
    let profile = view
        .defaults
        .provider
        .as_deref()
        .ok_or("Select /provider first")?;
    let provider = ProviderKind::parse(profile);
    let (model, chosen) = parse_model_argument(argument)?;
    let effort = chosen.unwrap_or_else(|| {
        if view.defaults.model.as_deref() == Some(model) {
            view.defaults.effort
        } else if provider == Some(ProviderKind::Custom) {
            None
        } else {
            Some(Effort::Low)
        }
    });
    match provider {
        Some(ProviderKind::ChatGpt) => {
            if effort.is_none() {
                return Err("ChatGPT requires an explicit effort");
            }
        }
        Some(ProviderKind::Custom) => {}
        Some(ProviderKind::OpenAi | ProviderKind::Anthropic) | None => {
            if arany::resolve_native_effort_for_run(profile, model, effort).is_err() {
                return Err("invalid native model/effort; choose a model and effort with /model");
            }
        }
    }
    Ok((model, effort))
}

pub(super) async fn select_model(
    admission: &Admission,
    session_id: SessionId,
    view: &mut SessionView,
    model: &str,
    effort: Option<Effort>,
) -> Result<String, LoopError> {
    let Some(profile) = view.defaults.provider.as_deref() else {
        return Ok("Select /provider first".into());
    };
    if validate_native_model_id(model).is_err() {
        return Ok("Error: invalid native model".into());
    }
    if let Some(name) = ProviderKind::custom_name(profile) {
        let root = open_state(admission)?;
        let custom = match CustomProfile::load_named(&root, name) {
            Ok(custom) => custom,
            Err(error) => return Ok(format!("Error: {error}")),
        };
        if custom.model() != model {
            return Ok("Error: Selected model does not match its custom profile".into());
        }
        if effort.is_some_and(|effort| !custom.efforts().contains(&effort)) {
            return Ok("Error: Effort unavailable for the selected Provider/model".into());
        }
    } else if ProviderKind::parse(profile) == Some(ProviderKind::ChatGpt) {
        if effort.is_none() {
            return Ok("Error: ChatGPT requires an explicit effort".into());
        }
    } else if arany::resolve_native_effort_for_run(profile, model, effort).is_err() {
        return Ok(
            "Error: invalid native model/effort; choose a model and effort with /model".into(),
        );
    }
    let mut defaults = view.defaults.clone();
    defaults.model = Some(model.to_owned());
    defaults.effort = effort;
    let reload_error = match update_defaults(admission, session_id, view, defaults).await {
        Ok(()) => None,
        Err(DefaultsError::Reload(error)) => Some(error),
        Err(error) => return recover_defaults_change(error),
    };
    let notice = match &reload_error {
        Some(error) => super::settings_reload_notice(error),
        None => format!(
            "Model: {model} · {}",
            effort.map_or("default", Effort::as_str)
        ),
    };
    // After a Reload failure, view.defaults still holds the saved selection.
    let notice = super::remembered_session_notice(admission, view, notice).await;
    match reload_error {
        Some(_) => Err(LoopError::Notice(notice)),
        None => Ok(notice),
    }
}

const TRANSCRIPT_USAGE: &str = "Usage: /transcript [agents [number]]";

pub(super) enum ActiveSubmission {
    SteeringNote(String),
    Pause,
    Rejected(String),
    Notice(String),
    OpenAgents,
    OpenHelp,
    OpenUsage { status: bool },
    OpenTranscript,
    OpenAgentChats(Option<usize>),
    ReadClipboard,
}

pub(super) fn handle_active_submission(
    composer: &mut Composer,
    view: &SessionView,
    starting_sequence: u64,
) -> ActiveSubmission {
    let notice = match composer.submission() {
        Ok(Submission::Command {
            command: command @ (InteractiveCommand::Usage | InteractiveCommand::Status),
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenUsage {
                status: command == InteractiveCommand::Status,
            };
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Transcript,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenTranscript;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Transcript,
            argument: Some(argument),
        }) => {
            let Some(number) = transcript_agents(argument) else {
                return ActiveSubmission::Rejected(TRANSCRIPT_USAGE.into());
            };
            composer.take();
            return ActiveSubmission::OpenAgentChats(number);
        }
        Ok(Submission::Objective(text)) => return ActiveSubmission::SteeringNote(text.to_owned()),
        Ok(Submission::Skill { name, .. }) => {
            return ActiveSubmission::Rejected(format!(
                "/{name} starts a new turn and cannot join the running one; wait for it or press Ctrl+C to cancel"
            ));
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Goal,
            argument: Some("pause"),
        }) => {
            composer.take();
            return ActiveSubmission::Pause;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Paste,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::ReadClipboard;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Agents,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenAgents;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Help,
            argument: None,
        }) => {
            composer.take();
            return ActiveSubmission::OpenHelp;
        }
        Ok(Submission::Command {
            command: InteractiveCommand::Goal,
            argument: Some(_),
        }) => "/goal changes are unavailable while a Run is active; /goal shows status; Ctrl+C cancels".into(),
        Ok(Submission::Command { command, .. }) => {
            let notice = active_command_notice(view, command, starting_sequence);
            if command == InteractiveCommand::New && composer.text().split_whitespace().next() == Some("/clear") {
                notice.replacen("/new", "/clear", 1)
            } else {
                notice
            }
        }
        Err(CommandParseError::Empty) => "Run is active; Ctrl+C cancels".into(),
        Err(CommandParseError::Unknown { suggestion }) => {
            return ActiveSubmission::Rejected(match suggestion {
                Some(name) => format!("Unknown command; did you mean /{name}?"),
                None => "Unknown command".into(),
            });
        }
        Err(error) => return ActiveSubmission::Rejected(error.to_string()),
    };
    composer.take();
    ActiveSubmission::Notice(notice)
}

pub(super) fn validate_idle_command(
    view: &SessionView,
    command: InteractiveCommand,
    argument: Option<&str>,
) -> Result<(), &'static str> {
    match command {
        InteractiveCommand::Resume
            if argument.is_some_and(|id| SessionId::from_str(id).is_err()) =>
        {
            Err("Usage: /resume SESSION_ID")
        }
        InteractiveCommand::Fork if argument.is_some_and(|id| SessionId::from_str(id).is_err()) => {
            Err("Usage: /fork [SESSION_ID]")
        }
        InteractiveCommand::Rename if argument.is_none() => Err("Usage: /rename TITLE"),
        InteractiveCommand::Rename
            if argument.is_some_and(|title| title.is_empty() || title.len() > 128) =>
        {
            Err("Title is empty or too long; use a shorter title")
        }
        InteractiveCommand::Provider
            if argument.is_some_and(|profile| ProviderArg::from_str(profile).is_err()) =>
        {
            Err("Provider must be openai, anthropic, chatgpt, or custom:NAME")
        }
        InteractiveCommand::Model if argument.is_some() => {
            typed_model_selection(view, argument.expect("model argument")).map(|_| ())
        }
        InteractiveCommand::Agents
            if argument.is_some_and(|policy| parse_policy(policy).is_err()) =>
        {
            Err("invalid collaboration policy")
        }
        InteractiveCommand::Cache if !matches!(argument, Some("clear" | "clear all")) => {
            Err(CACHE_USAGE)
        }
        InteractiveCommand::Transcript
            if argument.is_some_and(|argument| transcript_agents(argument).is_none()) =>
        {
            Err(TRANSCRIPT_USAGE)
        }
        _ => Ok(()),
    }
}

pub(super) async fn handle_command(
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    command: InteractiveCommand,
    argument: Option<&str>,
) -> Result<String, LoopError> {
    let notice = match command {
        InteractiveCommand::New => {
            let defaults = view.defaults.clone();
            let new_view = super::new_conversation(defaults);
            *session_id = new_view.id;
            *view = new_view;
            "New conversation · send a message to save it".into()
        }
        InteractiveCommand::Resume => {
            return resume_command(admission, session_id, view, argument).await;
        }
        InteractiveCommand::Goal if argument == Some("pause") => {
            "No Goal is running; /goal pause works during a Run".into()
        }
        InteractiveCommand::Goal => crate::presentation::goal_status(view),
        InteractiveCommand::Fork => {
            return fork_command(admission, session_id, view, argument).await;
        }
        InteractiveCommand::Rename => {
            return rename_command(admission, *session_id, view, argument).await;
        }
        InteractiveCommand::Provider => {
            return provider_command(admission, *session_id, view, argument).await;
        }
        InteractiveCommand::Model => {
            let Some(argument) = argument else {
                return Err("Model selection requires the terminal catalog".into());
            };
            let (model, effort) = match typed_model_selection(view, argument) {
                Ok(parsed) => parsed,
                Err(error) => return Ok(format!("Error: {error}")),
            };
            return select_model(admission, *session_id, view, model, effort).await;
        }
        InteractiveCommand::Agents => match argument {
            Some(argument) => match parse_policy(argument) {
                Ok(policy) => {
                    let mut defaults = view.defaults.clone();
                    defaults.policy = policy;
                    if let Err(error) =
                        update_defaults(admission, *session_id, view, defaults).await
                    {
                        return recover_defaults_change(error);
                    }
                    format!("Next-Run collaboration: {policy}")
                }
                Err(error) => error.into(),
            },
            None => format!("Next-Run collaboration: {}", view.defaults.policy),
        },
        InteractiveCommand::Usage | InteractiveCommand::Status => {
            return Err("Usage requires the terminal report".into());
        }
        InteractiveCommand::Help => {
            return Err("Help requires the terminal command registry".into());
        }
        InteractiveCommand::Permissions => {
            if admission.tools {
                "Tools requested from private tools.json; native protection must be admitted before a Run. File mutation requires the write grant; command changes are discarded. Primary only; commands/MCP have no network, host home or account state. Arany requests: selected Provider; OS TLS checks may connect separately.".into()
            } else if let Some(access) = admission
                .workspace_permissions
                .as_ref()
                .filter(|access| access.is_trusted())
            {
                let profiles = access.selected_runtime_profiles();
                format!(
                    "Folder trusted; file Tools use the Workspace. Commands/MCP use a private selected-project copy without network or account state; only these selected read-only toolchains and caches are mounted: {}. Arany requests: selected Provider; OS TLS checks may connect separately.",
                    if profiles.is_empty() {
                        "none".into()
                    } else {
                        profiles.join("; ")
                    }
                )
            } else {
                "Read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into()
            }
        }
        InteractiveCommand::Cache => return Ok(cache_command(admission, argument).await),
        _ => "Command is not available in this build".into(),
    };
    Ok(notice)
}

async fn resume_command(
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    argument: Option<&str>,
) -> Result<String, LoopError> {
    let Some(id) = argument.and_then(|value| SessionId::from_str(value).ok()) else {
        return Ok("Usage: /resume SESSION_ID".into());
    };
    let root = open_state(admission)?;
    let resumed = match resume_session(root, admission.workspace.clone(), id).await {
        Ok(resumed) => resumed,
        Err(error) => return Ok(format!("Error: {error}")),
    };
    let resumed = match super::prepare_resume(admission, resumed, Some(&view.defaults)).await {
        Ok(resumed) => resumed,
        Err(error) => return Ok(format!("Error: {error}")),
    };
    *session_id = id;
    *view = resumed;
    Ok(super::remembered_session_notice(
        admission,
        view,
        format!(
            "Resumed: {}",
            crate::presentation::escape_terminal(&view.conversation_title())
        ),
    )
    .await)
}

async fn fork_command(
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    argument: Option<&str>,
) -> Result<String, LoopError> {
    if argument.is_none() && view.created_sequence == 0 {
        return Ok("Send a message before forking this conversation".into());
    }
    let source_id = match argument {
        Some(value) => match SessionId::from_str(value) {
            Ok(id) => id,
            Err(_) => return Ok("Usage: /fork [SESSION_ID]".into()),
        },
        None => *session_id,
    };
    let root = open_state(admission)?;
    let id = match fork_session(root, admission.workspace.clone(), source_id, None).await {
        Ok(id) => id,
        Err(error) => return Ok(format!("Error: {error}")),
    };
    let forked = match load_view(&admission.state_dir, id).await {
        Ok(forked) => forked,
        Err(error) => {
            return Ok(format!(
                "Error: Forked Session {id}, but it could not be opened: {error}"
            ));
        }
    };
    let forked = match super::prepare_resume(admission, forked, Some(&view.defaults)).await {
        Ok(forked) => forked,
        Err(error) => {
            return Ok(format!(
                "Error: Forked Session {id}, but it could not be opened: {error}"
            ));
        }
    };
    *session_id = id;
    *view = forked;
    Ok(super::remembered_session_notice(admission, view, format!("Forked Session {id}")).await)
}

async fn rename_command(
    admission: &Admission,
    session_id: SessionId,
    view: &mut SessionView,
    argument: Option<&str>,
) -> Result<String, LoopError> {
    let Some(title) = argument else {
        return Ok("Usage: /rename TITLE".into());
    };
    if title.is_empty() || title.len() > 128 {
        return Ok("Error: invalid Run request".into());
    }
    if view.created_sequence == 0 {
        view.title = title.to_owned();
        view.title_is_explicit = true;
        return Ok(format!("Renamed conversation: {title}"));
    }
    let root = open_state(admission)?;
    match rename_session(
        root,
        admission.workspace.clone(),
        session_id,
        title.to_owned(),
    )
    .await
    {
        Ok(()) => match load_view(&admission.state_dir, session_id).await {
            Ok(renamed) => {
                *view = renamed;
                Ok(format!("Renamed Session: {title}"))
            }
            Err(error) => Ok(format!(
                "Error: Renamed Session, but it could not be reloaded: {error}"
            )),
        },
        Err(error) => Ok(format!("Error: {error}")),
    }
}

async fn provider_command(
    admission: &Admission,
    session_id: SessionId,
    view: &mut SessionView,
    argument: Option<&str>,
) -> Result<String, LoopError> {
    let Some(profile) = argument else {
        return Ok(format!(
            "Provider: {}",
            view.defaults.provider.as_deref().unwrap_or("not selected")
        ));
    };
    if ProviderArg::from_str(profile).is_err() {
        return Ok("Provider must be openai, anthropic, chatgpt, or custom:NAME".into());
    }
    let account_id = if ProviderKind::parse(profile) == Some(ProviderKind::ChatGpt) {
        match chatgpt::selected_account_id() {
            Ok(id) => Some(id),
            Err(chatgpt::AuthorizationError::NoSelectedAccount) => {
                return Ok("Error: no selected ChatGPT account; use /setup to connect".into());
            }
            Err(error) => return Ok(format!("Error: {error}")),
        }
    } else if view.defaults.provider.as_deref() == Some(profile) {
        view.defaults.account_id
    } else {
        None
    };
    let mut defaults = view.defaults.clone();
    if defaults.provider.as_deref() != Some(profile) || defaults.account_id != account_id {
        defaults.provider = Some(profile.to_owned());
        defaults.model = None;
        defaults.effort = None;
        defaults.account_id = account_id;
        if let Err(error) = update_defaults(admission, session_id, view, defaults).await {
            return recover_defaults_change(error);
        }
    }
    Ok(if view.defaults.model.is_none() {
        format!("Provider: {profile}; set /model")
    } else {
        format!("Provider: {profile}")
    })
}

async fn build_cache_used(admission: &Admission) -> Option<u64> {
    let path = admission.state_dir.clone();
    tokio::task::spawn_blocking(move || {
        StateRoot::open_existing(&path)
            .ok()?
            .build_cache_bytes()
            .ok()
    })
    .await
    .ok()
    .flatten()
}

async fn cache_command(admission: &Admission, argument: Option<&str>) -> String {
    let Ok(root) = StateRoot::open_existing(&admission.state_dir) else {
        return CACHE_CLEAR_FAILED.into();
    };
    let workspace = match argument {
        Some("clear") => Some(admission.workspace.as_path()),
        Some("clear all") => None,
        _ => return format!("Error: {CACHE_USAGE}"),
    };
    if let Err(error) = arany::recover_build_cache(&root, workspace).await {
        return if matches!(error, arany::StoreError::BuildCacheBusy) {
            CACHE_BUSY.into()
        } else {
            CACHE_CLEAR_FAILED.into()
        };
    }
    let workspace = workspace.map(std::path::Path::to_path_buf);
    let cleared = tokio::task::spawn_blocking(move || match workspace {
        Some(path) => (
            root.clear_workspace_build_cache(&path),
            "Cleared this Workspace's private build cache",
        ),
        None => (
            root.clear_all_build_caches(),
            "Cleared all private build caches",
        ),
    })
    .await;
    let Ok((result, cleared)) = cleared else {
        return CACHE_CLEAR_FAILED.into();
    };
    match result {
        Ok(()) => cleared.into(),
        Err(arany::StoreError::BuildCacheBusy) => CACHE_BUSY.into(),
        Err(_) => CACHE_CLEAR_FAILED.into(),
    }
}

pub(super) fn active_command_notice(
    view: &SessionView,
    command: InteractiveCommand,
    starting_sequence: u64,
) -> String {
    let unavailable = format!(
        "/{} is unavailable while a Run is active; Ctrl+C cancels",
        command.name()
    );
    if command.availability(true) == CommandAvailability::Locked {
        return unavailable;
    }
    let run = view
        .runs
        .last()
        .filter(|run| run.accepted_sequence > starting_sequence);
    match command {
        InteractiveCommand::Goal => crate::presentation::goal_status(view),
        InteractiveCommand::Agents => {
            "/agents changes are unavailable while a Run is active; /agents opens details; Ctrl+C cancels".into()
        }
        InteractiveCommand::Provider => {
            let pinned_provider = run
                .and_then(|run| run.config.as_ref())
                .map(|config| config.provider.profile());
            let provider = pinned_provider.as_deref().or(view.defaults.provider.as_deref());
            format!(
                "Provider: {}; /provider changes are unavailable while a Run is active; Ctrl+C cancels",
                provider.unwrap_or("not selected")
            )
        }
        InteractiveCommand::Model => {
            let model = run
                .and_then(|run| run.config.as_ref())
                .map_or(view.defaults.model.as_deref(), |config| {
                    Some(config.model.as_str())
                });
            let effort = run
                .and_then(|run| run.config.as_ref())
                .map_or(view.defaults.effort, |config| config.effort);
            format!(
                "Model: {} · {}; /model changes are unavailable while a Run is active; Ctrl+C cancels",
                model.unwrap_or("not selected"),
                effort.map_or("default", Effort::as_str)
            )
        }
        InteractiveCommand::Permissions => {
            if run
                .and_then(|run| run.config.as_ref())
                .is_some_and(|config| config.tool_policy.is_some())
            {
                "Primary has pinned guarded Tool grants: at most 16 Tool calls with bounded Tool context. Commands/MCP use a private selected-project copy without network or account state; only the selected read-only toolchains and caches from home are mounted, and their file changes are discarded. Children remain read-only. /permissions edits typed Git rules only when no Run is active; saved changes apply from the next Run. Arany requests: selected Provider; OS TLS checks may connect separately.".into()
            } else {
                "Read-only Workspace; Arany requests: selected Provider; OS TLS checks may connect separately; no Tools or sandbox".into()
            }
        }
        InteractiveCommand::Exit => {
            "/exit is unavailable while a Run is active; Ctrl+C cancels; enter /exit after it ends".into()
        }
        _ => unavailable,
    }
}

fn parse_policy(argument: &str) -> Result<CollaborationPolicy, &'static str> {
    let mut parts = argument.split_ascii_whitespace();
    let mode = parts.next().ok_or("invalid collaboration policy")?;
    let count = parts
        .next()
        .map(str::parse::<u8>)
        .transpose()
        .map_err(|_| "invalid collaboration policy")?;
    if parts.next().is_some() {
        return Err("invalid collaboration policy");
    }
    match (mode, count) {
        ("single", None) => Ok(CollaborationPolicy::Single),
        ("auto", count) if count.unwrap_or(3) <= 8 => Ok(CollaborationPolicy::Auto {
            max_active_children: count.unwrap_or(3),
        }),
        ("team", count) if (1..=8).contains(&count.unwrap_or(3)) => Ok(CollaborationPolicy::Team {
            max_active_children: count.unwrap_or(3),
        }),
        _ => Err("invalid collaboration policy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::attached::{EntryMode, persist_defaults};
    use crate::terminal::TerminalInput;
    use arany::{
        AgentRole, AgentRunId, AgentStatus, AgentView, ContextUsage, RunConfig, RunId, RunStatus,
        RunView, SessionDefaults, Store, create_session,
    };

    #[test]
    fn active_submissions_keep_objectives_local_and_lock_mutations() {
        let mut view = SessionView {
            usage: Default::default(),
            rewinds: Default::default(),
            id: SessionId::new(),
            title: "Current".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            host_epoch_floor: 0,
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            goals: Vec::new(),
            command_tool_probe: None,
            pending_provider_setup_edit: None,
            provider_setup_uncertain: false,
            environment_capture: None,
            runs: Vec::new(),
            compactions: Vec::new(),
        };
        let starting_sequence = view.last_sequence;
        for command in ["/usage", "/status"] {
            let mut draft = Composer::default();
            for character in command.chars() {
                draft.apply(TerminalInput::Character(character));
            }
            assert!(
                matches!(handle_active_submission(&mut draft, &view, starting_sequence), ActiveSubmission::OpenUsage { status } if status == (command == "/status"))
            );
        }

        let mut composer = Composer::default();
        for character in "//literal".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::SteeringNote(ref text) if text == "/literal"
        ));
        assert_eq!(composer.text(), "//literal");

        composer.clear();
        for character in "/goal pause".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::Pause
        ));
        assert!(
            composer.is_empty(),
            "/goal pause is a control, not steering text"
        );
        for character in "/goal pause now".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::Notice(ref notice) if notice == "/goal changes are unavailable while a Run is active; /goal shows status; Ctrl+C cancels"
        ));

        composer.clear();
        for character in "/provder".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        composer.apply(TerminalInput::Left);
        composer.apply(TerminalInput::Left);
        let caret = composer.cursor_byte_offset();
        let ActiveSubmission::Rejected(notice) =
            handle_active_submission(&mut composer, &view, starting_sequence)
        else {
            panic!("unknown active command must be rejected locally")
        };
        assert_eq!(notice, "Unknown command; did you mean /provider?");
        assert_eq!(
            composer.text(),
            "/provder",
            "local rejection keeps the draft"
        );
        assert_eq!(composer.cursor_byte_offset(), caret);

        for (command, expected) in [
            ("/status unused", "Usage: /status"),
            ("/help unused", "Usage: /help"),
            ("/permissions unused", "Usage: /permissions"),
            ("/exit unused", "Usage: /exit"),
            ("/clear unused", "Usage: /clear"),
        ] {
            composer.clear();
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            composer.apply(TerminalInput::Left);
            composer.apply(TerminalInput::Left);
            let caret = composer.cursor_byte_offset();
            let ActiveSubmission::Rejected(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("unexpected active command arguments must be rejected locally")
            };
            assert_eq!(notice, expected);
            assert_eq!(composer.text(), command);
            assert_eq!(composer.cursor_byte_offset(), caret);
            assert_eq!(view.defaults, SessionDefaults::default());
        }

        composer.clear();
        for command in ["/provider", "/provider anthropic"] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("command must remain local")
            };
            assert_eq!(
                notice,
                "Provider: not selected; /provider changes are unavailable while a Run is active; Ctrl+C cancels"
            );
            assert!(composer.is_empty());
            assert_eq!(view.defaults, SessionDefaults::default());
        }

        for character in "/agents".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::OpenAgents
        ));
        assert!(composer.is_empty());
        assert_eq!(view.defaults, SessionDefaults::default());

        for command in ["/model", "/model gpt-5.4"] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("model inspection must remain local")
            };
            assert_eq!(
                notice,
                "Model: not selected · default; /model changes are unavailable while a Run is active; Ctrl+C cancels",
                "{command}"
            );
            assert!(composer.is_empty());
            assert_eq!(view.defaults, SessionDefaults::default());
        }

        for character in "/help".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::OpenHelp
        ));
        assert!(composer.is_empty());

        for character in "/paste".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::ReadClipboard
        ));
        assert!(composer.is_empty());
        assert_eq!(view.defaults, SessionDefaults::default());

        for (command, suffix) in [
            ("/new", ""),
            ("/clear", ""),
            ("/compact", ""),
            ("/settings", ""),
            ("/exit", "; enter /exit after it ends"),
        ] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("command must remain local")
            };
            assert_eq!(
                notice,
                format!("{command} is unavailable while a Run is active; Ctrl+C cancels{suffix}"),
                "{command}"
            );
            assert!(composer.is_empty());
        }

        let run_id = RunId::new();
        view.runs.push(RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: run_id,
            objective: "Current objective".into(),
            images: Vec::new(),
            config: None,
            agents: vec![AgentView {
                tool_refusal: None,
                tool_refusal_sequence: None,
                profile: None,
                assignment: None,
                definition: None,
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                steering_notes: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 2,
            finished_sequence: None,
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
        });
        assert!(
            crate::presentation::usage_pages(&view, true, Some(starting_sequence))
                .concat()
                .contains("Context left: unknown (not reported)")
        );
        view.runs.last_mut().expect("active Run").config = Some(RunConfig {
            output_token_cap: 1024,
            workspace_inode: 1,
            context_usage: Some(ContextUsage {
                used_bytes: 32_000,
                budget_bytes: 65_536,
                compactable_bytes: 20_000,
                tool_history_bytes: 0,
            }),
            ..arany::test_support::run_config()
        });
        let notice =
            crate::presentation::usage_pages(&view, true, Some(starting_sequence)).concat();
        assert!(notice.contains("Context left: 32.8 KB (Arany request budget at admission)"));
        assert!(!notice.contains(&run_id.to_string()));
        for status in [RunStatus::Finished, RunStatus::Failed, RunStatus::Cancelled] {
            view.runs.last_mut().expect("latest Run").status = status;
            let report =
                crate::presentation::usage_pages(&view, true, Some(starting_sequence)).concat();
            assert!(report.contains("Model: gpt-5.4"));
            assert!(!report.contains("Request:"));
            assert_eq!(
                active_command_notice(&view, InteractiveCommand::Model, starting_sequence),
                "Model: gpt-5.4 · default; /model changes are unavailable while a Run is active; Ctrl+C cancels"
            );
        }
        view.runs.last_mut().expect("previous Run").status = RunStatus::Finished;
        assert_eq!(
            active_command_notice(&view, InteractiveCommand::Agents, starting_sequence),
            "/agents changes are unavailable while a Run is active; /agents opens details; Ctrl+C cancels"
        );
        assert_eq!(
            active_command_notice(&view, InteractiveCommand::Model, starting_sequence),
            "Model: gpt-5.4 · default; /model changes are unavailable while a Run is active; Ctrl+C cancels"
        );
        view.defaults.provider = Some("anthropic".into());
        view.defaults.model = Some("claude-sonnet-5".into());
        view.defaults.effort = Some(arany::Effort::Low);
        view.last_sequence = 4;
        view.runs
            .last_mut()
            .expect("previous Run")
            .finished_sequence = Some(4);
        let starting_sequence = view.last_sequence;
        let defaults = view.defaults.clone();
        for (command, expected) in [
            (
                "/provider",
                "Provider: anthropic; /provider changes are unavailable while a Run is active; Ctrl+C cancels".to_owned(),
            ),
            (
                "/model",
                "Model: claude-sonnet-5 · low; /model changes are unavailable while a Run is active; Ctrl+C cancels".to_owned(),
            ),
        ] {
            for character in command.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            let ActiveSubmission::Notice(notice) =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("admission inspection must remain local")
            };
            assert_eq!(notice, expected, "{command} before current Run progress");
            assert!(composer.is_empty());
            assert_eq!(view.defaults, defaults);
        }
        for (command, expected_status) in [("/status", true), ("/usage", false)] {
            composer.insert_paste(command).unwrap();
            let ActiveSubmission::OpenUsage { status } =
                handle_active_submission(&mut composer, &view, starting_sequence)
            else {
                panic!("admission inspection must open the local usage report")
            };
            assert_eq!(status, expected_status);
            let report =
                crate::presentation::usage_pages(&view, status, Some(starting_sequence)).concat();
            assert_eq!(
                report.contains("Preparing message · Ctrl+C cancels"),
                expected_status
            );
            if expected_status {
                assert!(!report.contains("Session totals"));
            } else {
                assert!(report.contains("Session totals: no calls yet"));
            }
            assert!(composer.is_empty());
            assert_eq!(view.defaults, defaults);
        }
        let image: arany::ImageAttachment = serde_json::from_str(r#"{"media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"}"#).unwrap();
        composer.attach_image(image.clone()).unwrap();
        assert!(matches!(
            handle_active_submission(&mut composer, &view, starting_sequence),
            ActiveSubmission::SteeringNote(_)
        ));
        for command in ["/help", "/agents", "/paste", "/provider"] {
            composer.insert_paste(command).unwrap();
            let result = handle_active_submission(&mut composer, &view, starting_sequence);
            assert!(matches!(
                result,
                ActiveSubmission::OpenHelp
                    | ActiveSubmission::OpenAgents
                    | ActiveSubmission::ReadClipboard
                    | ActiveSubmission::Notice(_)
            ));
            assert_eq!(
                composer.images(),
                std::slice::from_ref(&image),
                "active control discarded images: {command}"
            );
            assert_eq!(composer.text(), "");
        }
    }

    #[test]
    fn collaboration_command_accepts_only_bounded_next_run_policy() {
        let cases = [
            ("single", Some(CollaborationPolicy::Single)),
            (
                "auto",
                Some(CollaborationPolicy::Auto {
                    max_active_children: 3,
                }),
            ),
            (
                "auto 0",
                Some(CollaborationPolicy::Auto {
                    max_active_children: 0,
                }),
            ),
            (
                "team 8",
                Some(CollaborationPolicy::Team {
                    max_active_children: 8,
                }),
            ),
            ("single 1", None),
            ("auto 9", None),
            ("team 0", None),
            ("team 2 extra", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_policy(input).ok(), expected, "{input}");
        }
    }

    #[test]
    fn linear_settings_take_number_value_and_reject_invalid_rows_unchanged() {
        let mut preferences = Preferences::default();
        let original = preferences;
        for value in [
            "",
            "3",
            "3 ",
            "0 64",
            "12 64",
            "12 yes",
            "12 wheel only",
            "13 yes",
            "14 on",
            "x 64",
            "calls=64",
            "combined",
            "1 fullscreen",
            "2 tabs",
            "3 0",
            "3 4294967296",
            "4 0",
            "5 0s",
            "6 600.001s",
            "7 119.999s",
            "6 unlimited",
            "8 0B",
            "9 257",
            "10 1073741825B",
            "10 1B",
            "10 1073741823B",
            "11 8589934593B",
            "11 1B",
            "11 8589934591B",
            "5 60000",
            "6 30000",
            "8 512",
            "5 8.001h",
            "5 -1s",
            "5 +1s",
            "5 NaNs",
            "5 1e3s",
            "5 .5s",
            "5 1.s",
            "5 1..5s",
            "5 1ms",
            "5 0.0001s",
            "5 1s 1m",
            "5 1s 1s",
            "5 1s junk",
            "5 18446744073709551615h",
            "5 18446744073709551.615s 1s",
            "6 4294967.296s",
            "8 unlimited",
            "8 0.1B",
            "8 1GB 1MB",
            "8 1XB",
            "8 18446744073709551616B",
            "8 18446744073709551615TB",
            "8 0.0000000000000000000000000000000000000001KB",
        ] {
            assert!(!apply_setting(&mut preferences, value), "{value}");
            assert_eq!(
                preferences, original,
                "invalid value changed draft: {value}"
            );
        }
        let cores = std::thread::available_parallelism()
            .ok()
            .and_then(|cores| u32::try_from(cores.get()).ok());
        let process_limit = preferences.resources.build_processes;
        let build_jobs = match cores {
            Some(cores) => format!(
                "Build jobs: automatic; {} effective (cores {cores}, process limit {process_limit}) (default) (automatic or 1..cores)",
                cores.min(process_limit)
            ),
            None => {
                "Build jobs: automatic; Build unavailable (default) (automatic or 1..cores)".into()
            }
        };
        let build_cpu = match cores {
            Some(cores) => format!(
                "Build CPU cores: automatic; {cores} effective (1..{cores}) (default) (automatic or 1..cores; Linux quota)"
            ),
            None => {
                "Build CPU cores: automatic; Build unavailable (default) (automatic or 1..cores; Linux quota)"
                    .into()
            }
        };
        assert_eq!(
            linear_setting_rows(preferences, None),
            [
                "Slash menu: tabs (default) (tabs|combined)",
                "Chat view: fullscreen (default) (fullscreen|classic)",
                "Provider calls per task: unlimited (default) (number or unlimited)",
                "Tokens per task: unlimited (default) (number or unlimited)",
                "Active time per task: unlimited (default) (duration or unlimited)",
                "Default command timeout: 2 min (default) (duration, e.g. 30s)",
                "Maximum command timeout: 10 min (default) (duration, e.g. 5m)",
                "Build memory: 4.0 GB (default) (size, e.g. 512MB)",
                "Build processes/threads: 256 (default) (number)",
                "Build writable project: 1.0 GB (default) (size, e.g. 1GB)",
                "Build cache: unlimited (default) (size or unlimited, e.g. 8GB)",
                "Copy on select: on (default) (on|off)",
                "Newline shortcut: Shift+Enter (default) (shift+enter|alt+enter|ctrl+enter)",
                build_jobs.as_str(),
                build_cpu.as_str(),
                "Context window: 100% (default) (percent or tokens, e.g. 80% or 500k)",
            ]
        );
        for (raw, expected) in [
            ("80%", arany::ContextWindow::Percent(80)),
            ("500k", arany::ContextWindow::Tokens(500_000)),
            ("500000", arany::ContextWindow::Tokens(500_000)),
        ] {
            assert!(apply_setting(&mut preferences, &format!("16 {raw}")));
            assert_eq!(preferences.context_window, expected);
        }
        for rejected in ["16 5%", "16 101%", "16 1000", "16 3m"] {
            assert!(!apply_setting(&mut preferences, rejected), "{rejected}");
        }
        assert_eq!(
            preferences.context_window,
            arany::ContextWindow::Tokens(500_000)
        );
        let saved = encode_settings(preferences).expect("encoded settings");
        assert_eq!(
            serde_json::from_slice::<Value>(&saved).unwrap()["preferences"],
            serde_json::json!({"context_window": "500k"}),
            "only the changed key is saved"
        );
        assert_eq!(decode_settings(&saved), Ok(preferences));
        assert!(apply_setting(&mut preferences, "16 default"));
        assert_eq!(preferences, Preferences::default());
        let mut cycled = Vec::new();
        for _ in 0..CONTEXT_WINDOW_PRESETS.len() {
            cycle_setting(&mut preferences, 15, true, Some(400_000));
            cycled.push(preferences.context_window.to_string());
        }
        assert_eq!(
            cycled,
            ["75%", "50%", "256k", "128k", "100%", "75%", "50%"],
            "token presets above the model window are skipped"
        );
        preferences = Preferences::default();
        cycle_setting(&mut preferences, 15, false, None);
        assert_eq!(preferences.context_window.to_string(), "128k");
        preferences = Preferences::default();
        let typed = SETTINGS
            .iter()
            .filter(|setting| setting.input == SettingInput::Typed)
            .map(|setting| setting.label)
            .collect::<Vec<_>>();
        assert!(typed.contains(&"Context window") && typed.contains(&"Build memory"));
        assert!(!typed.contains(&"Chat view") && !typed.contains(&"Newline shortcut"));
        for (raw, expected) in [
            ("30s", 30_000),
            ("5m", 300_000),
            (" 5M ", 300_000),
            ("0.5h", 1_800_000),
            ("1 min 30.001 s", 90_001),
            ("0.001S", 1),
            ("0.0001m", 6),
        ] {
            assert!(apply_setting(&mut preferences, &format!("5 {raw}")));
            assert_eq!(preferences.goal_budget.max_active_ms, Some(expected));
        }
        for (raw, expected) in [
            ("1B", 1),
            ("1023 b", 1023),
            ("1 KB", 1024),
            ("512MB", 512 * 1024 * 1024),
            ("1.5gb", 1536 * 1024 * 1024),
            ("0.00390625KB", 4),
            ("4.0 GB", 4 * 1024_u64.pow(3)),
        ] {
            assert!(apply_setting(&mut preferences, &format!("8 {raw}")));
            assert_eq!(preferences.resources.build_memory_bytes, expected);
        }
        assert!(apply_setting(&mut preferences, "6 1.5s"));
        assert_eq!(preferences.resources.default_command_timeout_ms, 1500);
        assert!(apply_setting(&mut preferences, "10 512MB"));
        assert_eq!(preferences.resources.build_project_bytes, 512 * 1024 * 1024);
        assert!(apply_setting(&mut preferences, "11 0B"));
        assert_eq!(
            preferences.resources.build_cache_bytes,
            BuildCacheSize::DISABLED
        );
        assert!(apply_setting(&mut preferences, "11 8gb"));
        assert_eq!(
            preferences.resources.build_cache_bytes,
            BuildCacheSize::Bytes(8 * 1024_u64.pow(3))
        );
        assert!(apply_setting(&mut preferences, "11 unlimited"));
        assert_eq!(
            preferences.resources.build_cache_bytes,
            BuildCacheSize::Unlimited
        );
        assert_eq!(
            setting_rows(preferences, Some(3_435_973_837))[10],
            "Build cache: unlimited (default) · 3.2 GB used"
        );
        for shortcut in NewlineShortcut::ALL {
            assert!(apply_setting(
                &mut preferences,
                &format!("13 {}", shortcut.label().to_ascii_lowercase())
            ));
            assert_eq!(preferences.newline, shortcut);
            cycle_setting(&mut preferences, 12, true, None);
            cycle_setting(&mut preferences, 12, false, None);
            assert_eq!(preferences.newline, shortcut);
        }
        assert!(apply_setting(&mut preferences, "1 combined"));
        assert!(apply_setting(&mut preferences, "2  fullscreen"));
        assert!(apply_setting(&mut preferences, "3 8"));
        assert_eq!(preferences.goal_budget.max_provider_calls, Some(8));
        assert!(apply_setting(&mut preferences, "3 unlimited"));
        assert!(apply_setting(&mut preferences, "5 1min"));
        assert_eq!(preferences.goal_budget.max_active_ms, Some(60_000));
        assert!(apply_setting(&mut preferences, "5 unlimited"));
        assert!(apply_setting(&mut preferences, "4 100000"));
        assert!(apply_setting(&mut preferences, "7 5m"));
        assert!(apply_setting(&mut preferences, "12 off"));
        assert!(!preferences.copy_on_select);
        cycle_setting(&mut preferences, 11, true, None);
        assert!(preferences.copy_on_select);
        assert_eq!(preferences.completion, CompletionLayout::Combined);
        assert_eq!(preferences.view, TerminalMode::Fullscreen);
        assert_eq!(preferences.goal_budget.max_provider_calls, None);
        assert_eq!(preferences.goal_budget.max_tokens, Some(100_000));
        assert_eq!(preferences.resources.max_command_timeout_ms, 300_000);
        assert!(setting_rows(preferences, None).contains(&"Tokens per task: 100000".into()));
        assert!(apply_setting(&mut preferences, "4 default"));
        assert!(
            setting_rows(preferences, None)
                .contains(&"Tokens per task: unlimited (default)".into())
        );
        assert!(apply_setting(&mut preferences, "3 64"));
        reset_setting(&mut preferences, 2);
        assert_eq!(preferences.goal_budget, Preferences::default().goal_budget);
        for row in 0..SETTINGS.len() {
            reset_setting(&mut preferences, row);
        }
        assert_eq!(preferences, Preferences::default());
    }

    #[test]
    fn goal_budget_cycles_start_at_their_unlimited_default() {
        let mut preferences = Preferences::default();
        for (row, first) in [(2, Some(256)), (4, Some(arany::MAX_GOAL_ACTIVE_MS))] {
            cycle_setting(&mut preferences, row, true, None);
            let budget = preferences.goal_budget;
            assert_eq!(
                budget
                    .max_provider_calls
                    .map(u64::from)
                    .or(budget.max_active_ms),
                first
            );
            cycle_setting(&mut preferences, row, false, None);
            assert_eq!(preferences, Preferences::default());
        }
    }

    #[test]
    fn saved_settings_keep_only_changed_keys_and_follow_current_defaults() {
        assert_eq!(
            encode_settings(Preferences::default()).unwrap(),
            br#"{"version":9,"preferences":{}}"#
        );
        let mut changed = Preferences::default();
        changed.goal_budget.max_provider_calls = Some(64);
        changed.resources.build_jobs = Some(2);
        changed.view = TerminalMode::Classic;
        let record = encode_settings(changed).unwrap();
        assert_eq!(
            record,
            br#"{"version":9,"preferences":{"goal_budget":{"max_provider_calls":64},"resources":{"build_jobs":2},"view":"classic"}}"#
        );
        assert_eq!(decode_settings(&record), Ok(changed));
        assert!((UI_PREFERENCES.accepts)(&record));
        for invalid in [
            br#"{"version":9,"preferences":{"unknown":1}}"#.as_slice(),
            br#"{"version":9,"preferences":{"goal_budget":{"unknown":1}}}"#,
            br#"{"version":9,"preferences":{"goal_budget":3}}"#,
            br#"{"version":9,"preferences":{"view":"unknown"}}"#,
            br#"{"version":9}"#,
        ] {
            assert!(decode_settings(invalid).is_err());
        }
        assert_eq!(
            decode_settings(
                br#"{"version":9,"preferences":{"goal_budget":{"max_provider_calls":0}}}"#
            ),
            Err("unsupported settings; use /settings to replace them")
        );
    }

    #[test]
    fn every_setting_round_trips_its_rendered_value_and_limits_unlimited() {
        let defaults = Preferences::default();
        let custom = Preferences {
            completion: CompletionLayout::Combined,
            view: TerminalMode::Classic,
            goal_budget: arany::GoalBudget {
                max_provider_calls: Some(7),
                max_tokens: Some(123_456),
                max_active_ms: Some(90_001),
                ..defaults.goal_budget
            },
            resources: arany::ToolResourceSettings {
                default_command_timeout_ms: 1501,
                max_command_timeout_ms: 123_456,
                build_memory_bytes: 1025,
                build_processes: 17,
                build_jobs: None,
                build_cpu_cores: None,
                build_project_bytes: 513 * 1024 * 1024,
                build_cache_bytes: BuildCacheSize::Bytes(1025 * 1024 * 1024),
            },
            copy_on_select: false,
            newline: NewlineShortcut::AltEnter,
            context_window: arany::ContextWindow::Tokens(123_456),
        };
        assert!(setting_rows(custom, None).contains(&"Build memory: 1.0009765625 KB".into()));
        assert!(setting_rows(custom, None).contains(&"Context window: 123456".into()));
        assert!(setting_rows(custom, None).contains(&"Build cache: 1.0009765625 GB".into()));
        for mut original in [defaults, custom] {
            for memory in [
                1,
                1023,
                1024,
                1025,
                1536,
                1024 * 1024 - 1,
                4 * 1024_u64.pow(3) - 1,
                4 * 1024_u64.pow(3),
            ] {
                original.resources.build_memory_bytes = memory;
                assert!(original.resources.valid());
                for (index, setting) in SETTINGS.iter().enumerate() {
                    let number = index + 1;
                    let rendered = (setting.value)(&original);
                    let mut chosen = original;
                    assert!(
                        apply_setting(&mut chosen, &format!("{number} {rendered}")),
                        "{}: {rendered}",
                        setting.label
                    );
                    assert_eq!(chosen, original, "{}: {rendered}", setting.label);
                    let accepts_unlimited = matches!(number, 3..=5 | 11);
                    assert_eq!(
                        apply_setting(&mut chosen, &format!("{number} unlimited")),
                        accepts_unlimited,
                        "{}",
                        setting.label
                    );
                    if !accepts_unlimited {
                        assert_eq!(chosen, original, "{}", setting.label);
                    }
                }
            }
        }
    }

    #[test]
    fn idle_validation_rejects_local_arguments_before_any_effect() {
        let mut view = SessionView {
            usage: Default::default(),
            rewinds: Default::default(),
            id: SessionId::new(),
            title: "Current".into(),
            title_is_explicit: true,
            inherited_title: None,
            workspace_identity: None,
            defaults: SessionDefaults::default(),
            host_epoch_floor: 0,
            created_sequence: 1,
            last_sequence: 1,
            lineage: None,
            goals: Vec::new(),
            command_tool_probe: None,
            pending_provider_setup_edit: None,
            provider_setup_uncertain: false,
            environment_capture: None,
            runs: Vec::new(),
            compactions: Vec::new(),
        };
        let oversized_title = "x".repeat(129);
        let oversized_unicode_title = "é".repeat(65);
        for (command, argument, expected) in [
            (
                InteractiveCommand::Provider,
                "other",
                "Provider must be openai, anthropic, chatgpt, or custom:NAME",
            ),
            (
                InteractiveCommand::Agents,
                "team 0",
                "invalid collaboration policy",
            ),
            (
                InteractiveCommand::Resume,
                "not-an-id",
                "Usage: /resume SESSION_ID",
            ),
            (
                InteractiveCommand::Rename,
                oversized_title.as_str(),
                "Title is empty or too long; use a shorter title",
            ),
            (
                InteractiveCommand::Rename,
                "",
                "Title is empty or too long; use a shorter title",
            ),
            (
                InteractiveCommand::Rename,
                oversized_unicode_title.as_str(),
                "Title is empty or too long; use a shorter title",
            ),
        ] {
            assert_eq!(
                validate_idle_command(&view, command, Some(argument)),
                Err(expected),
                "{command:?} {argument}"
            );
        }
        assert_eq!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4")),
            Err("Select /provider first")
        );
        assert_eq!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4 medium")),
            Err("Select /provider first")
        );
        view.defaults.provider = Some("openai".into());
        assert!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4 medium")).is_ok()
        );
        assert_eq!(
            typed_model_selection(&view, "gpt-5.4 max"),
            Err("invalid native model/effort; choose a model and effort with /model")
        );
        assert_eq!(
            typed_model_selection(&view, "unknown-native-model"),
            Ok(("unknown-native-model", Some(Effort::Low)))
        );
        view.defaults.model = Some("gpt-5.4".into());
        view.defaults.effort = Some(Effort::High);
        assert_eq!(
            typed_model_selection(&view, "gpt-5.4"),
            Ok(("gpt-5.4", Some(Effort::High)))
        );
        view.defaults.provider = Some("chatgpt".into());
        assert_eq!(
            typed_model_selection(&view, "gpt-5.4 default"),
            Err("ChatGPT requires an explicit effort")
        );
        assert_eq!(
            validate_idle_command(&view, InteractiveCommand::Model, Some("gpt-5.4 impossible")),
            Err("Effort unavailable for the selected Provider/model")
        );
        assert!(validate_idle_command(&view, InteractiveCommand::Agents, Some("team 2")).is_ok());
        assert!(validate_idle_command(&view, InteractiveCommand::Exit, None).is_ok());
        for title in ["x".repeat(128), "é".repeat(64)] {
            assert!(validate_idle_command(&view, InteractiveCommand::Rename, Some(&title)).is_ok());
        }
    }

    #[tokio::test]
    async fn display_settings_reopen_without_session_authority_and_reject_unsafe_records() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let admission = Admission {
            roster: None,
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            workspace_identity: arany::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let combined_classic = Preferences {
            completion: CompletionLayout::Combined,
            view: TerminalMode::Classic,
            ..Preferences::default()
        };
        assert_eq!(
            restore_settings(&admission).await.unwrap(),
            Preferences {
                completion: CompletionLayout::Tabs,
                view: TerminalMode::Fullscreen,
                ..Preferences::default()
            }
        );
        assert!(
            !admission.state_dir.exists(),
            "reading defaults creates no State"
        );
        for preferences in [
            combined_classic,
            Preferences {
                view: TerminalMode::Fullscreen,
                ..Preferences::default()
            },
            Preferences {
                copy_on_select: false,
                ..Preferences::default()
            },
            Preferences::default(),
            Preferences {
                newline: NewlineShortcut::AltEnter,
                ..Preferences::default()
            },
            Preferences {
                newline: NewlineShortcut::CtrlEnter,
                ..Preferences::default()
            },
        ] {
            save_settings(&admission, preferences).await.unwrap();
            assert_eq!(restore_settings(&admission).await.unwrap(), preferences);
            assert_eq!(
                std::fs::metadata(admission.state_dir.join("events.sqlite3"))
                    .unwrap()
                    .len(),
                0
            );
        }
        let root = StateRoot::open_existing(&admission.state_dir).unwrap();
        for document in [
            br#"{"version":1,"completion":"tabs"}"#.as_slice(),
            br#"{"version":2,"completion":"tabs","view":"classic"}"#.as_slice(),
            br#"{"version":3,"preferences":{"completion":"tabs","view":"classic"}}"#.as_slice(),
            br#"{"version":5,"preferences":{"completion":"tabs","view":"classic"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"unknown","view":"classic"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs","view":"unknown"}}"#.as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs","view":"classic","tools":true}}"#
                .as_slice(),
            br#"{"version":2,"preferences":{"completion":"tabs","view":"classic"},"tools":true}"#
                .as_slice(),
            br#"{"version":2}"#.as_slice(),
            b"malformed",
        ] {
            root.record(RecordKind::UiPreferences)
                .replace(document)
                .unwrap();
            assert!(restore_settings(&admission).await.is_err());
            assert_eq!(
                settings_or_defaults(&admission).await.unwrap(),
                Preferences::default()
            );
            assert_eq!(
                root.record(RecordKind::UiPreferences)
                    .read()
                    .unwrap()
                    .unwrap(),
                document
            );
        }
        save_settings(&admission, combined_classic).await.unwrap();
        let original = root
            .record(RecordKind::UiPreferences)
            .read()
            .unwrap()
            .unwrap();
        assert_eq!(
            original,
            br#"{"version":9,"preferences":{"completion":"combined","view":"classic"}}"#
        );
        // Records of any other version stay untouched for the startup archive; reading refuses them.
        for version in [3, 8, 10] {
            let mut other: Value = serde_json::from_slice(&original).unwrap();
            other["version"] = version.into();
            let other = serde_json::to_vec(&other).unwrap();
            root.record(RecordKind::UiPreferences)
                .replace(&other)
                .unwrap();
            assert!(UI_PREFERENCES.mismatched(&other));
            assert!(restore_settings(&admission).await.is_err());
            assert_eq!(
                root.record(RecordKind::UiPreferences)
                    .read()
                    .unwrap()
                    .unwrap(),
                other
            );
        }
        let mut invalid_shortcut: Value = serde_json::from_slice(&original).unwrap();
        invalid_shortcut["preferences"]["newline"] = "enter".into();
        let invalid_shortcut = serde_json::to_vec(&invalid_shortcut).unwrap();
        root.record(RecordKind::UiPreferences)
            .replace(&invalid_shortcut)
            .unwrap();
        assert!(restore_settings(&admission).await.is_err());
        assert_eq!(
            root.record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            invalid_shortcut
        );
        root.record(RecordKind::UiPreferences)
            .replace(&original)
            .unwrap();
        let mut invalid = Preferences::default();
        invalid.resources.build_processes = 0;
        assert!(save_settings(&admission, invalid).await.is_err());
        assert_eq!(
            root.record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            original,
            "invalid settings never replace the current record"
        );
        assert!(
            root.record(RecordKind::UiPreferences)
                .replace(&vec![b'x'; 1025])
                .is_err()
        );
        assert_eq!(
            root.record(RecordKind::UiPreferences)
                .read()
                .unwrap()
                .unwrap(),
            original
        );
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let pending = admission.state_dir.join("ui-preferences.pending");
            let outside = temp.path().join("outside");
            std::fs::write(&outside, b"unchanged").unwrap();
            symlink(&outside, &pending).unwrap();
            assert!(
                save_settings(&admission, Preferences::default())
                    .await
                    .is_err()
            );
            assert_eq!(
                restore_settings(&admission).await.unwrap(),
                combined_classic
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
            std::fs::remove_file(pending).unwrap();
            let file = admission.state_dir.join("ui-preferences.json");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(restore_settings(&admission).await.is_err());
            assert_eq!(
                settings_or_defaults(&admission).await.unwrap_err(),
                "display settings unavailable or unsafe"
            );
            assert!(
                save_settings(&admission, Preferences::default())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn an_unconfirmed_settings_replacement_is_not_reported_as_kept() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let admission = Admission {
            roster: None,
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            workspace_identity: arany::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        arany::FaultPlan::fail_record_sync(admission.state_dir.clone(), "ui-preferences.json");
        let unconfirmed = save_settings(
            &admission,
            Preferences {
                newline: NewlineShortcut::AltEnter,
                ..Preferences::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(unconfirmed, SaveError::Unconfirmed);
        assert!(!save_failure_notice(&unconfirmed).contains("kept"));
        assert!(
            save_failure_notice(&SaveError::Failed("could not save display settings"))
                .ends_with("current settings kept")
        );
    }

    #[tokio::test]
    async fn idle_pause_reports_that_no_goal_is_running() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let admission = Admission {
            roster: None,
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            workspace_identity: arany::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let root = StateRoot::admit(&admission.state_dir).expect("state");
        let mut session_id = create_session(root, admission.workspace.clone(), None)
            .await
            .expect("Session");
        let mut view = load_view(&admission.state_dir, session_id)
            .await
            .expect("view");
        let notice = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Goal,
            Some("pause"),
        )
        .await
        .expect("local notice");
        assert_eq!(notice, "No Goal is running; /goal pause works during a Run");
    }

    #[tokio::test]
    async fn lifecycle_commands_switch_only_after_a_valid_durable_operation() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let mut admission = Admission {
            roster: None,
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace,
            workspace_identity: arany::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let root = StateRoot::admit(&admission.state_dir).expect("state");
        let original_id = create_session(root, admission.workspace.clone(), None)
            .await
            .expect("original Session");
        let mut session_id = original_id;
        let mut view = load_view(&admission.state_dir, session_id)
            .await
            .expect("original view");

        let invalid = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Resume,
            Some("not-an-id"),
        )
        .await
        .expect("local notice");
        assert_eq!(invalid, "Usage: /resume SESSION_ID");
        assert_eq!(session_id, original_id);

        let missing = SessionId::new().to_string();
        let original_title = view.title.clone();
        for command in [InteractiveCommand::Resume, InteractiveCommand::Fork] {
            let rejected = handle_command(
                &admission,
                &mut session_id,
                &mut view,
                command,
                Some(&missing),
            )
            .await
            .expect("recoverable missing Session");
            assert_eq!(rejected, "Error: Session not found");
            assert_eq!(session_id, original_id);
            assert_eq!(view.title, original_title);
        }
        let rejected_rename = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Rename,
            Some(""),
        )
        .await
        .expect("recoverable canonical title rejection");
        assert_eq!(rejected_rename, "Error: invalid Run request");
        assert_eq!(session_id, original_id);
        assert_eq!(view.title, original_title);

        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Rename,
            Some("Renamed"),
        )
        .await
        .expect("rename notice");
        assert_eq!(view.title, "Renamed");

        for (command, argument) in [
            (InteractiveCommand::Provider, "openai"),
            (InteractiveCommand::Model, "gpt-5.4"),
            (InteractiveCommand::Model, "gpt-5.4 medium"),
        ] {
            handle_command(
                &admission,
                &mut session_id,
                &mut view,
                command,
                Some(argument),
            )
            .await
            .expect("selected next-Run setting");
        }
        assert_eq!(view.defaults.effort, Some(Effort::Medium));
        let invalid_effort = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Model,
            Some("gpt-5.4 max"),
        )
        .await
        .expect("unavailable effort notice");
        assert_eq!(
            invalid_effort,
            "Error: invalid native model/effort; choose a model and effort with /model"
        );
        assert_eq!(view.defaults.effort, Some(Effort::Medium));
        let unreviewed_model = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Model,
            Some("gpt-5.4-mini"),
        )
        .await
        .expect("unreviewed model notice");
        assert_eq!(unreviewed_model, "Model: gpt-5.4-mini · low");
        assert_eq!(view.defaults.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(view.defaults.effort, Some(Effort::Low));
        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Provider,
            Some("anthropic"),
        )
        .await
        .expect("provider change");
        assert_eq!(view.defaults.effort, None);

        {
            use std::os::unix::fs::PermissionsExt;
            let file = admission.state_dir.join("provider-profiles.json");
            let profile = serde_json::json!({
                "version": arany::test_support::record_version(arany::RecordKind::ProviderProfiles),
                "profiles": [{
                    "name": "local",
                    "protocol": "openai-responses",
                    "endpoint": "http://127.0.0.1:9321/v1/responses",
                    "model": "model-1",
                    "credential_env": "ARANY_PROVIDER_LOCAL_KEY",
                    "outcome_encoding": "json_schema",
                    "privacy": "user_authorized",
                    "max_output_tokens": 4096,
                    "efforts": ["low", "high"]
                }]
            });
            std::fs::write(&file, serde_json::to_vec(&profile).unwrap()).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            for (command, argument) in [
                (InteractiveCommand::Provider, "custom:local"),
                (InteractiveCommand::Model, "model-1"),
                (InteractiveCommand::Model, "model-1 high"),
            ] {
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    command,
                    Some(argument),
                )
                .await
                .expect("custom next-Run selection");
            }
            assert_eq!(view.defaults.effort, Some(Effort::High));
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 high"),
                )
                .await
                .unwrap(),
                "Model: model-1 · high"
            );
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 max"),
                )
                .await
                .unwrap(),
                "Error: Effort unavailable for the selected Provider/model"
            );
            assert_eq!(view.defaults.effort, Some(Effort::High));
        }

        let cannot_fork = handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Fork,
            None,
        )
        .await
        .expect("local fork notice");
        assert_eq!(
            cannot_fork,
            "Error: Session has no committed Run boundary to fork"
        );
        assert_eq!(session_id, original_id);

        {
            use rustix::fs::{FlockOperation, flock};
            let held = std::fs::File::open(
                admission
                    .state_dir
                    .join(format!("session-{session_id}.lock")),
            )
            .expect("existing Session operation lock");
            flock(&held, FlockOperation::NonBlockingLockExclusive).expect("held operation");
            let before = view.clone();
            let store = Store::open_read_only(
                StateRoot::open_existing(&admission.state_dir).expect("State"),
            )
            .expect("source Store");
            let prefix = store.load_session(session_id).await.expect("source prefix");
            store.close().await.expect("close source Store");
            for (command, argument) in [
                (InteractiveCommand::Provider, "anthropic"),
                (InteractiveCommand::Model, "model-1 high"),
                (InteractiveCommand::Model, "model-1 default"),
                (InteractiveCommand::Model, "model-1 low"),
                (InteractiveCommand::Agents, "single"),
            ] {
                let notice = handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    command,
                    Some(argument),
                )
                .await
                .expect("recoverable busy selection");
                assert_eq!(
                    notice,
                    "Error: Session has an active operation; selection unchanged. Try again when it finishes"
                );
                assert_eq!(view, before);
                assert_eq!(session_id, original_id);
                let store = Store::open_read_only(
                    StateRoot::open_existing(&admission.state_dir).expect("State"),
                )
                .expect("rejected selection Store");
                assert_eq!(
                    store
                        .load_session(session_id)
                        .await
                        .expect("unchanged prefix"),
                    prefix
                );
                store.close().await.expect("close rejection Store");
            }
            flock(&held, FlockOperation::Unlock).expect("release operation");
            let workspace = admission.workspace.clone();
            admission.workspace = temp.path().join("other-workspace");
            std::fs::create_dir(&admission.workspace).expect("other Workspace");
            assert!(matches!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 low"),
                )
                .await
                .expect_err("Workspace failure stays fatal"),
                LoopError::Fatal(error) if error == "Session belongs to a different Workspace"
            ));
            assert_eq!(view, before);
            admission.workspace = workspace;
            assert_eq!(
                handle_command(
                    &admission,
                    &mut session_id,
                    &mut view,
                    InteractiveCommand::Model,
                    Some("model-1 low"),
                )
                .await
                .expect("explicit choice after unlock"),
                "Model: model-1 · low"
            );
            assert_eq!(view.defaults.effort, Some(Effort::Low));
        }

        let store = Store::open(StateRoot::open_existing(&admission.state_dir).expect("State"))
            .expect("interrupted source Store");
        let goal_id = arany::GoalId::new();
        let run_id = RunId::new();
        let workspace_identity = view.workspace_identity.expect("Session Workspace identity");
        store
            .append(
                original_id,
                arany::Event::GoalStarted {
                    include_paths: Some(Vec::new()),
                    goal_id,
                    objective: "Prior accepted objective".into(),
                    workspace_identity,
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
            )
            .await
            .expect("start interrupted source Goal");
        store
            .append(
                original_id,
                arany::Event::GoalRunLinked {
                    goal_id,
                    run_id,
                    phase: arany::GoalPhase::Implementation,
                },
            )
            .await
            .expect("link interrupted source Run");
        store
            .append(
                original_id,
                arany::Event::MessageAccepted {
                    skill: None,
                    run_id,
                    text: "Prior accepted objective".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("accepted source message");
        store.close().await.expect("close interrupted source Store");
        view = load_view(&admission.state_dir, original_id)
            .await
            .expect("interrupted source");
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        let current_defaults = view.defaults.clone();
        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::New,
            None,
        )
        .await
        .expect("new Session notice");
        assert_ne!(session_id, original_id);
        assert_eq!(view.defaults, current_defaults);
        assert_eq!(view.title, "New Session");
        assert!(view.runs.is_empty());

        handle_command(
            &admission,
            &mut session_id,
            &mut view,
            InteractiveCommand::Resume,
            Some(&original_id.to_string()),
        )
        .await
        .expect("resume notice");
        assert_eq!(session_id, original_id);
        assert_eq!(view.title, "Renamed");
        assert_eq!(view.runs[0].objective, "Prior accepted objective");

        admission.defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: Some(Effort::Low),
            account_id: None,
            policy: CollaborationPolicy::Single,
        };
        for (index, defaults) in [
            SessionDefaults::default(),
            SessionDefaults {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-5".into()),
                effort: Some(Effort::High),
                account_id: Some(uuid::Uuid::now_v7()),
                policy: CollaborationPolicy::Team {
                    max_active_children: 2,
                },
            },
            SessionDefaults {
                provider: Some("chatgpt".into()),
                model: None,
                effort: None,
                account_id: Some(uuid::Uuid::now_v7()),
                policy: CollaborationPolicy::Auto {
                    max_active_children: 1,
                },
            },
            current_defaults,
        ]
        .into_iter()
        .enumerate()
        {
            persist_defaults(&admission, session_id, defaults.clone())
                .await
                .expect("current selection");
            view = load_view(&admission.state_dir, session_id)
                .await
                .expect("current view");
            let source_id = session_id;
            let source_view = view.clone();
            let store = Store::open_read_only(
                StateRoot::open_existing(&admission.state_dir).expect("State"),
            )
            .expect("source Store");
            let source_events = store.load_session(source_id).await.expect("source prefix");
            store.close().await.expect("close source Store");
            let notice = handle_command(
                &admission,
                &mut session_id,
                &mut view,
                InteractiveCommand::New,
                None,
            )
            .await
            .expect("fresh conversation");
            assert_ne!(session_id, source_id);
            assert_eq!(notice, "New conversation · send a message to save it");
            assert_eq!(view.title, "New Session");
            assert_eq!(view.defaults, defaults);
            assert!(view.runs.is_empty() && view.compactions.is_empty());
            let store = Store::open_read_only(
                StateRoot::open_existing(&admission.state_dir).expect("State"),
            )
            .expect("closed replay Store");
            assert_eq!(
                store
                    .load_session(source_id)
                    .await
                    .expect("unchanged prefix"),
                source_events
            );
            let reopened_source = store
                .load_view(source_id)
                .await
                .expect("source replay")
                .expect("source Session");
            assert_eq!(reopened_source.title, source_view.title);
            assert_eq!(reopened_source.defaults, source_view.defaults);
            let events = store.load_session(session_id).await.expect("new prefix");
            assert!(events.is_empty(), "new/clear must leave no empty history");
            assert!(
                store
                    .load_view(session_id)
                    .await
                    .expect("unsaved lookup")
                    .is_none()
            );
            store.close().await.expect("close replay Store");
            let title = [
                "Local title",
                "New Session",
                "Forked Session",
                "Local title",
            ][index];
            handle_command(
                &admission,
                &mut session_id,
                &mut view,
                InteractiveCommand::Rename,
                Some(title),
            )
            .await
            .expect("local title");
            assert_eq!(view.created_sequence, 0);
            super::super::materialize_session(&admission, &mut session_id, &mut view)
                .await
                .expect("first submission");
            let reopened = load_view(&admission.state_dir, session_id)
                .await
                .expect("new replay");
            assert_eq!(reopened.defaults, defaults);
            assert_eq!(reopened.title, title);
            assert!(reopened.title_is_explicit);
            assert!(reopened.runs.is_empty() && reopened.compactions.is_empty());
        }
    }
}
