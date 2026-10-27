use super::*;
use crate::tools::fs::{identity, open_directory};
use crate::tools::guard::{BuildCacheSession, CacheCandidate, cache_directory};
use cap_std::fs::Dir;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

pub(in crate::tools::guard::linux) const HANDOFF_BYTES: usize = 256;
const ACK: &[u8] = b"CACHE_PINNED\n";

#[cfg(test)]
mod tests;

pub(in crate::tools::guard::linux) async fn prepare(
    runtime: &ToolRuntime,
    seed: &Path,
    intent: &EffectIntent,
    deadline: tokio::time::Instant,
) -> Result<
    (
        Option<BuildCacheSession>,
        (supervisor::Launch, std::fs::File),
    ),
    ToolError,
> {
    let cache = cache_directory::session(runtime, intent, deadline).await?;
    let mut launch = supervisor::Launch::prepare(&runtime.guard_executable, seed, intent).await?;
    if let Some(cache) = &cache {
        launch.0.cache = Grant::new(cache, seed)?;
    }
    Ok((cache, launch))
}

pub(in crate::tools::guard::linux) async fn complete(
    cache: &mut Option<BuildCacheSession>,
    candidate: &mut CacheCandidate,
    input: &mut (impl tokio::io::AsyncBufRead + Unpin),
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), ToolError> {
    if let Some(cache) = cache
        && let Some(directory) = &mut cache.directory
    {
        let (upper, captured) = receive(input, output).await?;
        directory.upper = upper;
        if *candidate == CacheCandidate::Received {
            *candidate = captured;
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools::guard::linux) struct Grant {
    source: PathBuf,
    root: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
    exclusions: Vec<String>,
}

impl Grant {
    pub(in crate::tools::guard::linux) fn new(
        cache: &BuildCacheSession,
        seed: &Path,
    ) -> Result<Option<Self>, ToolError> {
        let Some(session) = &cache.directory else {
            return Ok(None);
        };
        let source = unavailable(std::fs::read_link(format!(
            "/proc/self/fd/{}",
            session.lower.as_raw_fd()
        )))?;
        let root = seed.join("cache-namespace");
        unavailable(std::fs::create_dir(&root))?;
        let (device, inode) = identity(&session.lower)?;
        Ok(Some(Self {
            source,
            root,
            device,
            inode,
            bytes: cache.transfer.view_bytes,
            exclusions: cache.transfer.exclusions.clone(),
        }))
    }

    pub(in crate::tools::guard::linux) fn merged(&self) -> PathBuf {
        self.root.join("view/merged")
    }

    pub(in crate::tools::guard::linux) fn mount(self) -> Result<View, ToolError> {
        supervisor::mount_tmpfs(&self.root, self.bytes)?;
        let root = self.root.join("view");
        unavailable(std::fs::create_dir(&root))?;
        clone_lower(
            &root,
            &self.source,
            &scope::Root {
                relative: String::new(),
                device: self.device,
                inode: self.inode,
                directory: true,
            },
        )?;
        overlay_probe(&root)?;
        mount_overlay(&root, "")?;
        for path in &self.exclusions {
            if !crate::store::CACHE_ROOTS.contains(&path.as_str()) {
                return Err(ToolError::Configuration);
            }
            unavailable(std::fs::create_dir_all(self.merged().join(path)))?;
        }
        let view = View { grant: self };
        view.verify()?;
        Ok(view)
    }
}

pub(in crate::tools::guard::linux) struct View {
    grant: Grant,
}

impl View {
    pub(in crate::tools::guard::linux) fn bytes(&self) -> u64 {
        self.grant.bytes
    }

    pub(in crate::tools::guard::linux) fn verify(&self) -> Result<(), ToolError> {
        supervisor::verify_tmpfs(&self.grant.root, self.grant.bytes)?;
        let root = self.grant.root.join("view");
        let mounts = read_mountinfo()?;
        let lower = open_directory(&root.join("lower"))?;
        if identity(&lower)? != (self.grant.device, self.grant.inode)
            || mounts
                .iter()
                .filter(|entry| entry.path == root.join("lower") && option(&entry.options, b"ro"))
                .count()
                != 1
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        verify_overlay(&mounts, &self.grant.merged(), &[], Path::new("/"))?;
        verify_layers(&mounts, &root)?;
        if mounts.iter().any(|entry| {
            entry.path.starts_with(&self.grant.root)
                && entry.path != self.grant.root
                && entry.path != root.join("lower")
                && entry.path != self.grant.merged()
        }) {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(())
    }

    pub(in crate::tools::guard::linux) fn handoff(
        &self,
        input: &mut impl Read,
        output: &mut impl Write,
    ) -> Result<(), ToolError> {
        self.verify()?;
        let upper = open_directory(&self.grant.root.join("view/upper"))?;
        let (device, inode) = identity(&upper)?;
        let validation = crate::tools::fs::command::overlay::cache::capture(
            &open_directory(&self.grant.root.join("view/lower"))?,
            &upper,
            &self.grant.exclusions,
            std::time::Instant::now() + crate::tools::fs::command::overlay::cache::CAPTURE_TIME,
            self.grant.bytes,
        );
        let report = validation
            .map(|_| Handoff {
                pid: std::process::id(),
                fd: upper.as_raw_fd(),
                device,
                inode,
            })
            .map_err(|error| match error {
                ToolError::Limit | ToolError::Timeout(_) => CaptureFailure::Limit,
                _ => CaptureFailure::Rejected,
            });
        serde_json::to_writer(&mut *output, &report).map_err(|_| ToolError::Operation)?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|_| ToolError::Operation)?;
        let mut ack = [0; ACK.len()];
        input
            .read_exact(&mut ack)
            .map_err(|_| ToolError::Operation)?;
        if ack != ACK {
            return Err(ToolError::Operation);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    pid: u32,
    fd: i32,
    device: u64,
    inode: u64,
}

pub(in crate::tools::guard) async fn receive(
    input: &mut (impl tokio::io::AsyncBufRead + Unpin),
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(Option<Dir>, CacheCandidate), ToolError> {
    let report: Result<Handoff, CaptureFailure> =
        serde_json::from_slice(&read_line_capped(input, HANDOFF_BYTES).await?)
            .map_err(|_| ToolError::Operation)?;
    let handoff = match report {
        Ok(handle) => handle,
        Err(failure) => {
            output
                .write_all(ACK)
                .await
                .map_err(|_| ToolError::Operation)?;
            return Ok((
                None,
                if matches!(failure, CaptureFailure::Limit | CaptureFailure::Timeout) {
                    CacheCandidate::Rejected
                } else {
                    CacheCandidate::Failed
                },
            ));
        }
    };
    if handoff.pid == 0 || handoff.fd < 0 {
        return Err(ToolError::Operation);
    }
    let process = rustix::fs::open(
        format!("/proc/{}", handoff.pid),
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| ToolError::Operation)?;
    let file = std::fs::File::from(
        rustix::fs::openat(
            &process,
            format!("fd/{}", handoff.fd),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| ToolError::Operation)?,
    );
    let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
    if !metadata.is_dir()
        || (metadata.dev(), metadata.ino()) != (handoff.device, handoff.inode)
        || rustix::fs::fstatfs(&file)
            .map_err(|_| ToolError::Operation)?
            .f_type
            != nix::libc::TMPFS_MAGIC
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    output
        .write_all(ACK)
        .await
        .map_err(|_| ToolError::Operation)?;
    Ok((Some(Dir::from_std_file(file)), CacheCandidate::Received))
}

pub(in crate::tools::guard) fn verify_helper(
    bytes: Option<u64>,
    task: &Task,
) -> Result<(), ToolError> {
    let expected = task
        .cache
        .as_ref()
        .filter(|_| cache_directory::selected(&task.intent))
        .map(|cache| cache.view_bytes);
    if bytes != expected {
        return Err(ToolError::ProtectionUnavailable);
    }
    if bytes.is_some() {
        let mounts = read_mountinfo()?;
        let entry = mounts
            .iter()
            .find(|entry| entry.path == Path::new("/cache") && entry.filesystem == b"overlay")
            .ok_or(ToolError::ProtectionUnavailable)?;
        if !option(&entry.super_options, b"userxattr")
            || !option(&entry.options, b"rw")
            || entry.optional_fields != 0
            || entry.source != b"overlay"
            || entry.root != Path::new("/")
            || mounts
                .iter()
                .filter(|entry| entry.path == Path::new("/cache"))
                .count()
                != 1
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let targets = task
            .config
            .runtime_grants
            .iter()
            .filter(|grant| grant.kind.is_cache())
            .map(|grant| grant.mounted_path(true))
            .collect::<Result<Vec<_>, _>>()?;
        if mounts.iter().any(|entry| {
            entry.path != Path::new("/cache")
                && entry.path.starts_with("/cache")
                && (!targets.contains(&entry.path) || !option(&entry.options, b"ro"))
        }) {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}
