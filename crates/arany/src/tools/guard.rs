use super::catalog::{Family, Site};
use super::config::Config;
use super::toolchains::{RuntimeGrant, validate_selection};
use super::types::*;
use super::{Cancellation, ToolError, ToolRuntime};
use crate::store::{BuildCacheIdentity, BuildCacheLease, StoreError};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::{Child, Command};

const CAPTURE_BYTES: usize = 48 * 1024;
pub(super) const TASK_BYTES_CAP: usize = 96 * 1024;
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
mod linux;
#[cfg(target_os = "linux")]
mod profile;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::fixture::{Span as FixtureSpan, Stage as FixtureStage};
#[cfg(target_os = "macos")]
pub(super) use macos::{SystemPrograms, macos_enforcement_digest};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod admission;

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
    pub(super) lease: BuildCacheLease,
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
        return Ok(None);
    };
    let identity = BuildCacheIdentity::new(
        &runtime.workspace,
        intent.workspace_device,
        intent.workspace_inode,
    )
    .map_err(map_cache_error)?;
    let exclusions = cache_exclusions(&runtime.config.runtime_grants)?;
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
        lease,
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
    if observation.disposition != ToolDisposition::Succeeded {
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
                let mut cache = cache;
                let mut candidate = match cache.lease.open_candidate() {
                    Ok(candidate) => candidate,
                    Err(StoreError::StorageFull) if !cache.lease.publication_uncertain() => {
                        return CacheUpdate::Rejected;
                    }
                    Err(_) if cache.lease.publication_uncertain() => return CacheUpdate::Uncertain,
                    Err(_) => return CacheUpdate::Failed,
                };
                match cache
                    .lease
                    .publish(&mut candidate, &cache.transfer.exclusions)
                {
                    Ok(()) => CacheUpdate::Published,
                    Err(StoreError::StorageFull) if !cache.lease.publication_uncertain() => {
                        CacheUpdate::Rejected
                    }
                    Err(_) if cache.lease.publication_uncertain() => CacheUpdate::Uncertain,
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
pub(super) enum CleanupCheck {
    Inspection,
    ListingUnavailable,
    DeviceUnlisted,
    DetachFailed,
    ImageStillListed,
    MountStillPresent,
}

impl CleanupCheck {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Inspection => "inspection",
            Self::ListingUnavailable => "listing unavailable",
            Self::DeviceUnlisted => "device unlisted before detach",
            Self::DetachFailed => "detach failed",
            Self::ImageStillListed => "image still listed",
            Self::MountStillPresent => "mount still present",
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
    check: Option<CleanupCheck>,
}

impl std::fmt::Display for BootstrapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.stage.label())
    }
}

impl BootstrapFailure {
    pub(super) fn check(&self) -> Option<&'static str> {
        self.check.map(CleanupCheck::label)
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
                definition: None,
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
    let bytes = read_line_capped(reader, MAX_TOOL_OBSERVATION_BYTES).await?;
    let observation = parse_observation(&bytes)?;
    if observation.cache_update != CacheUpdate::NotApplicable
        || !observation.attested_for(intent)
        || !read_capped(&mut *reader, 1).await?.is_empty()
    {
        return Err(ToolError::Operation);
    }
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
    let mut buffer = [0; 32 * 1024];
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
    lease: &BuildCacheLease,
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
    let staged = drain_candidate(reader, length, lease.candidate_writer().ok()).await?;
    let Some(file) = staged else {
        return Ok(CacheCandidate::Failed);
    };
    #[cfg(any(test, feature = "test-support"))]
    let synced =
        crate::fault::FaultPlan::candidate_sync(lease.token()).and_then(|()| file.sync_all());
    #[cfg(not(any(test, feature = "test-support")))]
    let synced = file.sync_all();
    if synced.is_err() {
        return Ok(CacheCandidate::Failed);
    }
    Ok(CacheCandidate::Received)
}

// Keep draining after a staging write failure so the archive stream stays aligned.
async fn drain_candidate<R: tokio::io::AsyncRead + Unpin, W: Write>(
    reader: &mut R,
    length: u64,
    mut staged: Option<W>,
) -> Result<Option<W>, ToolError> {
    use tokio::io::AsyncReadExt;

    let mut buffer = [0; 32 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        reader
            .read_exact(&mut buffer[..wanted])
            .await
            .map_err(|_| ToolError::Operation)?;
        if staged
            .as_mut()
            .is_some_and(|file| file.write_all(&buffer[..wanted]).is_err())
        {
            staged = None;
        }
        remaining -= wanted as u64;
    }
    Ok(staged)
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

pub(super) fn spawn_before(
    command: &mut Command,
    deadline: tokio::time::Instant,
) -> Result<Child, ToolError> {
    if tokio::time::Instant::now() >= deadline {
        return Err(ToolError::Limit);
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
        Err(_) => ToolError::Limit,
    };
    let _ = child.start_kill();
    let _ = tokio::time::timeout(PAYLOAD_KILL_WAIT, reap_owned_child(&mut child)).await;
    Err(error)
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
    let workspace = if task.intent.call.is_process() {
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
    pub(super) definition: Option<Vec<u8>>,
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
    if task.intent.target != crate::session::worker::EffectTarget::Workspace
        || !task.intent.valid()
        || task.intent.call.facts().site != Site::Guard
        || task.intent.policy_digest != task.config.digest()
        || !task.config.allows(&task.intent.call)
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
        if (matches!(family, Family::File | Family::Git)
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
        #[cfg(target_os = "linux")]
        {
            stage = BootstrapStage::Capabilities;
            linux::harden()?;
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
        // Intent expiry ends the capacity wait; admitted preparation and GO have their own deadlines.
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
        drop(stdin);
        stage = BootstrapStage::Payload;
        #[cfg(not(target_os = "macos"))]
        if task.intent.call.is_process() {
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
            definition,
            runtime_started,
            #[cfg(target_os = "linux")]
            parent_namespaces: &task.parent_namespaces,
        };
        let result = runtime.block_on(super::dispatch::run(&site));
        let (disposition, output, enforcement_cause) =
            super::dispatch::outcome(&task.intent, result);
        stage = BootstrapStage::Receipt;
        let receipt = GuardReceipt {
            contract_version: 1,
            intent_digest: task.intent.digest(),
            enforcement_digest: task.intent.enforcement_digest,
            limits: task.intent.limits.clone(),
        };
        let mut observation = ToolObservation {
            enforcement_cause,
            intent: task.intent,
            disposition,
            output,
            attestation: Some(Attestation::Guard(receipt)),
            cache_update: if task.cache.is_some() {
                crate::tools::CacheUpdate::NotAttempted
            } else {
                crate::tools::CacheUpdate::NotApplicable
            },
        };
        if !observation.valid() {
            observation.disposition = ToolDisposition::Limit;
            observation.enforcement_cause = None;
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        if !observation.valid() {
            return Err(ToolError::Limit);
        }
        serde_json::to_writer(&mut stdout, &observation).map_err(|_| ToolError::Operation)?;
        stdout
            .write_all(b"\n")
            .and_then(|()| stdout.flush())
            .map_err(|_| ToolError::Operation)?;
        #[cfg(target_os = "linux")]
        if let Some(cache) = &task.cache {
            send_cache_candidate(cache, observation.disposition, &mut stdout, &root)?;
        }
        Ok(())
    })();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        let _ = serde_json::to_writer(
            std::io::stderr().lock(),
            &BootstrapFailure { stage, check: None },
        );
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
        commands: Vec::new(),
        skills: Vec::new(),
        mcp: Vec::new(),
        git: None,
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
    let mut command = Command::new(executable);
    command
        .args(args.iter().map(|arg| {
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
        }))
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
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            definition: Some(base64::engine::general_purpose::STANDARD.encode(&raw)),
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
                serde_json::json!({"name": format!("program{index}"), "executable": format!("/usr/bin/{}", "x".repeat(200)), "sha256": "0".repeat(64), "interpreter": false, "inputs": []})
            })
            .collect();
        let config: Config = serde_json::from_value(serde_json::json!({"version":1,"workspace_paths":["."],"write":false,"commands":commands,"skills":[],"mcp":[]})).unwrap();
        let task = |config: Config| Task {
            config,
            intent: intent(call.clone()),
            cache: None,
            hook_stdin: None,
            definition: Some(base64::engine::general_purpose::STANDARD.encode(&raw)),
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
        assert!(matches!(
            parse_observation(br#"{"stage":"made_up"}"#),
            Err(ToolError::Operation)
        ));
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

    #[tokio::test]
    async fn an_exhausted_runtime_is_a_limit_without_a_spawn() {
        let mut missing = Command::new("/nonexistent/arany-test-program");
        assert!(matches!(
            spawn_before(&mut missing, tokio::time::Instant::now()),
            Err(ToolError::Limit)
        ));
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
        let (disposition, output, cause) =
            super::super::dispatch::outcome(&observation.intent, Ok(output));
        assert_eq!(disposition, ToolDisposition::Failed);
        assert_eq!(cause, None, "program output cannot attest a Guard denial");
        observation.disposition = disposition;
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
            let (_, _, cause) = super::super::dispatch::outcome(&observation.intent, Err(error));
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
            let (_, _, cause) = super::super::dispatch::outcome(&intent, Ok(
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
        let (disposition, output, cause) =
            super::super::dispatch::outcome(&observation.intent, rejected.map(|_| String::new()));
        assert_eq!(cause, Some(EnforcementCause::FilesystemDenied));
        observation.disposition = disposition;
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
            expires_at_ms: 1,
            use_count: 1,
        }
    }

    fn config(runtime_grants: Vec<RuntimeGrant>) -> Config {
        Config {
            version: 1,
            workspace_paths: vec![".".into()],
            write: false,
            commands: Vec::new(),
            skills: Vec::new(),
            mcp: Vec::new(),
            runtime_grants,
            git: None,
        }
    }

    #[test]
    fn runtime_mount_rejection_reports_its_own_bootstrap_stage() {
        let failure = BootstrapFailure {
            stage: BootstrapStage::RuntimeMounts,
            check: None,
        };
        assert_eq!(failure.failure_stage(), "runtime mount topology");
        assert_eq!(failure.to_string(), "runtime mount topology");
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
            offset: 0,
            limit: 32,
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
            offset: 0,
            limit: 32,
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
            intent,
            disposition: ToolDisposition::Succeeded,
            output: r#"{"exit_code":0,"stdout":"built","stderr":"","changeset":{"files":[]}}"#
                .into(),
            attestation: Some(Attestation::Guard(guard)),
            cache_update: CacheUpdate::NotAttempted,
        }
    }

    fn cache_session(
        state: &crate::store::StateRoot,
        identity: &BuildCacheIdentity,
    ) -> BuildCacheSession {
        let lease = crate::store::begin_released(state, identity.clone()).unwrap();
        let candidate_bytes = lease.candidate_limit();
        BuildCacheSession {
            lease,
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
        let lease = crate::store::begin_released(state, identity.clone()).unwrap();
        let (mut file, length) = lease.archive().unwrap().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(length, bytes.len() as u64);
        bytes
    }

    #[tokio::test]
    async fn uncertain_commit_marker_keeps_the_command_and_recovers_the_prior_archive() {
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
    async fn candidate_sync_failure_drains_the_archive_and_keeps_the_prior_archive() {
        let temp = tempfile::tempdir().unwrap();
        let (state, identity) = cache_fixture(temp.path());
        let prior = captured_archive(temp.path(), &identity, b"prior");
        let candidate = captured_archive(temp.path(), &identity, b"candidate");
        publish_archive(&state, &identity, &prior).await;

        let lease = crate::store::begin_released(&state, identity.clone()).unwrap();
        let mut stream = b"CACHE_CANDIDATE\n".to_vec();
        stream.extend((candidate.len() as u64).to_le_bytes());
        stream.extend(&candidate);
        stream.extend(b"CACHE_DONE\n");
        let mut reader = tokio::io::BufReader::new(stream.as_slice());
        crate::fault::FaultPlan::fail_candidate_sync(&identity);
        let outcome = receive_cache_candidate(&mut reader, &lease).await.unwrap();
        assert!(matches!(outcome, CacheCandidate::Failed));
        let mut trailer = String::new();
        reader.read_line(&mut trailer).await.unwrap();
        assert_eq!(trailer, "CACHE_DONE\n");
        drop(lease);

        assert_eq!(committed_archive(&state, &identity), prior);
    }

    /// Accepts `remaining` bytes, then reports zero-length writes so `write_all` fails.
    struct FailAfter {
        remaining: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let accepted = bytes.len().min(self.remaining);
            self.remaining -= accepted;
            Ok(accepted)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn candidate_drain_consumes_the_archive_after_a_write_failure() {
        let archive = b"ARCHIVE\nbytes";
        let mut stream = archive.to_vec();
        stream.extend(b"CACHE_DONE\n");
        let mut reader = tokio::io::BufReader::new(stream.as_slice());
        let staged = drain_candidate(
            &mut reader,
            archive.len() as u64,
            Some(FailAfter { remaining: 4 }),
        )
        .await
        .unwrap();
        assert!(staged.is_none());
        let mut trailer = String::new();
        reader.read_line(&mut trailer).await.unwrap();
        assert_eq!(trailer, "CACHE_DONE\n");
    }

    #[tokio::test]
    async fn candidate_staging_failure_drains_the_archive_and_keeps_the_prior_archive() {
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

        let lease = crate::store::begin_released(&state, identity.clone()).unwrap();
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

        let lease = crate::store::begin_released(&state, identity.clone()).unwrap();
        let (mut file, length) = lease.archive().unwrap().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(length, bytes.len() as u64);
        assert_eq!(bytes, empty_archive(&identity));
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
                config: config.clone(),
                intent: EffectIntent {
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
                    expires_at_ms: crate::tools::now_ms() + 60_000,
                    use_count: 1,
                },
                cache: None,
                hook_stdin: None,
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
        let mut admitted = task(command);
        admitted.intent.expires_at_ms = 1;
        assert!(
            validate_task(&admitted).is_ok(),
            "preparation after admission is not charged to the intent expiry"
        );
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
