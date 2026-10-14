use super::config::Config;
use super::toolchains::{RuntimeGrant, validate_selection};
use super::types::*;
use super::{ToolError, ToolRuntime, now_ms};
use crate::engine::RunCancellation;
use crate::store::{BuildCacheIdentity, BuildCacheLease, StoreError};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::{Child, Command};

const CAPTURE_BYTES: usize = 48 * 1024;
const TASK_BYTES_CAP: usize = 96 * 1024;
pub(super) const RUNTIME_ROOT: &str = "/usr";

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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    config: Config,
    intent: EffectIntent,
    cache: Option<BuildCacheTransfer>,
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
        let path = grant.kind.cache_root().ok_or(ToolError::Configuration)?;
        exclusions.push(path.to_owned());
    }
    exclusions.sort();
    exclusions.dedup();
    Ok(exclusions)
}

fn map_cache_error(error: StoreError) -> ToolError {
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
    if update == CacheUpdate::Uncertain {
        observation.disposition = ToolDisposition::Uncertain;
    }
    if !observation.valid() {
        observation.cache_update = CacheUpdate::Uncertain;
        observation.disposition = ToolDisposition::Uncertain;
    }
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
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapFailure {
    stage: BootstrapStage,
}

impl std::fmt::Display for BootstrapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.stage.label())
    }
}

impl BootstrapFailure {
    pub(super) fn failure_stage(&self) -> &'static str {
        match self.stage {
            BootstrapStage::Admission => "admission",
            BootstrapStage::Workspace => "pinned input",
            BootstrapStage::RuntimeMounts => "runtime mount topology",
            BootstrapStage::Capabilities => "native protection",
            BootstrapStage::Handshake => "handshake",
            BootstrapStage::Payload => "execution",
            BootstrapStage::Receipt => "receipt",
        }
    }
}

/// A Guard stage that failed after bootstrap reported success, or outside its child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardStage {
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

fn verify_runtime_grants(config: &Config) -> Result<(), ToolError> {
    for grant in &config.runtime_grants {
        #[cfg(target_os = "linux")]
        grant.verify_at(&grant.mounted_path()?)?;
        #[cfg(target_os = "macos")]
        grant.verify_host()?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn verify_intent_runtime_grants(intent: &EffectIntent, config: &Config) -> Result<(), ToolError> {
    if intent.call.is_process() {
        verify_runtime_grants(config)
    } else {
        Ok(())
    }
}

fn attest_runtime_mounts(
    call: &ToolCall,
    grants: &[RuntimeGrant],
    stage: &mut BootstrapStage,
) -> Result<(), ToolError> {
    if !call.is_process() {
        return Ok(());
    }
    *stage = BootstrapStage::RuntimeMounts;
    #[cfg(target_os = "linux")]
    linux::verify_runtime_mounts(grants)?;
    #[cfg(target_os = "macos")]
    macos::verify_runtime_mounts(grants)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(ToolError::ProtectionUnavailable);
    Ok(())
}

pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    cancellation: RunCancellation,
) -> Result<ToolObservation, ToolError> {
    let mut dispatched = false;
    #[cfg(target_os = "macos")]
    let result = macos::execute_inner(runtime, intent, cancellation, &mut dispatched).await;
    #[cfg(target_os = "linux")]
    let result = linux::execute_inner(runtime, intent, cancellation, &mut dispatched).await;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = Err(ToolError::ProtectionUnavailable);
    match result {
        Err(error) if !dispatched => Ok(super::unstarted_observation(intent.clone(), error)),
        result => result,
    }
}

/// Waits briefly for the launcher's bounded stderr; `None` means it did not reach EOF.
pub(super) async fn drain_stderr(
    task: &mut tokio::task::JoinHandle<Result<Vec<u8>, ToolError>>,
) -> Option<Vec<u8>> {
    match tokio::time::timeout(Duration::from_secs(2), &mut *task).await {
        Ok(Ok(Ok(stderr))) => Some(stderr),
        Ok(_) => None,
        Err(_) => {
            task.abort();
            let _ = task.await;
            None
        }
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
    let bytes = read_line_capped(reader, CAPTURE_BYTES).await?;
    let observation: ToolObservation =
        serde_json::from_slice(&bytes).map_err(|_| ToolError::Operation)?;
    if observation.cache_update != CacheUpdate::NotApplicable
        || !observation.attested_for(intent)
        || !read_capped(&mut *reader, 1).await?.is_empty()
    {
        return Err(ToolError::Operation);
    }
    Ok(observation)
}

pub(super) async fn read_observation_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<ToolObservation, ToolError> {
    let bytes = read_line_capped(reader, CAPTURE_BYTES).await?;
    let observation: ToolObservation =
        serde_json::from_slice(&bytes).map_err(|_| ToolError::Operation)?;
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let file = lease.candidate_writer().map_err(map_cache_error)?;
    let mut file = tokio::fs::File::from_std(file.into_std());
    let mut buffer = [0; 32 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        reader
            .read_exact(&mut buffer[..wanted])
            .await
            .map_err(|_| ToolError::Operation)?;
        file.write_all(&buffer[..wanted])
            .await
            .map_err(|_| ToolError::Operation)?;
        remaining -= wanted as u64;
    }
    file.sync_all().await.map_err(|_| ToolError::Operation)?;
    Ok(CacheCandidate::Received)
}

/// Reaps a child this process owns. On macOS it closes stdin first and polls `try_wait`
/// at the supervision interval, because Seatbelt blocks Tokio's signal wake socket;
/// elsewhere it awaits exit.
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

async fn capture(
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
    let _ = tokio::time::timeout(Duration::from_secs(1), reap_owned_child(&mut child)).await;
    Err(error)
}

fn encode_task(task: &Task) -> Result<Vec<u8>, ToolError> {
    let mut bytes = serde_json::to_vec(task).map_err(|_| ToolError::Configuration)?;
    if bytes.len() > TASK_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_task(reader: &mut std::io::BufReader<impl Read>) -> Result<Task, ToolError> {
    let mut task = Vec::new();
    std::io::BufRead::read_until(
        &mut Read::take(&mut *reader, (TASK_BYTES_CAP + 1) as u64),
        b'\n',
        &mut task,
    )
    .map_err(|_| ToolError::Operation)?;
    if task.last() != Some(&b'\n') {
        return Err(if task.len() > TASK_BYTES_CAP {
            ToolError::Limit
        } else {
            ToolError::Operation
        });
    }
    if !reader.buffer().is_empty() {
        return Err(ToolError::Configuration);
    }
    serde_json::from_slice(&task).map_err(|_| ToolError::Configuration)
}

/// Returns the payload root and the Workspace this helper's call operates on.
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

pub(super) fn helper_main() -> ExitCode {
    let mut stage = BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        #[cfg(target_os = "macos")]
        macos::bootstrap()?;
        let mut stdin = std::io::BufReader::new(std::io::stdin().lock());
        let task = read_task(&mut stdin)?;
        task.config.validate()?;
        if let Some(cache) = &task.cache {
            cache.validate(&task.intent, &task.config)?;
        }
        verify_intent_runtime_grants(&task.intent, &task.config)?;
        if !task.intent.valid()
            || task.intent.policy_digest != task.config.digest()
            || task.intent.expires_at_ms <= now_ms()
            || !task.config.allows(&task.intent.call)
            || task.intent.enforcement_digest != task.enforcement_digest()?
            || task.intent.limits.resource_profile != ToolLimits::default().resource_profile
        {
            return Err(ToolError::Configuration);
        }
        let (root, workspace) = bootstrap_locations(&task, &mut stage)?;
        attest_runtime_mounts(&task.intent.call, &task.config.runtime_grants, &mut stage)?;
        #[cfg(target_os = "linux")]
        if let Some(cache) = &task.cache {
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
                let mut archive = Read::take(&mut stdin, length);
                crate::store::restore_archive_with_limit(
                    &mut archive,
                    &cache_dir,
                    &cache.identity,
                    &cache.exclusions,
                    cache.view_bytes,
                )
                .map_err(map_cache_error)?;
            }
        }
        let workspace = workspace.as_path();
        stage = BootstrapStage::Workspace;
        if (matches!(
            task.intent.call,
            ToolCall::List { .. }
                | ToolCall::Read { .. }
                | ToolCall::Search { .. }
                | ToolCall::Write { .. }
                | ToolCall::Edit { .. }
                | ToolCall::Mkdir { .. }
                | ToolCall::ApplyChangeset { .. }
        ) || cfg!(target_os = "macos") && matches!(task.intent.call, ToolCall::Skill { .. }))
            && super::fs::identity(&super::fs::open_directory(workspace)?)?
                != (task.intent.workspace_device, task.intent.workspace_inode)
        {
            return Err(ToolError::ChangedInput);
        }
        let skill = if matches!(task.intent.call, ToolCall::Skill { .. }) {
            Some(load_skill_bytes(&task)?)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        {
            attest_runtime_mounts(&task.intent.call, &task.config.runtime_grants, &mut stage)?;
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
        if go != *b"GO\n" || task.intent.expires_at_ms <= now_ms() {
            return Err(ToolError::Configuration);
        }
        verify_intent_runtime_grants(&task.intent, &task.config)?;
        attest_runtime_mounts(&task.intent.call, &task.config.runtime_grants, &mut stage)?;
        stage = BootstrapStage::Handshake;
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
        let result = runtime.block_on(async {
            match &task.intent.call {
                ToolCall::Command { program, args, cwd } => {
                    let definition = task
                        .config
                        .commands
                        .iter()
                        .find(|value| &value.name == program)
                        .ok_or(ToolError::Configuration)?;
                    #[cfg(target_os = "linux")]
                    let before = super::fs::command_snapshot(workspace)?;
                    let mut command = payload(
                        definition,
                        args,
                        cwd,
                        &root,
                        &task.config.runtime_grants,
                        task.intent.limits.cache_bytes > 0,
                    )?;
                    let (status, out, err) = capture(
                        command.spawn().map_err(|_| ToolError::Operation)?,
                        (task.intent.limits.result_bytes as usize / 2).saturating_sub(128),
                        Duration::from_millis(u64::from(task.intent.limits.runtime_ms)),
                    )
                    .await?;
                    #[cfg(target_os = "linux")]
                    let changeset = {
                        linux::stop_command_descendants(&task.parent_namespaces).await?;
                        super::fs::command_changeset(workspace, &before)?
                    };
                    #[cfg(not(target_os = "linux"))]
                    let changeset = CommandChangeset::default();
                    let output = CommandResult {
                        exit_code: status.code(),
                        stdout: String::from_utf8(out).map_err(|_| ToolError::Operation)?,
                        stderr: String::from_utf8(err).map_err(|_| ToolError::Operation)?,
                        changeset,
                    };
                    if !output.valid() {
                        return Err(ToolError::Limit);
                    }
                    serde_json::to_string(&output).map_err(|_| ToolError::Operation)
                }
                ToolCall::Skill { name, resource } => skill_text(
                    name,
                    resource.as_deref(),
                    skill.ok_or(ToolError::Configuration)?,
                ),
                ToolCall::McpList { .. } | ToolCall::McpCall { .. } => {
                    super::mcp::execute(&task.intent.call, &task.config, &root).await
                }
                call => super::fs::native(workspace, call, &task.config),
            }
        });
        let (disposition, output) = match result {
            Ok(output) if output.len() <= task.intent.limits.result_bytes as usize => {
                let failed = match &task.intent.call {
                    ToolCall::Command { .. } => serde_json::from_str::<serde_json::Value>(&output)
                        .map_or(true, |value| value["exit_code"] != 0),
                    ToolCall::McpCall { .. } => serde_json::from_str::<serde_json::Value>(&output)
                        .map_or(true, |value| value["is_error"] == true),
                    _ => false,
                };
                (
                    if failed {
                        ToolDisposition::Failed
                    } else {
                        ToolDisposition::Succeeded
                    },
                    output,
                )
            }
            Ok(_) | Err(ToolError::Limit) => (
                ToolDisposition::Limit,
                "Tool result or resource limit reached".into(),
            ),
            Err(ToolError::Conflict) => (
                ToolDisposition::Conflict,
                "File changed or edit did not match exactly once; read it again".into(),
            ),
            Err(ToolError::Path) => (
                ToolDisposition::Denied,
                "File path is unavailable or outside the admitted regular-file boundary".into(),
            ),
            Err(ToolError::Uncertain) => (
                ToolDisposition::Uncertain,
                "Filesystem change may have occurred; inspect current state before another attempt"
                    .into(),
            ),
            Err(_) => (
                ToolDisposition::Failed,
                "Tool operation failed; no automatic retry".into(),
            ),
        };
        stage = BootstrapStage::Receipt;
        let receipt = GuardReceipt {
            contract_version: 1,
            intent_digest: task.intent.digest(),
            enforcement_digest: task.intent.enforcement_digest,
            limits: task.intent.limits.clone(),
        };
        let mut observation = ToolObservation {
            intent: task.intent,
            disposition,
            output,
            guard: Some(receipt),
            cache_update: if task.cache.is_some() {
                crate::tools::CacheUpdate::NotAttempted
            } else {
                crate::tools::CacheUpdate::NotApplicable
            },
        };
        if !observation.valid() {
            observation.disposition = ToolDisposition::Limit;
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
            if observation.disposition != ToolDisposition::Succeeded {
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
                            &mut stdout,
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
        }
        Ok(())
    })();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        let _ = serde_json::to_writer(std::io::stderr().lock(), &BootstrapFailure { stage });
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "macos")]
pub(super) fn unit_main() -> ExitCode {
    macos::unit_main()
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

fn skill_text(name: &str, resource: Option<&str>, bytes: Vec<u8>) -> Result<String, ToolError> {
    if resource.unwrap_or("SKILL.md") == "SKILL.md" {
        super::skills::metadata(name, &bytes)?;
    }
    String::from_utf8(bytes).map_err(|_| ToolError::Operation)
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
    let mut path = vec!["/usr/bin".to_owned(), "/bin".to_owned()];
    for grant in runtime_grants
        .iter()
        .filter(|grant| grant.kind.is_toolchain())
    {
        #[cfg(target_os = "linux")]
        let runtime = grant.mounted_path()?;
        #[cfg(target_os = "macos")]
        let runtime = grant.path.clone();
        path.insert(0, runtime.join("bin").to_string_lossy().into_owned());
    }
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
        .env("PATH", path.join(":"))
        .env("HOME", &scratch)
        .env("TMPDIR", &scratch)
        .env("CARGO_HOME", cache_base.join("cargo"))
        .env("CARGO_TARGET_DIR", cache_base.join("target"))
        .env(
            "npm_config_cache",
            cache_root(
                runtime_grants,
                super::toolchains::RuntimeKind::NpmCache,
                cache_base,
            )
            .unwrap_or_else(|| cache_base.join(".npm")),
        )
        .env(
            "PIP_CACHE_DIR",
            cache_root(
                runtime_grants,
                super::toolchains::RuntimeKind::PipHttpCache,
                cache_base,
            )
            .or_else(|| {
                cache_root(
                    runtime_grants,
                    super::toolchains::RuntimeKind::PipWheelCache,
                    cache_base,
                )
            })
            .unwrap_or_else(|| cache_base.join(".cache/pip")),
        )
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

fn cache_root(
    runtime_grants: &[RuntimeGrant],
    kind: super::toolchains::RuntimeKind,
    scratch: &Path,
) -> Option<PathBuf> {
    if !runtime_grants.iter().any(|grant| grant.kind == kind) {
        return None;
    }
    Some(match kind {
        super::toolchains::RuntimeKind::NpmCache => scratch.join(".npm"),
        super::toolchains::RuntimeKind::PipHttpCache
        | super::toolchains::RuntimeKind::PipWheelCache => scratch.join(".cache/pip"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn runtime_mount_rejection_reports_its_own_bootstrap_stage() {
        let failure = BootstrapFailure {
            stage: BootstrapStage::RuntimeMounts,
        };
        assert_eq!(failure.failure_stage(), "runtime mount topology");
        assert_eq!(failure.to_string(), "runtime mount topology");
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
        assert!(verify_intent_runtime_grants(&process, &config).is_err());
    }

    #[test]
    fn package_manager_cache_paths_follow_private_scratch() {
        let scratch = Path::new("/native-unit/scratch");
        let npm = RuntimeGrant {
            kind: super::super::toolchains::RuntimeKind::NpmCache,
            manager: super::super::toolchains::RuntimeManager::HostCache,
            version: None,
            path: PathBuf::from("/home/user/.npm/_cacache"),
            device: 1,
            inode: 2,
        };
        let pip = RuntimeGrant {
            kind: super::super::toolchains::RuntimeKind::PipHttpCache,
            manager: super::super::toolchains::RuntimeManager::HostCache,
            version: None,
            path: PathBuf::from("/home/user/.cache/pip/http-v2"),
            device: 1,
            inode: 2,
        };
        assert_eq!(
            cache_root(
                std::slice::from_ref(&npm),
                super::super::toolchains::RuntimeKind::NpmCache,
                scratch,
            ),
            Some(scratch.join(".npm"))
        );
        assert_eq!(
            cache_root(
                std::slice::from_ref(&pip),
                super::super::toolchains::RuntimeKind::PipHttpCache,
                scratch,
            ),
            Some(scratch.join(".cache/pip"))
        );
    }
}
