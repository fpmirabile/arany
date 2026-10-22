use super::catalog::{Family, Site};
use super::config::Config;
use super::permissions::HiddenPaths;
use super::toolchains::{RuntimeGrant, validate_selection};
use super::types::*;
use super::{Cancellation, ToolError, ToolRuntime};
use crate::store::{BuildCacheIdentity, BuildCacheLease, StoreError};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::{Child, Command};

const CAPTURE_BYTES: usize = 48 * 1024;
pub(super) const TASK_BYTES_CAP: usize = 1024 * 1024;
// Only a task carrying one base64 agent definition file may exceed the common cap.
const DEFINITION_TASK_BYTES: usize = (64 * 1024_usize).div_ceil(3) * 4;
pub(super) const RUNTIME_ROOT: &str = "/usr";
// Loaded CI hosts stall teardown for seconds; one shared bound only widens the wait for termination evidence.
pub(super) const TEARDOWN_WAIT: Duration = Duration::from_secs(10);
const PAYLOAD_KILL_WAIT: Duration = Duration::from_secs(1);
// The launcher measures its runtime from GO, so the grace covers its kill wait, the report write and GO delivery.
#[cfg(any(target_os = "macos", test))]
const PAYLOAD_REPORT_GRACE: Duration = PAYLOAD_KILL_WAIT.saturating_add(Duration::from_secs(1));

#[cfg(any(target_os = "macos", test))]
pub(super) fn payload_report_deadline(
    go: tokio::time::Instant,
    runtime_ms: u32,
) -> tokio::time::Instant {
    go + Duration::from_millis(u64::from(runtime_ms)) + PAYLOAD_REPORT_GRACE
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod landlock;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod profile;
#[cfg(target_os = "linux")]
pub(super) use landlock::spawn_confined;

pub(super) fn git_writes_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        landlock::available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::fixture::{Span as FixtureSpan, Stage as FixtureStage};
#[cfg(target_os = "macos")]
pub(super) use macos::{
    RUNTIME_COMPAT_BYTES, RUNTIME_COMPAT_FILE, SystemPrograms, macos_enforcement_digest,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod admission;

pub(super) async fn recover_private(ids: &[uuid::Uuid]) -> Result<(), ToolError> {
    check_available()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return admission::recover_private(ids).await;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = ids;
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn runtime_roots() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["/System/Library", "/usr/lib", "/bin", "/usr/bin"]
    } else {
        &[RUNTIME_ROOT]
    }
}

pub(super) fn native_system_program(path: &str) -> Result<bool, ToolError> {
    #[cfg(target_os = "macos")]
    {
        macos::system_program(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
pub(super) use linux::RESERVATION_OVERHEAD;
#[cfg(target_os = "macos")]
pub(super) use macos::RESERVATION_OVERHEAD;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    config: Config,
    intent: EffectIntent,
    cache: Option<BuildCacheTransfer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hook_stdin: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    definition: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    worker: Option<super::worker::Transfer>,
    #[serde(deserialize_with = "Option::deserialize")]
    handover: Option<super::worker::PublicationTransfer>,
    #[serde(default, skip_serializing_if = "HiddenPaths::is_empty")]
    hidden: HiddenPaths,
    #[cfg(target_os = "linux")]
    parent_namespaces: profile::Namespaces,
    #[cfg(target_os = "macos")]
    workspace: std::path::PathBuf,
    #[cfg(target_os = "macos")]
    guard_executable: std::path::PathBuf,
    #[cfg(target_os = "macos")]
    system_programs: Option<SystemPrograms>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BuildCacheTransfer {
    pub(super) identity: BuildCacheIdentity,
    pub(super) exclusions: Vec<String>,
    pub(super) view_bytes: u64,
    pub(super) candidate_bytes: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum CacheCandidate {
    Received,
    Rejected,
    Failed,
    Uncertain,
}

pub(super) fn cache_transfer_deadline(
    intent: &EffectIntent,
    ceiling: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.limits.cache_transfer_ms));
    ceiling.map_or(deadline, |ceiling| deadline.min(ceiling))
}

impl BuildCacheTransfer {
    fn validate(&self, intent: &EffectIntent, config: &Config) -> Result<(), ToolError> {
        let expected = cache_exclusions(&config.runtime_grants)?;
        if !matches!(intent.call, ToolCall::Command { .. })
            || self.identity.device != intent.workspace_device
            || self.identity.inode != intent.workspace_inode
            || self.view_bytes > intent.limits.cache_bytes
            || self.candidate_bytes > intent.limits.cache_candidate_bytes
            || self.candidate_bytes > self.view_bytes
            || self.exclusions != expected
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }
}

pub(super) struct BuildCacheSession {
    pub(super) lease: Arc<BuildCacheLease>,
    pub(super) transfer: BuildCacheTransfer,
}

pub(super) fn build_cache_session(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
) -> Result<Option<BuildCacheSession>, ToolError> {
    if !matches!(intent.call, ToolCall::Command { .. }) {
        return Ok(None);
    }
    if intent.limits.cache_bytes == 0 {
        return Ok(None);
    }
    let Some(state) = runtime.cache_state.as_ref() else {
        if matches!(
            intent.target,
            crate::session::worker::EffectTarget::WorkerView { .. }
        ) {
            return Err(ToolError::Configuration);
        }
        return Ok(None);
    };
    let identity = BuildCacheIdentity::new(
        &runtime.workspace,
        intent.workspace_device,
        intent.workspace_inode,
    )
    .map_err(map_cache_error)?;
    let exclusions = cache_exclusions(&runtime.config.runtime_grants)?;
    if let crate::session::worker::EffectTarget::WorkerView {
        cache_generation, ..
    } = intent.target
    {
        let lease = state
            .read_build_cache(identity.clone(), intent.limits.cache_bytes)
            .map_err(map_cache_error)?;
        if lease.generation().map_err(map_cache_error)? != cache_generation {
            return Err(ToolError::ChangedInput);
        }
        return Ok(Some(BuildCacheSession {
            lease: Arc::new(lease),
            transfer: BuildCacheTransfer {
                identity,
                exclusions,
                view_bytes: intent.limits.cache_bytes,
                candidate_bytes: 0,
            },
        }));
    }
    let lease = match state.begin_build_cache_with_limits(
        identity.clone(),
        intent.limits.cache_bytes,
        intent.limits.cache_candidate_bytes,
    ) {
        Ok(lease) => lease,
        Err(StoreError::StorageFull) if intent.limits.cache_candidate_bytes > 0 => {
            match state.begin_build_cache_with_limits(
                identity.clone(),
                intent.limits.cache_bytes,
                0,
            ) {
                Ok(lease) => lease,
                Err(StoreError::StorageFull) => return Ok(None),
                Err(error) => return Err(map_cache_error(error)),
            }
        }
        Err(StoreError::StorageFull) => return Ok(None),
        Err(error) => return Err(map_cache_error(error)),
    };
    let candidate_bytes = lease.candidate_limit();
    Ok(Some(BuildCacheSession {
        lease: Arc::new(lease),
        transfer: BuildCacheTransfer {
            identity,
            exclusions,
            view_bytes: intent.limits.cache_bytes,
            candidate_bytes,
        },
    }))
}

fn cache_exclusions(grants: &[RuntimeGrant]) -> Result<Vec<String>, ToolError> {
    let mut exclusions = Vec::new();
    for grant in grants.iter().filter(|grant| grant.kind.is_cache()) {
        let path = grant
            .kind
            .sandbox_cache_path()
            .ok_or(ToolError::Configuration)?;
        exclusions.push(path.to_owned());
    }
    exclusions.sort();
    exclusions.dedup();
    Ok(exclusions)
}

pub(super) fn map_cache_error(error: StoreError) -> ToolError {
    match error {
        StoreError::StorageFull => ToolError::Limit,
        StoreError::BuildCacheBusy => ToolError::QueueFull,
        StoreError::StateNotPrivate | StoreError::InvalidBuildCache => {
            ToolError::ProtectionUnavailable
        }
        _ => ToolError::Operation,
    }
}

pub(super) async fn publish_cache_candidate(
    observation: &mut ToolObservation,
    candidate: CacheCandidate,
    cache: Option<BuildCacheSession>,
    deadline: tokio::time::Instant,
) {
    if observation.disposition != ToolDisposition::Succeeded
        || matches!(
            observation.intent.target,
            crate::session::worker::EffectTarget::WorkerView { .. }
        )
    {
        return;
    }
    let update = match (candidate, cache) {
        (CacheCandidate::Uncertain, _) => CacheUpdate::Uncertain,
        (CacheCandidate::Rejected, _) => CacheUpdate::Rejected,
        (CacheCandidate::Failed, _) => CacheUpdate::Failed,
        (CacheCandidate::Received, None) if !observation.intent.call.is_process() => {
            CacheUpdate::NotApplicable
        }
        (CacheCandidate::Received, None) if observation.intent.limits.cache_requested_bytes > 0 => {
            CacheUpdate::Rejected
        }
        (CacheCandidate::Received, None) => CacheUpdate::NotApplicable,
        (CacheCandidate::Received, Some(cache)) => {
            let publication = tokio::task::spawn_blocking(move || {
                let Ok(mut lease) = Arc::try_unwrap(cache.lease) else {
                    return CacheUpdate::Uncertain;
                };
                let mut candidate = match lease.open_candidate() {
                    Ok(candidate) => candidate,
                    Err(StoreError::StorageFull) if !lease.publication_uncertain() => {
                        return CacheUpdate::Rejected;
                    }
                    Err(_) if lease.publication_uncertain() => return CacheUpdate::Uncertain,
                    Err(_) => return CacheUpdate::Failed,
                };
                match lease.publish(&mut candidate, &cache.transfer.exclusions) {
                    Ok(()) => CacheUpdate::Published,
                    Err(StoreError::StorageFull) if !lease.publication_uncertain() => {
                        CacheUpdate::Rejected
                    }
                    Err(_) if lease.publication_uncertain() => CacheUpdate::Uncertain,
                    Err(_) => CacheUpdate::Failed,
                }
            });
            match tokio::time::timeout_at(deadline, publication).await {
                Ok(Ok(update)) => update,
                Ok(Err(_)) | Err(_) => CacheUpdate::Uncertain,
            }
        }
    };
    observation.cache_update = update;
    if !observation.valid() {
        observation.cache_update = CacheUpdate::Uncertain;
        observation.disposition = ToolDisposition::Uncertain;
        observation.limit_cause = None;
    }
    debug_assert!(observation.valid());
}

impl Task {
    fn enforcement_digest(&self) -> Result<[u8; 32], ToolError> {
        #[cfg(target_os = "macos")]
        if !self.intent.call.is_process() && self.system_programs.is_some() {
            return Err(ToolError::Configuration);
        }
        intent_enforcement_digest(
            self.intent.call.is_process(),
            &self.config.runtime_grants,
            #[cfg(target_os = "macos")]
            self.system_programs.as_ref(),
        )
    }
}

fn intent_enforcement_digest(
    is_process: bool,
    runtime_grants: &[RuntimeGrant],
    #[cfg(target_os = "macos")] programs: Option<&SystemPrograms>,
) -> Result<[u8; 32], ToolError> {
    #[cfg(target_os = "macos")]
    {
        if is_process {
            macos_runtime_enforcement_digest(programs, runtime_grants)
        } else {
            enforcement_digest(&[])
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        enforcement_digest(if is_process { runtime_grants } else { &[] })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    digest: [u8; 32],
    pid: u32,
    #[cfg(target_os = "linux")]
    namespaces: profile::Namespaces,
    profile_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GuardCheck {
    Inspection,
    ListingUnavailable,
    DeviceUnlisted,
    DetachFailed,
    ImageStillListed,
    MountStillPresent,
    WorkerClosed,
    WorkerPanicked,
    ReportMalformed,
    ReportUnattested,
    ReportTrailing,
    Cancelled,
    ControllerFailed,
}

impl GuardCheck {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Inspection => "inspection",
            Self::ListingUnavailable => "listing unavailable",
            Self::DeviceUnlisted => "device unlisted before detach",
            Self::DetachFailed => "detach failed",
            Self::ImageStillListed => "image still listed",
            Self::MountStillPresent => "mount still present",
            Self::WorkerClosed => "worker closed before its report",
            Self::WorkerPanicked => "worker panicked",
            Self::ReportMalformed => "malformed report",
            Self::ReportUnattested => "unattested report",
            Self::ReportTrailing => "report not followed by end of stream",
            Self::Cancelled => "cancelled",
            Self::ControllerFailed => "controller thread failed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BootstrapStage {
    Admission,
    Workspace,
    RuntimeMounts,
    Capabilities,
    Handshake,
    Payload,
    Receipt,
    ProcessStop,
    LauncherReap,
    StderrDrain,
    StorageCleanup,
}

impl BootstrapStage {
    fn label(&self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Workspace => "Workspace identity",
            Self::RuntimeMounts => "runtime mount topology",
            Self::Capabilities => "kernel capability",
            Self::Handshake => "handshake",
            Self::Payload => "payload",
            Self::Receipt => "receipt",
            Self::ProcessStop => "process stop",
            Self::LauncherReap => "launcher reap",
            Self::StderrDrain => "stderr drain",
            Self::StorageCleanup => "storage cleanup",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapFailure {
    stage: BootstrapStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    check: Option<GuardCheck>,
    // Local diagnostics only: never serialized, journaled or shown to the model.
    #[serde(skip)]
    detail: Option<String>,
}

impl std::fmt::Display for BootstrapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.stage.label())
    }
}

impl BootstrapFailure {
    fn at(stage: BootstrapStage) -> Self {
        Self {
            stage,
            check: None,
            detail: None,
        }
    }

    pub(super) fn sandbox_setup(&self) -> bool {
        match self.stage {
            BootstrapStage::RuntimeMounts
            | BootstrapStage::Capabilities
            | BootstrapStage::Handshake => true,
            BootstrapStage::Admission
            | BootstrapStage::Workspace
            | BootstrapStage::Payload
            | BootstrapStage::Receipt
            | BootstrapStage::ProcessStop
            | BootstrapStage::LauncherReap
            | BootstrapStage::StderrDrain
            | BootstrapStage::StorageCleanup => false,
        }
    }

    pub(super) fn check(&self) -> Option<&'static str> {
        self.check.map(GuardCheck::label)
    }

    pub(super) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub(super) fn failure_stage(&self) -> &'static str {
        match self.stage {
            BootstrapStage::Admission => "admission",
            BootstrapStage::Workspace => "pinned input",
            BootstrapStage::RuntimeMounts => "runtime mount topology",
            BootstrapStage::Capabilities => "native protection",
            BootstrapStage::Handshake => "handshake",
            BootstrapStage::Payload => "execution",
            BootstrapStage::Receipt => "receipt",
            BootstrapStage::ProcessStop => "process cleanup",
            BootstrapStage::LauncherReap => "launcher reap",
            BootstrapStage::StderrDrain => "output EOF",
            BootstrapStage::StorageCleanup => "storage cleanup",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardStage {
    ConnectionDeadline,
    PreparationDeadline,
    ResourceAttestation,
    ProcessCleanup,
    StorageCleanup,
    LauncherReap,
    OutputEof,
    NamespaceBootstrap,
    NativeManager,
    ControlProtocol,
}

impl GuardStage {
    pub(super) fn failure_stage(self) -> &'static str {
        match self {
            Self::ConnectionDeadline => "connection deadline",
            Self::PreparationDeadline => "preparation deadline",
            Self::ResourceAttestation => "resource attestation",
            Self::ProcessCleanup => "process cleanup",
            Self::StorageCleanup => "storage cleanup",
            Self::LauncherReap => "launcher reap",
            Self::OutputEof => "output EOF",
            Self::NamespaceBootstrap => "namespace bootstrap",
            Self::NativeManager => "native manager",
            Self::ControlProtocol => "control protocol",
        }
    }
}

impl std::fmt::Display for GuardStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.failure_stage())
    }
}

pub(super) fn check_available() -> Result<(), ToolError> {
    #[cfg(target_os = "linux")]
    {
        linux::check_available()
    }
    #[cfg(target_os = "macos")]
    {
        macos::check_available()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn enforcement_digest(runtime_grants: &[RuntimeGrant]) -> Result<[u8; 32], ToolError> {
    #[cfg(target_os = "linux")]
    {
        bind_runtime_grants(linux::enforcement_digest()?, runtime_grants)
    }
    #[cfg(target_os = "macos")]
    {
        bind_runtime_grants(macos::enforcement_digest()?, runtime_grants)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

#[cfg(target_os = "macos")]
pub(super) fn macos_runtime_enforcement_digest(
    programs: Option<&SystemPrograms>,
    runtime_grants: &[RuntimeGrant],
) -> Result<[u8; 32], ToolError> {
    bind_runtime_grants(macos_enforcement_digest(programs)?, runtime_grants)
}

fn bind_runtime_grants(
    profile: [u8; 32],
    runtime_grants: &[RuntimeGrant],
) -> Result<[u8; 32], ToolError> {
    validate_selection(runtime_grants)?;
    if runtime_grants.is_empty() {
        return Ok(profile);
    }
    let mut bytes = b"arany-runtime-profile-v2:metadata-detected;exact-versioned-roots;explicit-read-only-cache-grants;bounded-output-may-disclose;no-automatic-cache-load;no-parent-cache-config-or-credentials".to_vec();
    bytes.extend_from_slice(&profile);
    bytes.extend_from_slice(
        &serde_json::to_vec(runtime_grants).map_err(|_| ToolError::Configuration)?,
    );
    Ok(digest(&bytes))
}

fn verify_runtime_grants(
    config: &Config,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] cache_enabled: bool,
) -> Result<(), ToolError> {
    for grant in &config.runtime_grants {
        #[cfg(target_os = "linux")]
        grant.verify_at(&grant.mounted_path(cache_enabled)?)?;
        #[cfg(target_os = "macos")]
        grant.verify_host()?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn verify_intent_runtime_grants(intent: &EffectIntent, config: &Config) -> Result<(), ToolError> {
    if intent.call.is_process() {
        verify_runtime_grants(config, intent.limits.cache_bytes > 0)
    } else {
        Ok(())
    }
}

fn attest_runtime_mounts(
    call: &ToolCall,
    grants: &[RuntimeGrant],
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] cache_enabled: bool,
    stage: &mut BootstrapStage,
) -> Result<(), ToolError> {
    if !call.is_process() {
        return Ok(());
    }
    *stage = BootstrapStage::RuntimeMounts;
    #[cfg(target_os = "linux")]
    linux::verify_runtime_mounts(grants, cache_enabled)?;
    #[cfg(target_os = "macos")]
    macos::verify_runtime_mounts(grants)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(ToolError::ProtectionUnavailable);
    Ok(())
}

// Whether the complete Guard task of this intent encodes within the task ceiling.
pub(super) fn task_fits(runtime: &ToolRuntime, intent: &EffectIntent) -> bool {
    #[cfg(target_os = "linux")]
    {
        profile::Namespaces::read().is_ok_and(|parent_namespaces| {
            encode_task(&Task {
                config: runtime.config.clone(),
                intent: intent.clone(),
                cache: None,
                hook_stdin: runtime.hook_stdin.as_deref().map(str::to_owned),
                hidden: runtime.hidden_paths(&intent.call),
                definition: None,
                worker: runtime.worker.as_ref().map(|a| a.transfer.clone()),
                handover: runtime
                    .handover
                    .as_ref()
                    .and_then(|_| super::worker::handover_ref(intent)),
                parent_namespaces,
            })
            .is_ok()
        })
    }
    #[cfg(target_os = "macos")]
    {
        Task::new(runtime, intent, None)
            .and_then(|task| encode_task(&task))
            .is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (runtime, intent);
        false
    }
}

pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    cancellation: Cancellation,
) -> Result<ToolObservation, ToolError> {
    let mut dispatched = false;
    #[cfg(target_os = "macos")]
    let result = macos::execute_inner(runtime, intent, cancellation, &mut dispatched).await;
    #[cfg(target_os = "linux")]
    let result = Box::pin(linux::execute_inner(
        runtime,
        intent,
        cancellation,
        &mut dispatched,
    ))
    .await;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = Err(ToolError::ProtectionUnavailable);
    // Unless cleanup failed the unit is stopped, so leftover Git locks are reported, never moved.
    let stale = if dispatched
        && intent.call.is_git()
        && intent.call.mutates()
        && !matches!(
            result,
            Err(ToolError::GuardRejected(GuardStage::ProcessCleanup))
        ) {
        match super::git::recover_locks(&runtime.workspace, intent.id) {
            Ok(()) => None,
            Err(error @ ToolError::StaleGitLocks(_)) => Some(error.to_string()),
            Err(error) => {
                crate::diagnostics::tool_failure("Guard", &error);
                None
            }
        }
    } else {
        None
    };
    #[cfg(any(test, feature = "test-support"))]
    let result = if result.is_ok() && crate::fault::FaultPlan::guard_receipt_lost(intent.id) {
        Err(ToolError::Uncertain)
    } else {
        result
    };
    let result = dispatch_result(intent, dispatched, result);
    let Some(report) = stale else {
        return result;
    };
    let cache = super::guard_error_cache_update(&intent.call, runtime.cache_state.is_some());
    Ok(report_stale_locks(
        uncertain_after_stale(intent, result, cache),
        &report,
    ))
}

fn uncertain_after_stale(
    intent: &EffectIntent,
    result: Result<ToolObservation, ToolError>,
    cache: CacheUpdate,
) -> ToolObservation {
    match result {
        Ok(observation) if observation.disposition == ToolDisposition::Uncertain => observation,
        other => super::uncertain_observation(
            intent.clone(),
            other.err().unwrap_or(ToolError::Uncertain),
            cache,
        ),
    }
}

// The owner removes these by hand, so the report leads the journaled observation.
pub(super) fn report_stale_locks(
    mut observation: ToolObservation,
    report: &str,
) -> ToolObservation {
    observation.output = format!("{report}. {}", observation.output);
    observation
}

pub(super) fn dispatch_result(
    intent: &EffectIntent,
    dispatched: bool,
    result: Result<ToolObservation, ToolError>,
) -> Result<ToolObservation, ToolError> {
    match result {
        Err(error) if !dispatched => Ok(super::unstarted_observation(intent.clone(), error)),
        result => result,
    }
}

pub(super) async fn drain_stderr(
    task: &mut tokio::task::JoinHandle<Result<Vec<u8>, ToolError>>,
    deadline: tokio::time::Instant,
) -> Option<Vec<u8>> {
    match tokio::time::timeout_at(deadline, &mut *task).await {
        Ok(Ok(Ok(stderr))) => Some(stderr),
        Ok(_) => None,
        Err(_) => {
            task.abort();
            let _ = task.await;
            None
        }
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) async fn retry_transient<T, Fut: std::future::Future<Output = Result<T, ToolError>>>(
    attempts: usize,
    pause: Duration,
    deadline: tokio::time::Instant,
    mut operation: impl FnMut() -> Fut,
) -> Result<T, ToolError> {
    let mut attempt = 1;
    loop {
        match operation().await {
            Err(error @ (ToolError::ProtectionUnavailable | ToolError::Operation))
                if attempt < attempts && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep_until(
                    deadline.min(tokio::time::Instant::now() + pause * attempt as u32),
                )
                .await;
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                attempt += 1;
            }
            result => return result,
        }
    }
}

// A device gone at the re-check may still be detaching; never detach it, and let a fresh attempt confirm.
#[cfg(any(target_os = "macos", test))]
pub(super) fn require_listed<T: PartialEq>(
    mut listed: impl Iterator<Item = T>,
    device: T,
) -> Result<(), ToolError> {
    if listed.any(|candidate| candidate == device) {
        Ok(())
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn bootstrap_failure(stderr: &[u8]) -> Option<ToolError> {
    serde_json::from_slice::<BootstrapFailure>(stderr)
        .ok()
        .map(ToolError::GuardBootstrap)
}

pub(super) async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut output = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| ToolError::Operation)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > cap {
            return Err(ToolError::Limit);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

pub(super) async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut line = Vec::new();
    loop {
        let part = reader.fill_buf().await.map_err(|_| ToolError::Operation)?;
        if part.is_empty() {
            return Err(ToolError::Operation);
        }
        let count = part
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(part.len(), |index| index + 1);
        let complete = part[count - 1] == b'\n';
        if line.len() + count > cap {
            return Err(ToolError::Limit);
        }
        line.extend_from_slice(&part[..count]);
        reader.consume(count);
        if complete {
            return Ok(line);
        }
    }
}

fn parse_ready(bytes: &[u8], intent: &EffectIntent) -> Result<Ready, ToolError> {
    let ready: Ready =
        serde_json::from_slice(bytes).map_err(|_| ToolError::ProtectionUnavailable)?;
    if ready.digest != intent.digest() || ready.profile_digest != intent.enforcement_digest {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(ready)
}

async fn read_observation<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<ToolObservation, ToolError> {
    read_report(reader, intent)
        .await
        .map_err(|rejected| rejected.0)
}

// A rejection keeps the error the reader has always returned and, where it is an Operation, a closed check.
async fn read_report<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<ToolObservation, (ToolError, Option<GuardCheck>)> {
    let tagged = |check| {
        move |error| match error {
            ToolError::Operation => (error, Some(check)),
            error => (error, None),
        }
    };
    let bytes = read_line_capped(reader, MAX_TOOL_OBSERVATION_BYTES)
        .await
        .map_err(tagged(GuardCheck::WorkerClosed))?;
    let observation = parse_observation(&bytes).map_err(tagged(GuardCheck::ReportMalformed))?;
    if observation.cache_update != CacheUpdate::NotApplicable || !observation.attested_for(intent) {
        return Err((ToolError::Operation, Some(GuardCheck::ReportUnattested)));
    }
    match read_capped(&mut *reader, 1).await {
        Ok(rest) if rest.is_empty() => Ok(observation),
        Ok(_) => Err((ToolError::Operation, Some(GuardCheck::ReportTrailing))),
        Err(error) => Err(tagged(GuardCheck::ReportTrailing)(error)),
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn execution_failure(check: GuardCheck) -> ToolError {
    ToolError::GuardBootstrap(BootstrapFailure {
        stage: BootstrapStage::Payload,
        check: Some(check),
        detail: None,
    })
}

#[cfg(any(target_os = "macos", test))]
const WORKER_STDERR_TAIL: u64 = 4096;
#[cfg(any(target_os = "macos", test))]
const WORKER_OUTPUT_BYTES: usize = 4096;

#[cfg(any(target_os = "macos", test))]
fn output_share(written: &std::sync::atomic::AtomicUsize, wanted: usize) -> usize {
    let previous = written.fetch_add(wanted, std::sync::atomic::Ordering::Relaxed);
    WORKER_OUTPUT_BYTES.saturating_sub(previous).min(wanted)
}

// The supervisor's only output is panic text; all of it together stays within one bounded share.
#[cfg(target_os = "macos")]
pub(super) fn bound_worker_output() {
    static WRITTEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    std::panic::set_hook(Box::new(|info| {
        let text = format!("{info}\n");
        let allowed = output_share(&WRITTEN, text.len());
        let _ = std::io::stderr().write_all(&text.as_bytes()[..allowed]);
    }));
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn read_tail(
    file: &mut (impl std::io::Read + std::io::Seek),
) -> std::io::Result<Vec<u8>> {
    let end = file.seek(std::io::SeekFrom::End(0))?;
    file.seek(std::io::SeekFrom::Start(
        end.saturating_sub(WORKER_STDERR_TAIL),
    ))?;
    let mut tail = Vec::new();
    std::io::Read::take(file, WORKER_STDERR_TAIL).read_to_end(&mut tail)?;
    Ok(tail)
}

// A worker that closed before its report may have left a panic on its own stderr.
#[cfg(any(target_os = "macos", test))]
pub(super) fn with_worker_exit(error: ToolError, stderr_tail: &[u8]) -> ToolError {
    match error {
        ToolError::GuardBootstrap(BootstrapFailure {
            stage: BootstrapStage::Payload,
            check: Some(GuardCheck::WorkerClosed),
            ..
        }) => {
            let detail = String::from_utf8_lossy(stderr_tail)
                .chars()
                .map(|ch| if ch.is_ascii_graphic() { ch } else { ' ' })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let check = if detail.contains("panicked at") {
                GuardCheck::WorkerPanicked
            } else {
                GuardCheck::WorkerClosed
            };
            ToolError::GuardBootstrap(BootstrapFailure {
                stage: BootstrapStage::Payload,
                check: Some(check),
                detail: (!detail.is_empty()).then_some(detail),
            })
        }
        error => error,
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) async fn read_worker_report<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<ToolObservation, ToolError> {
    read_report(reader, intent)
        .await
        .map_err(|(error, check)| check.map_or(error, execution_failure))
}

pub(super) async fn read_worker_observation<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    receipt_budget: Duration,
) -> Result<ToolObservation, ToolError> {
    let action = runtime.worker.as_ref().ok_or(ToolError::Configuration)?;
    let bytes = tokio::time::timeout(
        receipt_budget,
        read_line_capped(reader, MAX_TOOL_OBSERVATION_BYTES),
    )
    .await
    .map_err(|_| ToolError::Limit)??;
    let observation = parse_observation(&bytes)?;
    if !super::worker::wire_valid(&observation, intent) {
        return Err(ToolError::Operation);
    }
    tokio::time::timeout(super::worker::transfer_time(intent), async {
        let bytes = super::worker::receive_frame(reader).await?;
        let image = super::worker::Image::decode(&bytes, &action.transfer.grant)?;
        if observation.disposition == ToolDisposition::Succeeded
            && !matches!(intent.call, ToolCall::Command { .. })
        {
            let before = action.transfer.decode(&action.input)?;
            super::worker::validate_edit(&before, &image, &intent.call)?;
        }
        *action.output.lock().map_err(|_| ToolError::Operation)? = Some(bytes);
        if !read_capped(reader, 1).await?.is_empty() {
            return Err(ToolError::Operation);
        }
        Ok::<(), ToolError>(())
    })
    .await
    .map_err(|_| ToolError::Limit)??;
    Ok(observation)
}

fn parse_observation(bytes: &[u8]) -> Result<ToolObservation, ToolError> {
    serde_json::from_slice(bytes).map_err(|_| {
        serde_json::from_slice::<BootstrapFailure>(bytes)
            .map_or(ToolError::Operation, ToolError::GuardBootstrap)
    })
}

pub(super) async fn read_observation_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<ToolObservation, ToolError> {
    let bytes = read_line_capped(reader, MAX_TOOL_OBSERVATION_BYTES).await?;
    let observation = parse_observation(&bytes)?;
    if observation.cache_update != CacheUpdate::NotAttempted || !observation.attested_for(intent) {
        return Err(ToolError::Operation);
    }
    Ok(observation)
}

pub(super) async fn send_cache_archive<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    lease: &BuildCacheLease,
) -> Result<(), ToolError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some((file, size)) = lease.archive().map_err(map_cache_error)? else {
        writer
            .write_all(&0u64.to_le_bytes())
            .await
            .map_err(|_| ToolError::Operation)?;
        return Ok(());
    };
    if size > crate::tools::types::MAX_BUILD_CACHE_BYTES {
        return Err(ToolError::Limit);
    }
    let mut file = tokio::fs::File::from_std(file.into_std());
    writer
        .write_all(&size.to_le_bytes())
        .await
        .map_err(|_| ToolError::Operation)?;
    let mut remaining = size;
    let mut buffer = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let count = file
            .read(&mut buffer[..wanted])
            .await
            .map_err(|_| ToolError::Operation)?;
        if count == 0 {
            return Err(ToolError::Operation);
        }
        writer
            .write_all(&buffer[..count])
            .await
            .map_err(|_| ToolError::Operation)?;
        remaining -= count as u64;
    }
    Ok(())
}

pub(super) async fn receive_cache_candidate<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    lease: &Arc<BuildCacheLease>,
) -> Result<CacheCandidate, ToolError> {
    use tokio::io::AsyncReadExt;

    let marker = read_line_capped(reader, 32).await?;
    if marker == b"CACHE_REJECTED\n" {
        return Ok(CacheCandidate::Rejected);
    }
    if marker == b"CACHE_FAILED\n" {
        return Ok(CacheCandidate::Failed);
    }
    if marker != b"CACHE_CANDIDATE\n" {
        return Err(ToolError::Operation);
    }
    let mut length = [0; 8];
    reader
        .read_exact(&mut length)
        .await
        .map_err(|_| ToolError::Operation)?;
    let length = u64::from_le_bytes(length);
    if length == 0 || length > lease.candidate_limit() {
        return Err(ToolError::Limit);
    }
    let lease = Arc::clone(lease);
    let synced = drain_candidate(
        reader,
        length,
        move || {
            lease.candidate_writer().ok().map(|file| CandidateStaging {
                file,
                _lease: lease,
            })
        },
        |staged| {
            #[cfg(any(test, feature = "test-support"))]
            let completion = crate::fault::FaultPlan::candidate_sync(staged._lease.token());
            #[cfg(any(test, feature = "test-support"))]
            let admitted = completion.is_ok();
            #[cfg(not(any(test, feature = "test-support")))]
            let admitted = true;
            let synced = admitted && staged.file.sync_all().is_ok();
            drop(staged);
            #[cfg(any(test, feature = "test-support"))]
            if let Ok(Some(finished)) = completion {
                let _ = finished.send(());
            }
            synced
        },
    )
    .await?;
    Ok(if synced {
        CacheCandidate::Received
    } else {
        CacheCandidate::Failed
    })
}

// Drop the file before the lease can reclaim its reservation and identity lock.
struct CandidateStaging {
    file: cap_std::fs::File,
    _lease: Arc<BuildCacheLease>,
}

impl Write for CandidateStaging {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

// Keep draining after a staging write failure so the archive stream stays aligned.
async fn drain_candidate<R: tokio::io::AsyncRead + Unpin, W: Write + Send + 'static>(
    reader: &mut R,
    length: u64,
    stage: impl FnOnce() -> Option<W> + Send + 'static,
    finish: impl FnOnce(W) -> bool + Send + 'static,
) -> Result<bool, ToolError> {
    let (chunks, mut pending) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let writer = tokio::task::spawn_blocking(move || {
        let mut staged = stage();
        let mut remaining = length;
        let mut failed = false;
        while let Some(bytes) = pending.blocking_recv() {
            remaining -= bytes.len() as u64;
            if !failed && let Some(writer) = staged.as_mut() {
                failed = writer.write_all(&bytes).is_err();
            }
        }
        let Some(mut writer) = staged else {
            return false;
        };
        remaining == 0 && !failed && writer.flush().is_ok() && finish(writer)
    });
    let mut remaining = length;
    let mut read = Ok(());
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(32 * 1024)).unwrap_or(32 * 1024);
        let mut buffer = vec![0u8; wanted];
        if reader.read_exact(&mut buffer).await.is_err() || chunks.send(buffer).await.is_err() {
            read = Err(ToolError::Operation);
            break;
        }
        remaining -= wanted as u64;
    }
    drop(chunks);
    let staged = writer.await.map_err(|_| ToolError::Operation)?;
    read.map(|()| staged)
}

// Seatbelt blocks Tokio's signal wake socket, so macOS closes stdin and polls try_wait when reaping.
pub(super) async fn reap_owned_child(
    child: &mut Child,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "macos")]
    {
        drop(child.stdin.take());
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(macos::SUPERVISION_INTERVAL).await;
        }
    }
    #[cfg(not(target_os = "macos"))]
    child.wait().await
}

// Orphaning a group with a stopped member sends SIGHUP to every member, so the supervisor never shares one with its payload.
#[cfg(any(target_os = "macos", test))]
pub(super) fn launcher_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.process_group(0);
    command
}

pub(super) fn spawn_before(
    command: &mut Command,
    deadline: tokio::time::Instant,
) -> Result<Child, ToolError> {
    if tokio::time::Instant::now() >= deadline {
        return Err(ToolError::Timeout(String::new()));
    }
    command.spawn().map_err(|_| ToolError::Operation)
}

pub(super) async fn capture(
    mut child: Child,
    cap: usize,
    deadline: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), ToolError> {
    let out = child.stdout.take().ok_or(ToolError::Operation)?;
    let err = child.stderr.take().ok_or(ToolError::Operation)?;
    let result = tokio::time::timeout(deadline, async {
        tokio::try_join!(
            async {
                reap_owned_child(&mut child)
                    .await
                    .map_err(|_| ToolError::Operation)
            },
            read_capped(out, cap),
            read_capped(err, cap)
        )
    })
    .await;
    let error = match result {
        Ok(Ok(value)) => return Ok(value),
        Ok(Err(error)) => error,
        Err(_) => ToolError::Timeout(String::new()),
    };
    let _ = child.start_kill();
    let _ = tokio::time::timeout(PAYLOAD_KILL_WAIT, reap_owned_child(&mut child)).await;
    Err(error)
}

pub(super) async fn capture_command(
    mut child: Child,
    cap: usize,
    deadline: Duration,
) -> Result<
    (
        Option<std::process::ExitStatus>,
        super::output::Window,
        super::output::Window,
    ),
    ToolError,
> {
    let out = child.stdout.take().ok_or(ToolError::Operation)?;
    let err = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stdout = super::output::Window::new(cap);
    let mut stderr = super::output::Window::new(cap);
    let result = tokio::time::timeout(deadline, async {
        tokio::try_join!(
            async {
                reap_owned_child(&mut child)
                    .await
                    .map_err(|_| ToolError::Operation)
            },
            super::output::read_window_into(out, &mut stdout),
            super::output::read_window_into(err, &mut stderr)
        )
    })
    .await;
    if let Ok(Ok((status, (), ()))) = result {
        return Ok((Some(status), stdout, stderr));
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(PAYLOAD_KILL_WAIT, reap_owned_child(&mut child)).await;
    match result {
        Err(_) => Ok((None, stdout, stderr)),
        Ok(Err(error)) => Err(error),
        Ok(Ok(_)) => unreachable!(),
    }
}

fn task_cap(definition: bool) -> usize {
    TASK_BYTES_CAP + if definition { DEFINITION_TASK_BYTES } else { 0 }
}

fn encode_task(task: &Task) -> Result<Vec<u8>, ToolError> {
    let mut bytes = serde_json::to_vec(task).map_err(|_| ToolError::Configuration)?;
    if bytes.len() > task_cap(task.definition.is_some()) {
        return Err(ToolError::Limit);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_task(reader: &mut std::io::BufReader<impl Read>) -> Result<Task, ToolError> {
    let mut task = Vec::new();
    std::io::BufRead::read_until(
        &mut Read::take(&mut *reader, (task_cap(true) + 1) as u64),
        b'\n',
        &mut task,
    )
    .map_err(|_| ToolError::Operation)?;
    if task.last() != Some(&b'\n') {
        return Err(if task.len() > task_cap(true) {
            ToolError::Limit
        } else {
            ToolError::Operation
        });
    }
    if !reader.buffer().is_empty() {
        return Err(ToolError::Configuration);
    }
    let task_len = task.len();
    let task: Task = serde_json::from_slice(&task).map_err(|_| ToolError::Configuration)?;
    if task_len > task_cap(task.definition.is_some()) + 1 {
        return Err(ToolError::Limit);
    }
    Ok(task)
}

#[cfg(target_os = "macos")]
fn bootstrap_locations(
    task: &Task,
    stage: &mut BootstrapStage,
) -> Result<(std::path::PathBuf, std::path::PathBuf), ToolError> {
    *stage = BootstrapStage::Capabilities;
    let root = macos::install(task, stage)?;
    let workspace = if task.worker.is_some() {
        // A fresh HFS+ volume gains root metadata, so the image gets its own owner-only directory.
        let tree = root.join("workspace/tree");
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&tree)
            .map_err(|_| ToolError::Path)?;
        tree
    } else if task.intent.call.is_process() {
        root.join("workspace")
    } else {
        task.workspace.clone()
    };
    Ok((root, workspace))
}

#[cfg(not(target_os = "macos"))]
fn bootstrap_locations(
    _task: &Task,
    _stage: &mut BootstrapStage,
) -> Result<(std::path::PathBuf, std::path::PathBuf), ToolError> {
    Ok((
        Path::new("/").to_owned(),
        Path::new("/workspace").to_owned(),
    ))
}

pub(super) struct GuardContext<'a> {
    pub(super) intent: &'a EffectIntent,
    pub(super) config: &'a Config,
    pub(super) root: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) skill: Option<Vec<u8>>,
    pub(super) hook_stdin: Option<&'a str>,
    pub(super) hidden: &'a HiddenPaths,
    pub(super) definition: Option<Vec<u8>>,
    pub(super) handover: Option<crate::session::worker::WorkerChangeset>,
    runtime_started: Option<std::time::Instant>,
    #[cfg(target_os = "linux")]
    parent_namespaces: &'a profile::Namespaces,
}

impl GuardContext<'_> {
    pub(super) fn remaining_runtime(&self) -> Duration {
        runtime_after(
            self.runtime_started,
            self.intent.limits.runtime_ms,
            std::time::Instant::now(),
        )
    }
}

// Only macOS has a supervisor deadline counted from GO; elsewhere the full runtime starts at spawn.
fn runtime_start_at_go() -> Option<std::time::Instant> {
    cfg!(target_os = "macos").then(std::time::Instant::now)
}

fn runtime_after(
    started: Option<std::time::Instant>,
    runtime_ms: u32,
    now: std::time::Instant,
) -> Duration {
    let runtime = Duration::from_millis(u64::from(runtime_ms));
    started.map_or(runtime, |started| {
        (started + runtime).saturating_duration_since(now)
    })
}

#[cfg(target_os = "linux")]
impl GuardContext<'_> {
    pub(super) async fn stop_descendants(&self) -> Result<(), ToolError> {
        linux::stop_command_descendants(self.parent_namespaces).await
    }
}

fn validate_task(task: &Task) -> Result<(), ToolError> {
    task.config.validate()?;
    if let Some(cache) = &task.cache {
        cache.validate(&task.intent, &task.config)?;
    }
    verify_intent_runtime_grants(&task.intent, &task.config)?;
    if let Some(worker) = &task.worker {
        worker.validate(&task.intent, &task.config)?;
    }
    if task
        .handover
        .as_ref()
        .is_some_and(|data| Some(data.clone()) != super::worker::handover_ref(&task.intent))
        || task.worker.is_some() && task.handover.is_some()
    {
        return Err(ToolError::Configuration);
    }
    if task.worker.is_none()
        && task.intent.target != crate::session::worker::EffectTarget::Workspace
        || !task.intent.valid()
        || task.intent.call.facts().site != Site::Guard
        || task.intent.policy_digest != task.config.digest()
        || !(task.config.allows(&task.intent.call)
            || task.worker.is_some()
                && super::worker::handover_call(&task.intent.call, task.intent.agent_run_id)
            || task.handover.is_some() && task.config.write)
        || task.intent.enforcement_digest != task.enforcement_digest()?
        || task.intent.limits.resource_profile != ToolLimits::default().resource_profile
        || task.hook_stdin.as_ref().is_some_and(|stdin| {
            stdin.len() > super::permissions::MAX_HOOK_INPUT_BYTES
                || !matches!(task.intent.call, ToolCall::Command { .. })
        })
    {
        return Err(ToolError::Configuration);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_cache(
    cache: &BuildCacheTransfer,
    stdin: &mut impl Read,
    root: &Path,
) -> Result<(), ToolError> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(b"CACHE_READY\n")
        .and_then(|()| stdout.flush())
        .map_err(|_| ToolError::Operation)?;
    let mut length = [0; 8];
    stdin
        .read_exact(&mut length)
        .map_err(|_| ToolError::Operation)?;
    let length = u64::from_le_bytes(length);
    if length > cache.view_bytes {
        return Err(ToolError::Limit);
    }
    if length > 0 {
        let cache_dir = super::fs::open_directory(&root.join("cache"))?;
        let mut archive = Read::take(&mut *stdin, length);
        crate::store::restore_archive_with_limit(
            &mut archive,
            &cache_dir,
            &cache.identity,
            &cache.exclusions,
            cache.view_bytes,
        )
        .map_err(map_cache_error)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn send_cache_candidate(
    cache: &BuildCacheTransfer,
    disposition: ToolDisposition,
    stdout: &mut impl Write,
    root: &Path,
) -> Result<(), ToolError> {
    if disposition != ToolDisposition::Succeeded {
        stdout
            .write_all(b"CACHE_REJECTED\n")
            .and_then(|()| stdout.flush())
            .map_err(|_| ToolError::Operation)?;
    } else if cache.candidate_bytes == 0 {
        stdout
            .write_all(b"CACHE_REJECTED\n")
            .and_then(|()| stdout.flush())
            .map_err(|_| ToolError::Operation)?;
    } else {
        let cache_dir = super::fs::open_directory(&root.join("cache"))?;
        match crate::store::archive_size_with_limit(
            &cache_dir,
            &cache.identity,
            &cache.exclusions,
            cache.candidate_bytes,
        ) {
            Ok(size) => {
                stdout
                    .write_all(b"CACHE_CANDIDATE\n")
                    .and_then(|()| stdout.flush())
                    .map_err(|_| ToolError::Operation)?;
                stdout
                    .write_all(&size.to_le_bytes())
                    .map_err(|_| ToolError::Operation)?;
                let captured = crate::store::capture_archive_with_limit(
                    &cache_dir,
                    &cache.identity,
                    &cache.exclusions,
                    &mut *stdout,
                    cache.candidate_bytes,
                )
                .map_err(map_cache_error)?;
                if captured != size {
                    return Err(ToolError::Operation);
                }
                stdout.flush().map_err(|_| ToolError::Operation)?;
            }
            Err(StoreError::StorageFull) => {
                stdout
                    .write_all(b"CACHE_REJECTED\n")
                    .and_then(|()| stdout.flush())
                    .map_err(|_| ToolError::Operation)?;
            }
            Err(_) => {
                stdout
                    .write_all(b"CACHE_FAILED\n")
                    .and_then(|()| stdout.flush())
                    .map_err(|_| ToolError::Operation)?;
            }
        }
    }
    Ok(())
}

fn receive_bulk(
    task: &Task,
    stdin: &mut std::io::BufReader<impl Read>,
    workspace: &Path,
) -> Result<
    (
        Option<super::worker::Image>,
        Option<crate::session::worker::WorkerChangeset>,
    ),
    ToolError,
> {
    if task.worker.is_none() && task.handover.is_none() {
        return Ok((None, None));
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(b"WORKER_READY\n")
        .and_then(|()| stdout.flush())
        .map_err(|_| ToolError::Operation)?;
    let bytes = super::worker::read_frame(stdin)?;
    if !stdin.buffer().is_empty() {
        return Err(ToolError::Configuration);
    }
    if let Some(transfer) = &task.worker {
        Ok((Some(transfer.decode(&bytes)?), None))
    } else {
        let changeset = super::worker::decode_handover(&task.intent, &bytes, &task.config)?;
        super::fs::worker_publication_paths(
            &super::fs::open_directory(workspace)?,
            &changeset.files,
        )?;
        Ok((None, Some(changeset)))
    }
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn send_result(
    task: Task,
    workspace: &Path,
    result: Result<String, ToolError>,
    stdout: &mut impl Write,
    root: &Path,
    before: Option<&super::worker::Image>,
) -> Result<(), ToolError> {
    let (disposition, output, enforcement_cause, limit_cause) =
        super::dispatch::outcome(&task.intent, result);
    let private_command =
        task.worker.is_some() && matches!(task.intent.call, ToolCall::Command { .. });
    let receipt = GuardReceipt {
        contract_version: 1,
        intent_digest: task.intent.digest(),
        enforcement_digest: task.intent.enforcement_digest,
        limits: task.intent.limits.clone(),
    };
    let mut observation = ToolObservation {
        enforcement_cause,
        limit_cause,
        intent: task.intent,
        disposition,
        output,
        attestation: Some(Attestation::Guard(receipt)),
        approval_cause: None,
        cache_update: if private_command {
            crate::tools::CacheUpdate::Discarded
        } else if task.cache.is_some() {
            crate::tools::CacheUpdate::NotAttempted
        } else {
            crate::tools::CacheUpdate::NotApplicable
        },
    };
    if private_command
        && serde_json::from_str::<crate::session::worker::WorkerCommandResult>(&observation.output)
            .is_err()
    {
        observation.output = serde_json::to_string(&crate::session::worker::WorkerCommandResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_total: Default::default(),
            stderr_total: Default::default(),
            private_capture: crate::session::worker::PrivateCapture::None {},
        })
        .map_err(|_| ToolError::Operation)?;
    }
    if private_command && observation.output.len() > observation.intent.limits.result_bytes as usize
    {
        observation.disposition = ToolDisposition::Limit;
        observation.limit_cause = Some(crate::tools::LimitCause::Resource);
        observation.enforcement_cause = None;
        observation.output = serde_json::to_string(&crate::session::worker::WorkerCommandResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_total: Default::default(),
            stderr_total: Default::default(),
            private_capture: crate::session::worker::PrivateCapture::None {},
        })
        .map_err(|_| ToolError::Operation)?;
    }
    if task.worker.is_none() && !observation.valid() {
        observation.disposition = ToolDisposition::Limit;
        observation.limit_cause = Some(crate::tools::LimitCause::Resource);
        observation.enforcement_cause = None;
        observation.output = "Tool result exceeded its reserved serialized capacity".into();
    }
    if task.worker.is_none() && !observation.valid() {
        return Err(ToolError::Limit);
    }
    let candidate = if let Some(worker) = &task.worker {
        let image = if matches!(observation.intent.call, ToolCall::Command { .. }) {
            let before = before.ok_or(ToolError::Configuration)?;
            #[cfg(target_os = "linux")]
            let (image, private_capture) = if observation.disposition == ToolDisposition::Succeeded
            {
                super::worker::command_capture(
                    before,
                    &super::fs::open_directory(workspace)?,
                    &worker.grant,
                )
            } else {
                (
                    before.clone(),
                    crate::session::worker::PrivateCapture::None {},
                )
            };
            #[cfg(not(target_os = "linux"))]
            let (image, private_capture) = (
                before.clone(),
                crate::session::worker::PrivateCapture::None {},
            );
            if let Ok(mut result) = serde_json::from_str::<
                crate::session::worker::WorkerCommandResult,
            >(&observation.output)
            {
                result.private_capture = private_capture;
                observation.output =
                    serde_json::to_string(&result).map_err(|_| ToolError::Operation)?;
            }
            image
        } else {
            super::fs::worker_image(
                &super::fs::open_directory(workspace)?,
                &super::worker::capture_paths(&worker.grant),
            )?
        };
        image.validate(&worker.grant)?;
        Some(image.encode()?)
    } else {
        None
    };
    serde_json::to_writer(&mut *stdout, &observation).map_err(|_| ToolError::Operation)?;
    stdout
        .write_all(b"\n")
        .and_then(|()| stdout.flush())
        .map_err(|_| ToolError::Operation)?;
    if let Some(bytes) = candidate {
        super::worker::write_frame(stdout, &bytes)?;
        stdout.flush().map_err(|_| ToolError::Operation)?;
    }
    #[cfg(target_os = "linux")]
    if let Some(cache) = &task.cache
        && task.worker.is_none()
    {
        send_cache_candidate(cache, observation.disposition, stdout, root)?;
    }
    Ok(())
}

pub(super) fn helper_main() -> ExitCode {
    let mut stage = BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        #[cfg(target_os = "macos")]
        macos::bootstrap()?;
        let mut stdin = std::io::BufReader::new(std::io::stdin().lock());
        let task = read_task(&mut stdin)?;
        validate_task(&task)?;
        let (root, workspace) = bootstrap_locations(&task, &mut stage)?;
        attest_runtime_mounts(
            &task.intent.call,
            &task.config.runtime_grants,
            task.intent.limits.cache_bytes > 0,
            &mut stage,
        )?;
        #[cfg(target_os = "linux")]
        if let Some(cache) = &task.cache {
            restore_cache(cache, &mut stdin, &root)?;
        }
        let workspace = workspace.as_path();
        stage = BootstrapStage::Workspace;
        let family = task.intent.call.facts().family;
        if task.worker.is_none()
            && (matches!(family, Family::File | Family::Git)
                || cfg!(target_os = "macos") && family == Family::Skill)
            && super::fs::identity(&super::fs::open_directory(workspace)?)?
                != (task.intent.workspace_device, task.intent.workspace_inode)
        {
            return Err(ToolError::ChangedInput);
        }
        let definition = definition_bytes(&task)?;
        let skill = if task.intent.call.facts().family == Family::Skill {
            Some(load_skill_bytes(&task)?)
        } else {
            None
        };
        let (image, handover) = receive_bulk(&task, &mut stdin, workspace)?;
        #[cfg(target_os = "linux")]
        {
            stage = BootstrapStage::Capabilities;
            linux::harden()?;
            if (image.is_some() || handover.is_some()) && !task.intent.call.is_process() {
                profile::install_non_spawning()?;
            }
        }
        stage = BootstrapStage::Handshake;
        let ready = Ready {
            digest: task.intent.digest(),
            pid: std::process::id(),
            #[cfg(target_os = "linux")]
            namespaces: profile::Namespaces::read()?,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            profile_digest: task.enforcement_digest()?,
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            profile_digest: [0; 32],
        };
        #[cfg(target_os = "linux")]
        if !ready.namespaces.isolated_from(&task.parent_namespaces) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &ready).map_err(|_| ToolError::Operation)?;
        stdout
            .write_all(b"\n")
            .and_then(|()| stdout.flush())
            .map_err(|_| ToolError::Operation)?;
        let mut go = [0; 3];
        stdin
            .read_exact(&mut go)
            .map_err(|_| ToolError::Operation)?;
        let runtime_started = runtime_start_at_go();
        // Admitted preparation and GO have their own deadlines.
        if go != *b"GO\n" {
            return Err(ToolError::Configuration);
        }
        verify_intent_runtime_grants(&task.intent, &task.config)?;
        attest_runtime_mounts(
            &task.intent.call,
            &task.config.runtime_grants,
            task.intent.limits.cache_bytes > 0,
            &mut stage,
        )?;
        if image.is_some() || handover.is_some() {
            let mut trailing = [0; 1];
            if stdin
                .read(&mut trailing)
                .map_err(|_| ToolError::Operation)?
                != 0
            {
                return Err(ToolError::Configuration);
            }
        }
        drop(stdin);
        stage = BootstrapStage::Payload;
        if let Some(image) = &image {
            super::worker::materialize(image, &super::fs::open_directory(workspace)?)?;
        }
        #[cfg(not(target_os = "macos"))]
        if task.intent.call.is_process() && task.worker.is_none() {
            super::fs::copy_seed()?;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ToolError::Operation)?;
        let site = GuardContext {
            intent: &task.intent,
            config: &task.config,
            root: &root,
            workspace,
            skill,
            hook_stdin: task.hook_stdin.as_deref(),
            hidden: &task.hidden,
            definition,
            handover,
            runtime_started,
            #[cfg(target_os = "linux")]
            parent_namespaces: &task.parent_namespaces,
        };
        let result = runtime.block_on(super::dispatch::run(&site));
        stage = BootstrapStage::Receipt;
        send_result(task, workspace, result, &mut stdout, &root, image.as_ref())?;
        Ok(())
    })();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        let _ = serde_json::to_writer(std::io::stderr().lock(), &BootstrapFailure::at(stage));
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "macos")]
pub(super) fn unit_main() -> ExitCode {
    macos::unit_main()
}

#[cfg(target_os = "linux")]
fn parser_config(config: &Config) -> Config {
    Config {
        child_unguarded: super::UnguardedGrant::Denied,
        commands: Vec::new(),
        skills: Vec::new(),
        mcp: Vec::new(),
        git: None,
        git_identity: None,
        runtime_grants: Vec::new(),
        ..config.clone()
    }
}

fn definition_bytes(task: &Task) -> Result<Option<Vec<u8>>, ToolError> {
    use base64::Engine as _;
    match (&task.intent.call, &task.definition) {
        (ToolCall::ParseAgentDefinition { raw_digest, .. }, Some(encoded)) => {
            if encoded.len() > DEFINITION_TASK_BYTES {
                return Err(ToolError::Limit);
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| ToolError::Configuration)?;
            if bytes.len() > 64 * 1024 {
                return Err(ToolError::Limit);
            }
            if digest(&bytes) != *raw_digest
                || base64::engine::general_purpose::STANDARD.encode(&bytes) != *encoded
            {
                return Err(ToolError::ChangedInput);
            }
            Ok(Some(bytes))
        }
        (ToolCall::ParseAgentDefinition { .. }, _) | (_, Some(_)) => Err(ToolError::Configuration),
        (_, None) => Ok(None),
    }
}

fn load_skill_bytes(task: &Task) -> Result<Vec<u8>, ToolError> {
    let ToolCall::Skill { name, resource } = &task.intent.call else {
        return Err(ToolError::Configuration);
    };
    let resource = resource.as_deref().unwrap_or("SKILL.md");
    #[cfg(target_os = "macos")]
    let path = {
        let skill = task
            .config
            .skills
            .iter()
            .find(|skill| &skill.name == name)
            .ok_or(ToolError::Configuration)?;
        Path::new(&skill.directory)
            .join(resource)
            .to_str()
            .ok_or(ToolError::Path)?
            .to_owned()
    };
    #[cfg(not(target_os = "macos"))]
    let path = format!("/skills/{name}/{resource}");
    let bytes = super::fs::read_regular(super::fs::open_absolute(&path)?, MAX_TOOL_RESULT_BYTES)?;
    if task
        .config
        .skills
        .iter()
        .find(|skill| &skill.name == name)
        .and_then(|skill| skill.files.get(resource))
        != Some(&hex_digest(&bytes))
    {
        return Err(ToolError::ChangedInput);
    }
    Ok(bytes)
}

pub(super) fn runtime_path(runtime_grants: &[RuntimeGrant]) -> Result<String, ToolError> {
    let mut paths = Vec::new();
    for grant in runtime_grants
        .iter()
        .filter(|grant| grant.kind.is_toolchain())
    {
        let bin = grant.path.join("bin");
        paths.push(bin.to_str().ok_or(ToolError::Path)?.to_owned());
    }
    paths.extend(["/usr/bin".into(), "/bin".into()]);
    Ok(paths.join(":"))
}

#[cfg(any(target_os = "macos", test))]
fn prefix_shell_runtime_compat(
    args: &mut [std::ffi::OsString],
    path: &str,
) -> Result<(), ToolError> {
    if !args.first().is_some_and(|arg| arg == "-c") {
        return Ok(());
    }
    let script = args.get_mut(1).ok_or(ToolError::Configuration)?;
    let text = script.to_str().ok_or(ToolError::Path)?;
    // Shells parse options after -c; reject ambiguous script positions before changing argv.
    if text.starts_with(['-', '+']) {
        return Err(ToolError::ProtectionUnavailable);
    }
    *script = format!(
        "export DYLD_INSERT_LIBRARIES='{}'\n{text}",
        path.replace('\'', "'\\''"),
    )
    .into();
    Ok(())
}

pub(super) fn payload(
    program: &super::config::Program,
    args: &[String],
    cwd: &str,
    root: &Path,
    runtime_grants: &[RuntimeGrant],
    cache_enabled: bool,
) -> Result<Command, ToolError> {
    let resources = if cfg!(target_os = "macos") {
        root.join("runtime")
    } else {
        root.to_owned()
    };
    let workspace = root.join("workspace");
    let scratch = root.join("scratch");
    let cache = root.join("cache");
    let cache_base = if cache_enabled { &cache } else { &scratch };
    let executable = if native_system_program(&program.executable)? {
        Path::new(&program.executable).to_owned()
    } else {
        resources.join("programs").join(&program.name)
    };
    let bytes = super::fs::read_regular(
        super::fs::open_absolute(executable.to_str().ok_or(ToolError::Path)?)?,
        MAX_SNAPSHOT_BYTES,
    )?;
    if hex_digest(&bytes) != program.sha256 {
        return Err(ToolError::ChangedInput);
    }
    let path = runtime_path(runtime_grants)?;
    let payload_args: Vec<_> = args
        .iter()
        .map(|arg| {
            if cfg!(target_os = "macos") {
                for (alias, target) in [
                    ("workspace", &workspace),
                    ("scratch", &scratch),
                    ("cache", &cache),
                    ("inputs", &resources.join("inputs")),
                    ("skills", &resources.join("skills")),
                    ("programs", &resources.join("programs")),
                ] {
                    if let Ok(relative) = Path::new(arg).strip_prefix(format!("/{alias}")) {
                        return target.join(relative).into_os_string();
                    }
                }
            }
            std::ffi::OsString::from(arg)
        })
        .collect();
    let mut command = Command::new(executable);
    command
        .current_dir(workspace.join(cwd))
        .env_clear()
        .env("PATH", path)
        .env("HOME", &scratch)
        .env("TMPDIR", &scratch)
        .env("CARGO_HOME", cache_base.join("cargo"))
        .env("CARGO_TARGET_DIR", cache_base.join("target"))
        .env("npm_config_cache", cache_base.join(".npm"))
        .env("PIP_CACHE_DIR", cache_base.join(".cache/pip"))
        .env("ARANY_TOOL_WORKSPACE", &workspace)
        .env("ARANY_TOOL_INPUTS", resources.join("inputs"))
        .env("ARANY_TOOL_SKILLS", resources.join("skills"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "macos")]
    let payload_args = {
        let mut args = payload_args;
        macos::configure_runtime_compat(&mut command, program, &mut args, &resources)?;
        args
    };
    command.args(payload_args);
    // Cargo's libcurl otherwise initializes LibreSSL, which reads /private/etc/ssl and fails under Seatbelt.
    if cfg!(target_os = "macos")
        && runtime_grants
            .iter()
            .any(|grant| grant.kind == super::toolchains::RuntimeKind::Rust)
    {
        command.env("CURL_SSL_BACKEND", "Secure-Transport");
    }
    if cfg!(target_os = "macos")
        && let Some(sdk) = runtime_grants
            .iter()
            .find(|grant| grant.kind == super::toolchains::RuntimeKind::AppleSdk)
    {
        command.env("SDKROOT", &sdk.path);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_runtime_compat_preserves_script_position_and_rejects_options() {
        use std::ffi::OsString;

        let path = "/private/runtime 'quoted'/compat.dylib";
        let script = "printf '%s\\n' \"$0\" \"$1\"";
        for tail in [vec![], vec!["arg0"], vec!["arg0", "-e"]] {
            let mut args: Vec<OsString> = ["-c", script]
                .into_iter()
                .chain(tail.iter().copied())
                .map(OsString::from)
                .collect();
            let original = args.clone();
            prefix_shell_runtime_compat(&mut args, path).unwrap();
            assert_eq!(args[0], original[0]);
            assert_eq!(args[2..], original[2..]);
            assert_eq!(
                args[1],
                OsString::from(format!(
                    "export DYLD_INSERT_LIBRARIES='/private/runtime '\\''quoted'\\''/compat.dylib'\n{script}"
                ))
            );
        }
        for option in ["--", "-e", "+e", "-", "+"] {
            let mut args = ["-c", option, script].map(OsString::from);
            let original = args.clone();
            assert!(matches!(
                prefix_shell_runtime_compat(&mut args, path),
                Err(ToolError::ProtectionUnavailable)
            ));
            assert_eq!(args, original);
        }
        let mut missing = [OsString::from("-c")];
        assert!(matches!(
            prefix_shell_runtime_compat(&mut missing, path),
            Err(ToolError::Configuration)
        ));
        let mut file = [OsString::from("script.sh"), OsString::from("-c")];
        let original = file.clone();
        prefix_shell_runtime_compat(&mut file, path).unwrap();
        assert_eq!(file, original);
    }

    #[test]
    fn agent_definition_guard_task_binds_snapshot_digest_and_closed_payload() {
        use base64::Engine as _;
        let raw = vec![b' '; 64 * 1024];
        let call = ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: digest(&raw),
        };
        let config: Config = serde_json::from_value(serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
        let mut task = Task {
            config,
            intent: intent(call),
            cache: None,
            hook_stdin: None,
            hidden: Default::default(),
            definition: Some(base64::engine::general_purpose::STANDARD.encode(&raw)),
            worker: None,
            handover: None,
            #[cfg(target_os = "linux")]
            parent_namespaces: profile::Namespaces::read().unwrap(),
            #[cfg(target_os = "macos")]
            workspace: "/private/fixture/workspace".into(),
            #[cfg(target_os = "macos")]
            guard_executable: "/private/fixture/arany".into(),
            #[cfg(target_os = "macos")]
            system_programs: None,
        };
        let encoded = encode_task(&task).unwrap();
        assert!(encoded.len() <= TASK_BYTES_CAP + 1);
        let decoded = read_task(&mut std::io::BufReader::new(encoded.as_slice())).unwrap();
        assert_eq!(
            definition_bytes(&decoded).unwrap().unwrap().len(),
            raw.len()
        );
        let mut value = serde_json::to_value(&task).unwrap();
        value.as_object_mut().unwrap().remove("definition");
        let mut missing = serde_json::to_vec(&value).unwrap();
        missing.push(b'\n');
        assert!(read_task(&mut std::io::BufReader::new(missing.as_slice())).is_err());
        task.definition =
            Some(base64::engine::general_purpose::STANDARD.encode(vec![b' '; 64 * 1024 + 1]));
        assert!(matches!(definition_bytes(&task), Err(ToolError::Limit)));
        task.definition = Some(base64::engine::general_purpose::STANDARD.encode(b"changed"));
        assert!(matches!(
            definition_bytes(&task),
            Err(ToolError::ChangedInput)
        ));
        task.definition = None;
        assert!(definition_bytes(&task).is_err());
        task.definition = Some("invalid*".into());
        assert!(definition_bytes(&task).is_err());
        task.intent.call = ToolCall::List { path: ".".into() };
        assert!(definition_bytes(&task).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn agent_definition_task_fits_the_cap_with_a_large_pinned_config() {
        use base64::Engine as _;
        let raw = vec![b' '; 64 * 1024];
        let call = ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: digest(&raw),
        };
        let commands: Vec<_> = (0..200)
            .map(|index| {
                serde_json::json!({"name": format!("program{index}"), "executable": format!("/usr/bin/{}", "x".repeat(4900)), "sha256": "0".repeat(64), "interpreter": false, "inputs": []})
            })
            .collect();
        let config: Config = serde_json::from_value(serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":commands,"skills":[],"mcp":[]})).unwrap();
        let task = |config: Config| Task {
            config,
            intent: intent(call.clone()),
            cache: None,
            hook_stdin: None,
            hidden: Default::default(),
            definition: Some(base64::engine::general_purpose::STANDARD.encode(&raw)),
            worker: None,
            handover: None,
            parent_namespaces: profile::Namespaces::read().unwrap(),
        };
        let encoded = encode_task(&task(config.clone())).unwrap();
        assert!(encoded.len() > TASK_BYTES_CAP);
        let decoded = read_task(&mut std::io::BufReader::new(encoded.as_slice())).unwrap();
        assert!(decoded.definition.is_some());
        let mut plain = task(config);
        plain.definition = None;
        plain.intent.call = ToolCall::List { path: ".".into() };
        plain.config.workspace_paths = vec!["p".repeat(TASK_BYTES_CAP)];
        assert!(matches!(encode_task(&plain), Err(ToolError::Limit)));
        let mut oversized = serde_json::to_vec(&plain).unwrap();
        oversized.push(b'\n');
        assert!(matches!(
            read_task(&mut std::io::BufReader::new(oversized.as_slice())),
            Err(ToolError::Limit)
        ));
    }

    #[test]
    fn the_largest_typed_write_fits_one_task_frame_at_its_exact_ceiling() {
        let config: Config = serde_json::from_value(serde_json::json!({"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]})).unwrap();
        let task = Task {
            config,
            intent: intent(ToolCall::Write {
                path: "\"".repeat(512),
                expected_digest: Some("0".repeat(64)),
                content: "\u{1}".repeat(crate::tools::types::MAX_EDIT_CONTENT_BYTES),
            }),
            cache: None,
            hook_stdin: None,
            hidden: Default::default(),
            definition: None,
            worker: None,
            handover: None,
            #[cfg(target_os = "linux")]
            parent_namespaces: profile::Namespaces::read().unwrap(),
            #[cfg(target_os = "macos")]
            workspace: "/private/fixture/workspace".into(),
            #[cfg(target_os = "macos")]
            guard_executable: "/private/fixture/arany".into(),
            #[cfg(target_os = "macos")]
            system_programs: None,
        };
        let mut frame = encode_task(&task).unwrap();
        assert!(frame.len() <= TASK_BYTES_CAP + 1);
        let read = |bytes: &[u8]| read_task(&mut std::io::BufReader::new(bytes));
        assert!(read(&frame).is_ok());
        frame.pop();
        frame.resize(TASK_BYTES_CAP, b' ');
        frame.push(b'\n');
        assert!(read(&frame).is_ok(), "the exact ceiling is admissible");
        frame.insert(0, b' ');
        assert!(matches!(read(&frame), Err(ToolError::Limit)));
    }

    #[test]
    fn task_reads_classify_eof_short_malformed_and_oversized_inputs() {
        let read = |bytes: &[u8]| read_task(&mut std::io::BufReader::new(bytes));
        assert!(matches!(read(b""), Err(ToolError::Operation)));
        assert!(matches!(read(b"{\"config\""), Err(ToolError::Operation)));
        assert!(matches!(read(b"{\n"), Err(ToolError::Configuration)));
        assert!(matches!(read(b"{}\n{}\n"), Err(ToolError::Configuration)));
        let mut oversized = vec![b' '; task_cap(true) + 1];
        assert!(matches!(read(&oversized), Err(ToolError::Limit)));
        oversized.push(b'\n');
        assert!(matches!(read(&oversized), Err(ToolError::Limit)));
    }

    #[test]
    fn worker_failure_lines_keep_their_closed_stage() {
        let failure = br#"{"stage":"launcher_reap"}"#;
        assert!(matches!(
            parse_observation(failure),
            Err(ToolError::GuardBootstrap(found)) if found.failure_stage() == "launcher reap"
        ));
        let mut hook = intent(ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
        });
        hook.policy_digest = [1; 32];
        hook.enforcement_digest = [2; 32];
        let before = dispatch_result(&hook, false, parse_observation(failure)).unwrap();
        assert_eq!(before.enforcement_cause, None);
        assert!(before.valid());
        assert!(before.stops_run());
        assert!(dispatch_result(&hook, true, parse_observation(failure)).is_err());
        assert!(matches!(
            parse_observation(br#"{"stage":"made_up"}"#),
            Err(ToolError::Operation)
        ));
    }

    #[test]
    fn bootstrap_continuation_and_hook_reruns_require_sandbox_setup() {
        let mut hook = intent(ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
        });
        hook.policy_digest = [1; 32];
        hook.enforcement_digest = [2; 32];
        let unguarded = ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
            justification: "Run the failed hook once".into(),
        };
        for (stage, setup) in [
            (BootstrapStage::Admission, false),
            (BootstrapStage::Workspace, false),
            (BootstrapStage::RuntimeMounts, true),
            (BootstrapStage::Capabilities, true),
            (BootstrapStage::Handshake, true),
            (BootstrapStage::Payload, false),
            (BootstrapStage::Receipt, false),
            (BootstrapStage::ProcessStop, false),
            (BootstrapStage::LauncherReap, false),
            (BootstrapStage::StderrDrain, false),
            (BootstrapStage::StorageCleanup, false),
        ] {
            let failure = BootstrapFailure::at(stage);
            let encoded = serde_json::to_vec(&failure).unwrap();
            let observation = dispatch_result(&hook, false, parse_observation(&encoded)).unwrap();
            assert_eq!(
                observation.disposition,
                ToolDisposition::Failed,
                "{failure}"
            );
            assert!(observation.attestation.is_none(), "{failure}");
            assert_eq!(
                observation.enforcement_cause,
                setup.then_some(EnforcementCause::GuardBootstrap),
                "{failure}"
            );
            assert!(observation.valid(), "{failure}");
            assert_eq!(observation.stops_run(), !setup, "{failure}");
            assert_eq!(
                qualifying_hook_failure(&unguarded, std::iter::once(&observation)).is_some(),
                setup,
                "{failure}"
            );
            assert!(dispatch_result(&hook, true, parse_observation(&encoded)).is_err());
        }
    }

    #[test]
    fn launcher_runtime_ends_at_the_go_deadline_inside_the_supervisor_grace() {
        let go = std::time::Instant::now();
        let runtime = Duration::from_millis(5_000);
        let setup = Duration::from_millis(1_500);
        let launcher_end = go + setup + runtime_after(Some(go), 5_000, go + setup);
        assert_eq!(launcher_end, go + runtime);
        assert_eq!(
            runtime_after(Some(go), 5_000, go + runtime + setup),
            Duration::ZERO
        );
        assert_eq!(runtime_after(None, 5_000, go + runtime + setup), runtime);
        assert_eq!(
            runtime_start_at_go().is_some(),
            cfg!(target_os = "macos"),
            "Linux keeps the full runtime from spawn"
        );
        let supervisor_go = tokio::time::Instant::now();
        assert!(
            payload_report_deadline(supervisor_go, 5_000)
                > supervisor_go + runtime + PAYLOAD_KILL_WAIT
        );
        assert!(payload_report_deadline(supervisor_go, u32::MAX) > supervisor_go);
    }

    #[test]
    fn worker_output_shares_never_exceed_the_cap_and_drop_the_rest() {
        let written = std::sync::atomic::AtomicUsize::new(0);
        assert_eq!(output_share(&written, 1_000), 1_000);
        assert_eq!(output_share(&written, 5_000), WORKER_OUTPUT_BYTES - 1_000);
        assert_eq!(output_share(&written, 10), 0);
        assert_eq!(output_share(&written, 0), 0);
    }

    #[test]
    fn a_closed_worker_names_its_panic_from_a_bounded_stderr_tail() {
        let mut file = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut file, &vec![b'x'; 10_000]).unwrap();
        std::io::Write::write_all(
            &mut file,
            b"\nthread 'main' panicked at src/unit.rs:1:2:\n\x1b[31mboom\n",
        )
        .unwrap();
        let tail = read_tail(&mut file).unwrap();
        assert_eq!(tail.len() as u64, WORKER_STDERR_TAIL);
        let closed = || execution_failure(GuardCheck::WorkerClosed);
        let ToolError::GuardBootstrap(panicked) = with_worker_exit(closed(), &tail) else {
            panic!("a closed worker stays a Guard failure");
        };
        assert_eq!(panicked.failure_stage(), "execution");
        assert_eq!(panicked.check(), Some("worker panicked"));
        let detail = panicked.detail().unwrap();
        assert!(detail.ends_with("thread 'main' panicked at src/unit.rs:1:2: [31mboom"));
        assert!(detail.chars().all(|ch| ch.is_ascii_graphic() || ch == ' '));
        assert_eq!(
            serde_json::to_string(&panicked).unwrap(),
            r#"{"stage":"payload","check":"worker_panicked"}"#
        );

        let ToolError::GuardBootstrap(silent) = with_worker_exit(closed(), b"") else {
            panic!("a closed worker stays a Guard failure");
        };
        assert_eq!(
            (silent.check(), silent.detail()),
            (Some("worker closed before its report"), None)
        );
        let other = with_worker_exit(
            execution_failure(GuardCheck::ReportMalformed),
            b"panicked at x",
        );
        assert!(matches!(
            other,
            ToolError::GuardBootstrap(failure)
                if failure.check() == Some("malformed report") && failure.detail().is_none()
        ));
    }

    #[test]
    #[ignore = "pipe-gated child of the_launcher_never_shares_the_supervisor_process_group"]
    fn launcher_group_child() {
        use std::io::Read;
        let _ = std::io::stdin().read(&mut [0; 1]);
    }

    #[tokio::test]
    async fn the_launcher_never_shares_the_supervisor_process_group() {
        // The child is this test binary blocked on the pipe the test holds, so no shell is needed.
        let executable = std::env::current_exe().unwrap();
        let mut launcher = launcher_command(&executable)
            .args([
                "--exact",
                "tools::guard::tests::launcher_group_child",
                "--ignored",
                "--nocapture",
            ])
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let _gate = launcher.stdin.take().unwrap();
        let pid = rustix::process::Pid::from_raw(launcher.id().unwrap() as i32);
        let group = rustix::process::getpgid(pid).unwrap();
        assert_eq!(Some(group), pid);
        assert_ne!(group, rustix::process::getpgrp());
        launcher.start_kill().unwrap();
        let _ = launcher.wait().await;
    }

    #[tokio::test]
    async fn an_exhausted_runtime_is_a_limit_without_a_spawn() {
        let mut missing = Command::new("/nonexistent/arany-test-program");
        let expired = spawn_before(&mut missing, tokio::time::Instant::now());
        assert!(matches!(&expired, Err(ToolError::Timeout(output)) if output.is_empty()));
        let intent = intent(ToolCall::Command {
            program: "/nonexistent/arany-test-program".into(),
            args: Vec::new(),
            cwd: String::new(),
        });
        let (disposition, output, enforcement_cause, limit_cause) =
            super::super::dispatch::outcome(&intent, expired.map(|_| String::new()));
        assert_eq!(disposition, ToolDisposition::Limit);
        assert_eq!(limit_cause, Some(LimitCause::Timeout));
        assert_eq!(output, "Tool runtime expired");
        assert_eq!(enforcement_cause, None);
        assert!(matches!(
            spawn_before(
                &mut missing,
                tokio::time::Instant::now() + Duration::from_secs(60)
            ),
            Err(ToolError::Operation)
        ));
    }

    #[tokio::test]
    async fn an_unlisted_device_is_refused_retryably_and_retried_within_the_bound() {
        assert!(require_listed(["disk4", "disk5"].into_iter(), "disk4").is_ok());
        assert!(matches!(
            require_listed(["disk5"].into_iter(), "disk4"),
            Err(ToolError::ProtectionUnavailable)
        ));
        let recheck = |listings: &'static [&'static [&'static str]]| {
            let attempt = std::cell::Cell::new(0);
            move || {
                let listed = listings[attempt.get().min(listings.len() - 1)];
                attempt.set(attempt.get() + 1);
                std::future::ready(require_listed(listed.iter().copied(), "disk4"))
            }
        };
        let far = tokio::time::Instant::now() + Duration::from_secs(60);
        assert!(
            retry_transient(8, Duration::ZERO, far, recheck(&[&[], &["disk4"]]))
                .await
                .is_ok(),
            "a later attempt re-lists before acting"
        );
        let calls = std::cell::Cell::new(0);
        let unlisted = recheck(&[&[]]);
        let never_listed = retry_transient(8, Duration::ZERO, far, || {
            calls.set(calls.get() + 1);
            unlisted()
        })
        .await;
        assert!(matches!(
            never_listed,
            Err(ToolError::ProtectionUnavailable)
        ));
        assert_eq!(calls.get(), 8);
        let lapsed = retry_transient(
            8,
            Duration::ZERO,
            tokio::time::Instant::now(),
            recheck(&[&[]]),
        )
        .await;
        assert!(matches!(lapsed, Err(ToolError::ProtectionUnavailable)));
    }

    #[tokio::test]
    async fn worker_report_rejections_name_their_execution_check() {
        let mut intent = intent(ToolCall::Command {
            program: "sh".into(),
            args: vec![],
            cwd: String::new(),
        });
        intent.policy_digest = [1; 32];
        intent.enforcement_digest = [2; 32];
        let check = |bytes: Vec<u8>| {
            let intent = intent.clone();
            async move {
                let mut reader = tokio::io::BufReader::new(bytes.as_slice());
                match read_worker_report(&mut reader, &intent).await {
                    Err(ToolError::GuardBootstrap(failure)) => {
                        Some((failure.failure_stage(), failure.check()))
                    }
                    _ => None,
                }
            }
        };
        let observation = |attested: bool| ToolObservation {
            enforcement_cause: None,
            limit_cause: Some(crate::tools::LimitCause::Resource),
            intent: intent.clone(),
            disposition: ToolDisposition::Limit,
            output: "Tool result or resource limit reached".into(),
            attestation: attested.then(|| {
                Attestation::Guard(GuardReceipt {
                    contract_version: 1,
                    intent_digest: intent.digest(),
                    enforcement_digest: intent.enforcement_digest,
                    limits: intent.limits.clone(),
                })
            }),
            approval_cause: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        let line = |observation: &ToolObservation| {
            let mut bytes = serde_json::to_vec(observation).unwrap();
            bytes.push(b'\n');
            bytes
        };
        assert_eq!(
            check(Vec::new()).await,
            Some(("execution", Some("worker closed before its report")))
        );
        assert_eq!(
            check(b"partial".to_vec()).await,
            Some(("execution", Some("worker closed before its report")))
        );
        assert_eq!(
            check(b"not a report\n".to_vec()).await,
            Some(("execution", Some("malformed report")))
        );
        assert_eq!(
            check(
                br#"{"stage":"storage_cleanup","check":"detach_failed"}
"#
                .to_vec()
            )
            .await,
            Some(("storage cleanup", Some("detach failed"))),
            "a worker's own closed failure passes through unchanged"
        );
        assert_eq!(
            check(line(&observation(false))).await,
            Some(("execution", Some("unattested report")))
        );
        let attested = observation(true);
        assert!(attested.attested_for(&intent));
        let mut trailing = line(&attested);
        trailing.push(b'x');
        assert_eq!(
            check(trailing).await,
            Some(("execution", Some("report not followed by end of stream")))
        );
        let mut overlong = line(&attested);
        overlong.extend_from_slice(b"xy");
        let mut reader = tokio::io::BufReader::new(overlong.as_slice());
        assert!(
            matches!(
                read_worker_report(&mut reader, &intent).await,
                Err(ToolError::Limit)
            ),
            "two trailing bytes keep the resource-limit outcome"
        );
        let report = line(&attested);
        let mut reader = tokio::io::BufReader::new(report.as_slice());
        assert_eq!(
            read_worker_report(&mut reader, &intent)
                .await
                .unwrap()
                .disposition,
            ToolDisposition::Limit
        );
    }

    #[tokio::test]
    async fn transient_retry_is_bounded_and_keeps_terminal_errors() {
        let calls = std::cell::Cell::new(0);
        let far = tokio::time::Instant::now() + Duration::from_secs(60);
        let recovered = retry_transient(3, Duration::ZERO, far, || {
            calls.set(calls.get() + 1);
            let attempt = calls.get();
            async move {
                if attempt < 3 {
                    Err(ToolError::ProtectionUnavailable)
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert!(matches!(recovered, Ok(3)));

        calls.set(0);
        let exhausted: Result<(), _> = retry_transient(3, Duration::ZERO, far, || {
            calls.set(calls.get() + 1);
            async { Err(ToolError::Operation) }
        })
        .await;
        assert!(matches!(exhausted, Err(ToolError::Operation)));
        assert_eq!(calls.get(), 3);

        calls.set(0);
        let expired: Result<(), _> =
            retry_transient(3, Duration::ZERO, tokio::time::Instant::now(), || {
                calls.set(calls.get() + 1);
                async { Err(ToolError::Operation) }
            })
            .await;
        assert!(matches!(expired, Err(ToolError::Operation)));
        assert_eq!(calls.get(), 1);

        calls.set(0);
        let soon = tokio::time::Instant::now() + Duration::from_millis(20);
        let lapsed: Result<(), _> = retry_transient(3, Duration::from_secs(5), soon, || {
            calls.set(calls.get() + 1);
            async { Err(ToolError::Operation) }
        })
        .await;
        assert!(matches!(lapsed, Err(ToolError::Operation)));
        assert_eq!(
            calls.get(),
            1,
            "no attempt starts after the pause reaches the deadline"
        );

        calls.set(0);
        let terminal: Result<(), _> = retry_transient(3, Duration::ZERO, far, || {
            calls.set(calls.get() + 1);
            async { Err(ToolError::Path) }
        })
        .await;
        assert!(matches!(terminal, Err(ToolError::Path)));
        assert_eq!(calls.get(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enforcement_causes_require_guard_evidence() {
        let mut observation = command_observation();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf '%s' 'bwrap: Permission denied; sandbox deny network; EACCES EPERM EROFS' >&2; exit 1"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let (status, stdout, stderr) =
            capture(command.spawn().unwrap(), 1024, Duration::from_secs(5))
                .await
                .unwrap();
        assert_eq!(status.code(), Some(1));
        let output = serde_json::json!({
            "exit_code": status.code(),
            "stdout": String::from_utf8(stdout).unwrap(),
            "stderr": String::from_utf8(stderr).unwrap(),
            "enforcement_cause": "filesystem_denied",
            "changeset": {"files": []}
        })
        .to_string();
        let (disposition, output, cause, limit_cause) =
            super::super::dispatch::outcome(&observation.intent, Ok(output));
        assert_eq!(disposition, ToolDisposition::Failed);
        assert_eq!(cause, None, "program output cannot attest a Guard denial");
        observation.disposition = disposition;
        observation.limit_cause = limit_cause;
        observation.output = output;
        assert!(
            model_observation(&observation)
                .get("enforcement_cause")
                .is_none()
        );
        for error in [
            ToolError::Path,
            ToolError::Operation,
            ToolError::ProtectionUnavailable,
        ] {
            let (_, _, cause, _) = super::super::dispatch::outcome(&observation.intent, Err(error));
            assert_eq!(cause, None, "opaque failures have no attributable cause");
        }
        for call in [
            ToolCall::McpCall {
                server: "server".into(),
                tool: "tool".into(),
                schema_digest: "0".repeat(64),
                arguments: "{}".into(),
            },
            ToolCall::Skill {
                name: "skill".into(),
                resource: None,
            },
        ] {
            let intent = intent(call);
            let (_, _, cause, _) = super::super::dispatch::outcome(&intent, Ok(
                r#"{"is_error":true,"enforcement_cause":"filesystem_denied","text":"sandbox deny"}"#.into()
            ));
            assert_eq!(cause, None, "MCP and Skill text are data");
        }
        let before = model_observation(&observation);
        assert!(before.get("enforcement_cause").is_none());
        let workspace = tempfile::tempdir().unwrap();
        let snapshot = super::super::fs::command_snapshot(workspace.path()).unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        std::fs::write(
            workspace.path().join(".git/config"),
            b"protected fixture content",
        )
        .unwrap();
        let rejected = super::super::fs::command_changeset(workspace.path(), &snapshot);
        let (disposition, output, cause, limit_cause) =
            super::super::dispatch::outcome(&observation.intent, rejected.map(|_| String::new()));
        assert_eq!(cause, Some(EnforcementCause::FilesystemDenied));
        observation.disposition = disposition;
        observation.limit_cause = limit_cause;
        observation.output = output;
        observation.enforcement_cause = cause;
        assert!(observation.valid());
        assert!(!observation.output.contains(".git"));
        assert!(!observation.output.contains("protected fixture content"));
        let projected = model_observation(&observation);
        assert_eq!(
            projected["enforcement_cause"],
            "Guard enforcement: protected path or set-ID entry refused"
        );
    }

    fn node_grant(home: &Path) -> RuntimeGrant {
        RuntimeGrant {
            kind: super::super::toolchains::RuntimeKind::Node,
            manager: super::super::toolchains::RuntimeManager::Nvm,
            version: Some("v22.3.0".into()),
            path: home.join(".nvm/versions/node/v22.3.0"),
            device: 1,
            inode: 2,
        }
    }

    fn intent(call: ToolCall) -> EffectIntent {
        EffectIntent {
            host: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::default(),
            agent_run_id: crate::session::AgentRunId::default(),
            policy_digest: [0; 32],
            enforcement_digest: [0; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            use_count: 1,
        }
    }

    fn config(runtime_grants: Vec<RuntimeGrant>) -> Config {
        Config {
            version: 1,
            workspace_paths: vec![".".into()],
            write: false,
            child_unguarded: crate::tools::UnguardedGrant::Denied,
            commands: Vec::new(),
            skills: Vec::new(),
            mcp: Vec::new(),
            runtime_grants,
            git: None,
            git_identity: None,
        }
    }

    #[test]
    fn runtime_mount_rejection_reports_its_own_bootstrap_stage() {
        let failure = BootstrapFailure::at(BootstrapStage::RuntimeMounts);
        assert_eq!(failure.failure_stage(), "runtime mount topology");
        assert_eq!(failure.to_string(), "runtime mount topology");
        let encoded = serde_json::to_vec(&failure).unwrap();
        assert_eq!(encoded, br#"{"stage":"runtime_mounts"}"#);
        let mut intent = intent(ToolCall::Command {
            program: "node".into(),
            args: Vec::new(),
            cwd: String::new(),
        });
        intent.policy_digest = [1; 32];
        intent.enforcement_digest = [2; 32];
        let observation = dispatch_result(&intent, false, parse_observation(&encoded)).unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert!(observation.attestation.is_none());
        assert_eq!(
            observation.enforcement_cause,
            Some(EnforcementCause::GuardBootstrap)
        );
        assert!(observation.valid());
        assert!(!observation.stops_run());
        assert_eq!(
            model_observation(&observation)["enforcement_cause"],
            "Guard enforcement: bootstrap failed before dispatch"
        );
        let unguarded = ToolCall::CommandUnguarded {
            program: "node".into(),
            args: Vec::new(),
            cwd: String::new(),
            justification: "Run the failed command once".into(),
        };
        assert!(qualifying_guard_failure(&unguarded, std::iter::once(&observation)).is_none());
        assert!(dispatch_result(&intent, true, parse_observation(&encoded)).is_err());
    }

    #[test]
    fn storage_cleanup_failure_carries_a_closed_check_without_changing_its_stage() {
        let failure: BootstrapFailure =
            serde_json::from_str(r#"{"stage":"storage_cleanup","check":"detach_failed"}"#).unwrap();
        assert_eq!(failure.failure_stage(), "storage cleanup");
        assert_eq!(failure.check(), Some("detach failed"));
        assert_eq!(
            serde_json::to_string(&failure).unwrap(),
            r#"{"stage":"storage_cleanup","check":"detach_failed"}"#
        );
        let unlisted: BootstrapFailure =
            serde_json::from_str(r#"{"stage":"storage_cleanup","check":"device_unlisted"}"#)
                .unwrap();
        assert_eq!(unlisted.check(), Some("device unlisted before detach"));
        let bare: BootstrapFailure = serde_json::from_str(r#"{"stage":"payload"}"#).unwrap();
        assert_eq!(bare.check(), None);
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"stage":"payload"}"#
        );
        assert!(
            serde_json::from_str::<BootstrapFailure>(r#"{"stage":"payload","check":"raw text"}"#)
                .is_err()
        );
    }

    #[test]
    fn file_profile_digest_excludes_selected_process_runtimes() {
        let temp = tempfile::tempdir().unwrap();
        let grants = [node_grant(temp.path())];
        let file_profile = intent_enforcement_digest(
            false,
            &grants,
            #[cfg(target_os = "macos")]
            None,
        )
        .unwrap();
        let process_profile = intent_enforcement_digest(
            true,
            &grants,
            #[cfg(target_os = "macos")]
            None,
        )
        .unwrap();
        assert_eq!(file_profile, enforcement_digest(&[]).unwrap());
        assert_ne!(file_profile, process_profile);
    }

    #[test]
    fn helper_revalidates_runtime_roots_only_for_process_intents() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(vec![node_grant(temp.path())]);
        let file = intent(ToolCall::Read {
            path: "selected.txt".into(),
            offset: None,
            limit: Some(32),
        });
        let process = intent(ToolCall::Command {
            program: "node".into(),
            args: Vec::new(),
            cwd: ".".into(),
        });
        assert!(verify_intent_runtime_grants(&file, &config).is_ok());
        assert!(matches!(
            verify_intent_runtime_grants(&process, &config),
            Err(ToolError::ChangedInput)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn drifted_runtime_grant_rejects_commands_and_keeps_file_intents() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".nvm/versions/node/v22.3.0");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let node = root.join("bin/node");
        std::fs::write(&node, b"synthetic node executable").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (device, inode) =
            crate::tools::fs::identity(&crate::tools::fs::open_directory(&root).unwrap()).unwrap();
        let config = config(vec![RuntimeGrant {
            kind: super::super::toolchains::RuntimeKind::Node,
            manager: super::super::toolchains::RuntimeManager::Nvm,
            version: Some("v22.3.0".into()),
            path: root.clone(),
            device,
            inode,
        }]);
        let file = intent(ToolCall::Read {
            path: "selected.txt".into(),
            offset: None,
            limit: Some(32),
        });
        let process = intent(ToolCall::Command {
            program: "node".into(),
            args: Vec::new(),
            cwd: ".".into(),
        });
        assert!(verify_intent_runtime_grants(&process, &config).is_ok());

        // Keep the old directory alive while the path is recreated, so the new inode cannot be reused.
        std::fs::rename(&root, temp.path().join("previous-node")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        assert!(matches!(
            verify_intent_runtime_grants(&process, &config),
            Err(ToolError::ChangedInput)
        ));
        assert!(verify_intent_runtime_grants(&file, &config).is_ok());
    }

    fn cache_fixture(root: &Path) -> (crate::store::StateRoot, BuildCacheIdentity) {
        use std::os::unix::fs::MetadataExt;

        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let metadata = std::fs::metadata(&workspace).unwrap();
        let identity = BuildCacheIdentity::new(&workspace, metadata.dev(), metadata.ino()).unwrap();
        let state = crate::store::StateRoot::admit(&root.join("state")).unwrap();
        (state, identity)
    }

    fn empty_archive(identity: &BuildCacheIdentity) -> Vec<u8> {
        let mut bytes = b"ARANYCACHE\0\x01".to_vec();
        bytes.extend((identity.path.len() as u16).to_le_bytes());
        bytes.extend(identity.device.to_le_bytes());
        bytes.extend(identity.inode.to_le_bytes());
        bytes.extend(identity.path.as_bytes());
        bytes.extend([0; 12]);
        bytes
    }

    fn command_observation() -> ToolObservation {
        let mut intent = intent(ToolCall::Command {
            program: "cargo".into(),
            args: vec!["build".into()],
            cwd: ".".into(),
        });
        intent.policy_digest = [1; 32];
        intent.enforcement_digest = [2; 32];
        let guard = GuardReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        };
        ToolObservation {
            enforcement_cause: None,
            limit_cause: None,
            intent,
            disposition: ToolDisposition::Succeeded,
            output: r#"{"exit_code":0,"stdout":"built","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":5,"lines":1},"stderr_total":{"bytes":0,"lines":0}}"#
                .into(),
            attestation: Some(Attestation::Guard(guard)),
            approval_cause: None,
            cache_update: CacheUpdate::NotAttempted,
        }
    }

    fn cache_session(
        state: &crate::store::StateRoot,
        identity: &BuildCacheIdentity,
    ) -> BuildCacheSession {
        let lease = state.begin_build_cache(identity.clone()).unwrap();
        let candidate_bytes = lease.candidate_limit();
        BuildCacheSession {
            lease: Arc::new(lease),
            transfer: BuildCacheTransfer {
                identity: identity.clone(),
                exclusions: Vec::new(),
                view_bytes: crate::store::BUILD_CACHE_ARCHIVE_BYTES,
                candidate_bytes,
            },
        }
    }

    fn far_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(60)
    }

    #[tokio::test]
    async fn cache_publication_never_replaces_a_succeeded_command() {
        if crate::test_process::isolate_test() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (state, identity) = cache_fixture(temp.path());

        let session = cache_session(&state, &identity);
        let mut corrupt = session.lease.candidate_writer().unwrap();
        corrupt.write_all(b"not an archive").unwrap();
        drop(corrupt);
        let mut observation = command_observation();
        publish_cache_candidate(
            &mut observation,
            CacheCandidate::Received,
            Some(session),
            far_deadline(),
        )
        .await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert_eq!(observation.cache_update, CacheUpdate::Failed);

        let session = cache_session(&state, &identity);
        let mut valid = session.lease.candidate_writer().unwrap();
        valid.write_all(&empty_archive(&identity)).unwrap();
        valid.sync_all().unwrap();
        drop(valid);
        let mut observation = command_observation();
        publish_cache_candidate(
            &mut observation,
            CacheCandidate::Received,
            Some(session),
            far_deadline(),
        )
        .await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert_eq!(observation.cache_update, CacheUpdate::Published);
        assert!(observation.valid());
    }

    fn captured_archive(root: &Path, identity: &BuildCacheIdentity, marker: &[u8]) -> Vec<u8> {
        let source = root.join(format!("source-{}", marker.len()));
        std::fs::create_dir_all(source.join("target")).unwrap();
        std::fs::write(source.join("target/marker"), marker).unwrap();
        let source =
            cap_std::fs::Dir::open_ambient_dir(&source, cap_std::ambient_authority()).unwrap();
        let mut bytes = Vec::new();
        crate::store::capture_archive(&source, identity, &[], &mut bytes).unwrap();
        bytes
    }

    async fn publish_archive(
        state: &crate::store::StateRoot,
        identity: &BuildCacheIdentity,
        bytes: &[u8],
    ) {
        let session = cache_session(state, identity);
        let mut candidate = session.lease.candidate_writer().unwrap();
        candidate.write_all(bytes).unwrap();
        candidate.sync_all().unwrap();
        drop(candidate);
        let mut observation = command_observation();
        publish_cache_candidate(
            &mut observation,
            CacheCandidate::Received,
            Some(session),
            far_deadline(),
        )
        .await;
        assert_eq!(observation.cache_update, CacheUpdate::Published);
    }

    fn committed_archive(
        state: &crate::store::StateRoot,
        identity: &BuildCacheIdentity,
    ) -> Vec<u8> {
        let lease = state.begin_build_cache(identity.clone()).unwrap();
        let (mut file, length) = lease.archive().unwrap().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(length, bytes.len() as u64);
        bytes
    }

    #[tokio::test]
    async fn uncertain_commit_marker_keeps_the_command_and_recovers_the_prior_archive() {
        if crate::test_process::isolate_test() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (state, identity) = cache_fixture(temp.path());
        let prior = captured_archive(temp.path(), &identity, b"prior");
        let candidate = captured_archive(temp.path(), &identity, b"candidate");
        assert_ne!(prior, candidate);
        publish_archive(&state, &identity, &prior).await;

        let session = cache_session(&state, &identity);
        let mut valid = session.lease.candidate_writer().unwrap();
        valid.write_all(&candidate).unwrap();
        valid.sync_all().unwrap();
        drop(valid);
        crate::fault::FaultPlan::fail_commit_marker(&identity);
        let mut observation = command_observation();
        publish_cache_candidate(
            &mut observation,
            CacheCandidate::Received,
            Some(session),
            far_deadline(),
        )
        .await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert_eq!(observation.cache_update, CacheUpdate::Uncertain);
        assert!(observation.valid());
        assert!(observation.stops_run());

        // The commit marker never reached disk, so recovery rolls back to the prior archive.
        assert_eq!(committed_archive(&state, &identity), prior);
    }

    #[tokio::test]
    async fn candidate_sync_orders_publication_and_cancellation_keeps_the_prior_archive() {
        if crate::test_process::isolate_test() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (state, identity) = cache_fixture(temp.path());
        let prior = captured_archive(temp.path(), &identity, b"prior");
        let candidate = captured_archive(temp.path(), &identity, &vec![b'C'; 2 * 32 * 1024 + 1]);
        publish_archive(&state, &identity, &prior).await;

        for action in ["cancel", "timeout", "fail", "publish"] {
            let session = cache_session(&state, &identity);
            let mut stream = b"CACHE_CANDIDATE\n".to_vec();
            stream.extend((candidate.len() as u64).to_le_bytes());
            stream.extend(&candidate);
            stream.extend(b"CACHE_DONE\n");
            let mut reader = tokio::io::BufReader::new(stream.as_slice());
            let (entered, release, finished) =
                crate::fault::FaultPlan::gate_candidate_sync(&identity);
            if action == "fail" {
                crate::fault::FaultPlan::fail_candidate_sync(&identity);
            }
            let mut receive = Box::pin(receive_cache_candidate(&mut reader, &session.lease));
            tokio::select! {
                biased;
                _ = &mut receive => panic!("candidate returned before sync: {action}"),
                result = entered => {
                    result.unwrap();
                }
            }
            let (mut committed, _) = session.lease.archive().unwrap().unwrap();
            let mut bytes = Vec::new();
            committed.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, prior);

            if matches!(action, "cancel" | "timeout") {
                if action == "timeout" {
                    assert!(
                        tokio::time::timeout_at(tokio::time::Instant::now(), &mut receive)
                            .await
                            .is_err()
                    );
                }
                drop(receive);
                drop(session);
                let reservation = state
                    .path()
                    .join("build-cache")
                    .join(format!("reserve-{}.reserve", identity.token()));
                let candidate_path = state
                    .path()
                    .join("build-cache")
                    .join(format!("candidate-{}.arc", identity.token()));
                assert!(reservation.exists(), "reservation released during {action}");
                assert!(
                    candidate_path.exists(),
                    "candidate unlinked during {action}"
                );
                assert!(matches!(
                    state.begin_build_cache(identity.clone()),
                    Err(StoreError::BuildCacheBusy)
                ));
                let (_, held_capacity) = state
                    .effective_build_cache_bytes(&identity, MAX_BUILD_CACHE_BYTES)
                    .unwrap();
                release.send(()).unwrap();
                finished.await.unwrap();
                assert!(!reservation.exists(), "reservation retained after {action}");
                assert!(
                    !candidate_path.exists(),
                    "candidate retained after {action}"
                );
                let (_, released_capacity) = state
                    .effective_build_cache_bytes(&identity, MAX_BUILD_CACHE_BYTES)
                    .unwrap();
                assert!(released_capacity > held_capacity);
                assert_eq!(committed_archive(&state, &identity), prior);
                continue;
            }
            release.send(()).unwrap();
            let outcome = receive.await.unwrap();
            assert!(
                outcome
                    == if action == "fail" {
                        CacheCandidate::Failed
                    } else {
                        CacheCandidate::Received
                    },
                "unexpected candidate outcome: {action}"
            );
            let mut trailer = String::new();
            reader.read_line(&mut trailer).await.unwrap();
            assert_eq!(trailer, "CACHE_DONE\n");
            let mut observation = command_observation();
            publish_cache_candidate(&mut observation, outcome, Some(session), far_deadline()).await;
            assert_eq!(observation.disposition, ToolDisposition::Succeeded);
            assert_eq!(
                observation.cache_update,
                if action == "fail" {
                    CacheUpdate::Failed
                } else {
                    CacheUpdate::Published
                }
            );
            assert_eq!(
                &committed_archive(&state, &identity),
                if action == "fail" { &prior } else { &candidate }
            );
        }
    }

    struct FailAfter {
        remaining: usize,
        flush_fails: bool,
    }

    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let accepted = bytes.len().min(self.remaining);
            self.remaining -= accepted;
            Ok(accepted)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.flush_fails {
                Err(std::io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn candidate_read_error_waits_for_the_writer_to_stop() {
        struct GatedWriter {
            entered: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
            stopped: Option<tokio::sync::oneshot::Sender<()>>,
        }

        impl Write for GatedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let _ = self.entered.take().unwrap().send(());
                self.release
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(std::io::Error::other)?;
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                panic!("truncated candidate reached flush");
            }
        }

        impl Drop for GatedWriter {
            fn drop(&mut self) {
                let _ = self.stopped.take().unwrap().send(());
            }
        }

        let (entered, writing) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::sync_channel(1);
        let (stopped, mut completion) = tokio::sync::oneshot::channel();
        let archive = vec![b'A'; 32 * 1024];
        let mut reader = archive.as_slice();
        let mut receive = Box::pin(drain_candidate(
            &mut reader,
            archive.len() as u64 + 1,
            move || {
                Some(GatedWriter {
                    entered: Some(entered),
                    release: gate,
                    stopped: Some(stopped),
                })
            },
            |_| panic!("truncated candidate reached sync"),
        ));
        let early = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                result = &mut receive => Some(result),
                result = writing => {
                    result.unwrap();
                    None
                }
            }
        })
        .await;
        release.send(()).unwrap();
        let early = early.unwrap();
        let returned_early = early.is_some();
        let result = match early {
            Some(result) => result,
            None => tokio::time::timeout(Duration::from_secs(5), receive)
                .await
                .unwrap(),
        };
        let stopped_before_return = completion.try_recv().is_ok();
        if !stopped_before_return {
            tokio::time::timeout(Duration::from_secs(5), completion)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(
            !returned_early,
            "read error returned while the writer was held"
        );
        assert!(
            stopped_before_return,
            "staging survived the read error return"
        );
        assert!(matches!(result, Err(ToolError::Operation)));
    }

    #[tokio::test]
    async fn candidate_drain_keeps_framing_and_skips_sync_after_failures() {
        let archive = vec![b'A'; 2 * 32 * 1024 + 1];
        let reactor = std::thread::current().id();
        for (create_fails, remaining, flush_fails, truncated) in [
            (true, usize::MAX, false, false),
            (false, 4, false, false),
            (false, usize::MAX, true, false),
            (false, usize::MAX, false, true),
        ] {
            let mut stream = archive.clone();
            if truncated {
                stream.pop();
            } else {
                stream.extend(b"CACHE_DONE\n");
            }
            let mut reader = tokio::io::BufReader::new(stream.as_slice());
            let (synced, completion) = tokio::sync::oneshot::channel();
            let staged = drain_candidate(
                &mut reader,
                archive.len() as u64,
                move || {
                    assert_ne!(std::thread::current().id(), reactor);
                    (!create_fails).then_some(FailAfter {
                        remaining,
                        flush_fails,
                    })
                },
                move |_| {
                    let _ = synced.send(());
                    true
                },
            )
            .await;
            assert!(
                completion.await.is_err(),
                "failed staging reached candidate sync"
            );
            if truncated {
                assert!(matches!(staged, Err(ToolError::Operation)));
                continue;
            }
            assert!(!staged.unwrap());
            let mut trailer = String::new();
            reader.read_line(&mut trailer).await.unwrap();
            assert_eq!(trailer, "CACHE_DONE\n");
        }
    }

    #[tokio::test]
    async fn candidate_staging_failure_drains_the_archive_and_keeps_the_prior_archive() {
        if crate::test_process::isolate_test() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (state, identity) = cache_fixture(temp.path());
        let session = cache_session(&state, &identity);
        let mut valid = session.lease.candidate_writer().unwrap();
        valid.write_all(&empty_archive(&identity)).unwrap();
        valid.sync_all().unwrap();
        drop(valid);
        let mut observation = command_observation();
        publish_cache_candidate(
            &mut observation,
            CacheCandidate::Received,
            Some(session),
            far_deadline(),
        )
        .await;
        assert_eq!(observation.cache_update, CacheUpdate::Published);

        let lease = Arc::new(state.begin_build_cache(identity.clone()).unwrap());
        // Occupies the candidate name, so the create_new staging step fails.
        drop(lease.candidate_writer().unwrap());

        let archive = b"ARCHIVE\nbytes";
        let mut stream = b"CACHE_CANDIDATE\n".to_vec();
        stream.extend((archive.len() as u64).to_le_bytes());
        stream.extend(archive);
        stream.extend(b"CACHE_DONE\n");
        let mut reader = tokio::io::BufReader::new(stream.as_slice());
        let candidate = receive_cache_candidate(&mut reader, &lease).await.unwrap();
        assert!(matches!(candidate, CacheCandidate::Failed));
        let mut trailer = String::new();
        reader.read_line(&mut trailer).await.unwrap();
        assert_eq!(trailer, "CACHE_DONE\n");
        drop(lease);

        let lease = state.begin_build_cache(identity.clone()).unwrap();
        let (mut file, length) = lease.archive().unwrap().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(length, bytes.len() as u64);
        assert_eq!(bytes, empty_archive(&identity));
    }

    #[test]
    fn stale_git_locks_lead_the_uncertain_observation_the_owner_sees() {
        let commit = intent(ToolCall::GitCommit {
            message: "m".into(),
            paths: vec![".".into()],
            staged: false,
        });
        let report = ToolError::StaleGitLocks(".git/index.lock, .git/refs/heads/main.lock".into());
        let observation = report_stale_locks(
            super::super::uncertain_observation(
                commit,
                ToolError::Uncertain,
                CacheUpdate::NotApplicable,
            ),
            &report.to_string(),
        );
        assert_eq!(observation.disposition, ToolDisposition::Uncertain);
        assert!(
            observation.output.starts_with(
                "Git locks left by an interrupted write: .git/index.lock, .git/refs/heads/main.lock; remove them after checking that no git process is running. "
            ),
            "{}",
            observation.output
        );
        assert!(observation.output.contains("Do not retry automatically"));
    }

    #[test]
    fn stale_locks_turn_a_failed_observation_uncertain() {
        let commit = intent(ToolCall::GitCommit {
            message: "m".into(),
            paths: vec![".".into()],
            staged: false,
        });
        let mut failed = super::super::uncertain_observation(
            commit.clone(),
            ToolError::Uncertain,
            CacheUpdate::NotApplicable,
        );
        failed.disposition = ToolDisposition::Failed;
        let settled = uncertain_after_stale(&commit, Ok(failed), CacheUpdate::NotApplicable);
        assert_eq!(settled.disposition, ToolDisposition::Uncertain);
    }

    #[test]
    fn guard_helper_refuses_an_unguarded_intent() {
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("sh");
        std::fs::copy(std::fs::canonicalize("/bin/sh").unwrap(), &shell).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "version": 1,
            "workspace_paths": ["src"],
            "write": false,
            "commands": [{
                "name": "sh",
                "executable": shell.to_str().unwrap(),
                "sha256": super::super::types::hex_digest(&std::fs::read(&shell).unwrap()),
                "interpreter": true,
                "inputs": []
            }],
            "skills": [],
            "mcp": []
        }))
        .unwrap();
        let task = |call: ToolCall| {
            let probe = Task {
                definition: None,
                worker: None,
                handover: None,
                config: config.clone(),
                intent: EffectIntent {
                    host: None,
                    target: crate::session::worker::EffectTarget::Workspace,
                    id: uuid::Uuid::now_v7(),
                    run_id: crate::session::RunId::new(),
                    agent_run_id: crate::session::AgentRunId::new(),
                    policy_digest: config.digest(),
                    enforcement_digest: [0; 32],
                    workspace_device: 1,
                    workspace_inode: 2,
                    call,
                    limits: ToolLimits::default(),
                    use_count: 1,
                },
                cache: None,
                hook_stdin: None,
                hidden: Default::default(),
                #[cfg(target_os = "linux")]
                parent_namespaces: profile::Namespaces::read().unwrap(),
                #[cfg(target_os = "macos")]
                workspace: std::path::PathBuf::new(),
                #[cfg(target_os = "macos")]
                guard_executable: std::path::PathBuf::new(),
                #[cfg(target_os = "macos")]
                system_programs: None,
            };
            let digest = probe.enforcement_digest().unwrap();
            let mut task = probe;
            task.intent.enforcement_digest = digest;
            task
        };
        let args = vec!["-c".to_owned(), "exit 7".to_owned()];
        let command = ToolCall::Command {
            program: "sh".into(),
            args: args.clone(),
            cwd: String::new(),
        };
        assert!(validate_task(&task(command.clone())).is_ok());
        assert!(matches!(
            validate_task(&task(ToolCall::CommandUnguarded {
                program: "sh".into(),
                args,
                cwd: String::new(),
                justification: "Run the failed command once".into(),
            })),
            Err(ToolError::Configuration)
        ));
    }
}

#[cfg(test)]
mod command_output_tests {
    use super::*;

    #[tokio::test]
    async fn command_capture_drains_large_and_invalid_output_without_changing_exit_status() {
        for script in [
            "printf HEAD; printf '%1048568s' ''; printf TAIL; exit 7",
            "printf '\\377\\n'; printf err >&2",
        ] {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let (status, out, err) =
                capture_command(command.spawn().unwrap(), 4096, Duration::from_secs(10))
                    .await
                    .unwrap();
            let output = super::super::process::fit_command(
                status.and_then(|status| status.code()),
                &out,
                &err,
                Default::default(),
                4096,
                super::super::process::Truncation::Allow,
            )
            .unwrap();
            let result: super::super::types::CommandResult = serde_json::from_str(&output).unwrap();
            assert!(result.valid());
            if script.ends_with("exit 7") {
                assert_eq!(result.exit_code, Some(7));
                assert_eq!(result.stdout_total.bytes, 1048576);
                assert!(result.stdout.starts_with("HEAD") && result.stdout.ends_with("TAIL"));
            } else {
                assert_eq!(result.exit_code, Some(0));
                assert_eq!(result.stdout, "\u{fffd}\n");
                assert_eq!(result.stdout_total.bytes, 2);
                assert_eq!(result.stderr, "err");
            }
        }
    }
}
