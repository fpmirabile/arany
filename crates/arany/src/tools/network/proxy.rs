#[cfg(target_os = "linux")]
use super::BRIDGE_OVERFLOW;
use super::{
    MAX_HOST_BYTES, MAX_NETWORK_BYTES, MAX_NETWORK_CONNECTIONS, NETWORK_IDLE_MS,
    NETWORK_REFUSAL_ENTRIES, Network, OPEN_CONNECTIONS, REFUSAL_MARKER, valid_domain,
};
use crate::tools::ToolError;
use crate::tools::types::{
    CommandResult, MAX_TOOL_RESULT_BYTES, NetworkRefusal, NetworkRefusalReason, ToolObservation,
};
use std::net::IpAddr;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(target_os = "macos")]
use tokio::net::{TcpListener as Listener, TcpStream as Client};
#[cfg(target_os = "linux")]
use tokio::net::{UnixListener as Listener, UnixStream as Client};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, timeout};

const HTTPS_PORT: u16 = 443;
const HEAD_BYTES: usize = 8 * 1024;
const HEAD_DEADLINE: Duration = Duration::from_secs(10);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
const CHUNK_BYTES: usize = 16 * 1024;
const STOP_DEADLINE: Duration = Duration::from_secs(2);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(10);

#[cfg(target_os = "linux")]
type Endpoint = PathBuf;
#[cfg(target_os = "macos")]
type Endpoint = u16;

pub(crate) struct Proxy {
    task: JoinHandle<()>,
    stop: Option<oneshot::Sender<()>>,
    refusals: Arc<Refusals>,
    endpoint: Endpoint,
}

struct Context {
    grant: Network,
    idle: Duration,
    refusals: Arc<Refusals>,
    connections: AtomicU32,
    uploaded: AtomicU64,
    downloaded: AtomicU64,
}

struct Activity {
    started: Instant,
    last_ms: AtomicU64,
    idle: Duration,
}

impl Proxy {
    #[cfg(target_os = "linux")]
    pub(crate) fn start(grant: Network, socket: PathBuf) -> Result<Self, ToolError> {
        Self::start_idle(grant, socket, Duration::from_millis(NETWORK_IDLE_MS))
    }

    #[cfg(target_os = "linux")]
    fn start_idle(grant: Network, socket: PathBuf, idle: Duration) -> Result<Self, ToolError> {
        use std::os::unix::fs::PermissionsExt;
        let listener = Listener::bind(&socket).map_err(|_| ToolError::ProtectionUnavailable)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        Ok(Self::serving(listener, grant, idle, socket))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn start(grant: Network) -> Result<Self, ToolError> {
        Self::start_idle(grant, Duration::from_millis(NETWORK_IDLE_MS))
    }

    // The unit profile admits one numeric loopback port, so the listener takes an ephemeral port on 127.0.0.1 only.
    #[cfg(target_os = "macos")]
    fn start_idle(grant: Network, idle: Duration) -> Result<Self, ToolError> {
        let unavailable = |_| ToolError::ProtectionUnavailable;
        let listener =
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(unavailable)?;
        listener.set_nonblocking(true).map_err(unavailable)?;
        let port = listener.local_addr().map_err(unavailable)?.port();
        let listener = Listener::from_std(listener).map_err(unavailable)?;
        Ok(Self::serving(listener, grant, idle, port))
    }

    fn serving(listener: Listener, grant: Network, idle: Duration, endpoint: Endpoint) -> Self {
        let refusals = Arc::new(Refusals::default());
        let (stop, stopped) = oneshot::channel();
        let context = Arc::new(Context {
            grant,
            idle,
            refusals: Arc::clone(&refusals),
            connections: AtomicU32::new(0),
            uploaded: AtomicU64::new(0),
            downloaded: AtomicU64::new(0),
        });
        Self {
            task: tokio::spawn(serve(listener, context, stopped)),
            stop: Some(stop),
            refusals,
            endpoint,
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn socket(&self) -> &std::path::Path {
        &self.endpoint
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn port(&self) -> u16 {
        self.endpoint
    }

    // A cleanup failure propagates only once the listener and every tunnel are closed.
    #[cfg(target_os = "macos")]
    pub(crate) async fn settle(
        proxy: Option<Self>,
        cleanup: Result<(), ToolError>,
    ) -> Result<Vec<NetworkRefusal>, ToolError> {
        let refusals = match proxy {
            Some(proxy) => proxy.finish().await,
            None => Vec::new(),
        };
        cleanup.map(|()| refusals)
    }

    // Returns only after the server and every connection task are gone, so no outbound work outlives the effect.
    pub(crate) async fn finish(mut self) -> Vec<NetworkRefusal> {
        let died = self.task.is_finished();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if timeout(STOP_DEADLINE, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        if died {
            self.refusals
                .record("", NetworkRefusalReason::ProxyUnavailable);
        }
        self.refusals.take()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(target_os = "linux")]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

async fn serve(listener: Listener, context: Arc<Context>, mut stopped: oneshot::Receiver<()>) {
    let mut connections = JoinSet::new();
    let mut refusing = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => admit(stream, &context, &mut connections, &mut refusing),
                    Err(_) => sleep(ACCEPT_BACKOFF).await,
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            Some(_) = refusing.join_next(), if !refusing.is_empty() => {}
        }
    }
    connections.abort_all();
    refusing.abort_all();
    while connections.join_next().await.is_some() {}
    while refusing.join_next().await.is_some() {}
}

fn admit(
    mut stream: Client,
    context: &Arc<Context>,
    connections: &mut JoinSet<()>,
    refusing: &mut JoinSet<()>,
) {
    // Completed tasks keep their slots until reaped, bounding both live work and retained results.
    if connections.len() < OPEN_CONNECTIONS {
        connections.spawn(connection(stream, Arc::clone(context)));
    } else {
        context.refusals.record("", NetworkRefusalReason::Limit);
        if refusing.len() < MAX_NETWORK_CONNECTIONS as usize {
            refusing.spawn(async move {
                refuse(&mut stream, 429, NetworkRefusalReason::Limit).await;
            });
        }
    }
}

struct Request {
    host: String,
    port: u16,
    form: Form,
    early: Vec<u8>,
}

enum Form {
    Tunnel,
    Plain(Vec<u8>),
}

async fn connection(mut client: Client, context: Arc<Context>) {
    let request = match read_request(&mut client).await {
        Ok(request) => request,
        Err((host, reason)) => {
            context.refusals.record(&host, reason);
            return refuse(&mut client, 405, reason).await;
        }
    };
    let Request {
        host,
        port,
        form,
        early,
    } = request;
    if !context.grant.lists(&host) {
        context
            .refusals
            .record(&host, NetworkRefusalReason::Unlisted);
        return refuse(&mut client, 403, NetworkRefusalReason::Unlisted).await;
    }
    let admitted = match form {
        Form::Tunnel => context.grant.admits(&host, port),
        Form::Plain(_) => context.grant.admits_plain(&host, port),
    };
    if !admitted {
        context
            .refusals
            .record(&host, NetworkRefusalReason::Unsupported);
        return refuse(&mut client, 403, NetworkRefusalReason::Unsupported).await;
    }
    if context.connections.fetch_add(1, Ordering::AcqRel) >= MAX_NETWORK_CONNECTIONS {
        context.refusals.record(&host, NetworkRefusalReason::Limit);
        return refuse(&mut client, 429, NetworkRefusalReason::Limit).await;
    }
    let Some(upstream) = dial(&host, port).await else {
        context
            .refusals
            .record(&host, NetworkRefusalReason::Destination);
        return refuse(&mut client, 502, NetworkRefusalReason::Destination).await;
    };
    let forwarded = match form {
        Form::Tunnel => {
            if client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            early
        }
        Form::Plain(mut head) => {
            head.extend_from_slice(&early);
            head
        }
    };
    tunnel(client, upstream, &forwarded, &host, &context).await;
}

async fn dial(host: &str, port: u16) -> Option<TcpStream> {
    let addresses = crate::provider::resolve_admitted(host, port).await.ok()?;
    timeout(CONNECT_DEADLINE, TcpStream::connect(&addresses[..]))
        .await
        .ok()?
        .ok()
}

async fn read_request(client: &mut Client) -> Result<Request, (String, NetworkRefusalReason)> {
    let unsupported = |host: &str| (host.to_owned(), NetworkRefusalReason::Unsupported);
    let mut head = Vec::with_capacity(512);
    let end = timeout(HEAD_DEADLINE, async {
        let mut chunk = [0u8; 1024];
        loop {
            let read = client
                .read(&mut chunk)
                .await
                .ok()
                .filter(|read| *read > 0)?;
            head.extend_from_slice(&chunk[..read]);
            if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
                return (end + 4 <= HEAD_BYTES).then_some(end + 4);
            }
            if head.len() > HEAD_BYTES {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(|| unsupported(""))?;
    let line_end = head.iter().position(|byte| *byte == b'\r').unwrap_or(0);
    let line = std::str::from_utf8(&head[..line_end]).map_err(|_| unsupported(""))?;
    let mut parts = line.split(' ');
    #[cfg(target_os = "linux")]
    if line == BRIDGE_OVERFLOW {
        return Err((String::new(), NetworkRefusalReason::Limit));
    }
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    let target = target.unwrap_or_default();
    if !version.is_some_and(|version| version.starts_with("HTTP/1.")) || parts.next().is_some() {
        return Err(unsupported(""));
    }
    let (host, port, form) = match method {
        Some("CONNECT") => {
            let (host, port) = split_authority(target).ok_or_else(|| unsupported(""))?;
            (host, port, Form::Tunnel)
        }
        Some(method @ ("GET" | "POST")) => {
            let rest = target
                .strip_prefix("http://")
                .ok_or_else(|| unsupported(""))?;
            let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let (host, port) = split_authority(authority)
                .or_else(|| {
                    (!authority.contains(':')).then_some((authority.to_ascii_lowercase(), 80))
                })
                .filter(|(host, _)| !host.is_empty())
                .ok_or_else(|| unsupported(""))?;
            let path = if path.is_empty() { "/" } else { path };
            let rewritten = origin_form(&head[..end], method, path, version.unwrap_or_default());
            (host, port, Form::Plain(rewritten))
        }
        _ => {
            let host = split_authority(target)
                .map(|(host, _)| host)
                .unwrap_or_default();
            return Err(unsupported(&host));
        }
    };
    Ok(Request {
        host,
        port,
        form,
        early: head.split_off(end),
    })
}

// The upstream sees an origin-form request that closes after one response, so the tunnel never carries a second target.
fn origin_form(head: &[u8], method: &str, path: &str, version: &str) -> Vec<u8> {
    let mut out = format!("{method} {path} {version}\r\n").into_bytes();
    for line in head.split(|byte| *byte == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let name = line.split(|byte| *byte == b':').next().unwrap_or_default();
        if line.is_empty()
            || name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"proxy-connection")
        {
            continue;
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

fn split_authority(authority: &str) -> Option<(String, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.to_ascii_lowercase();
    (!host.is_empty()).then_some(())?;
    Some((host, port.parse().ok().filter(|port| *port != 0)?))
}

async fn refuse(client: &mut Client, status: u16, reason: NetworkRefusalReason) {
    let text = match status {
        403 => "Forbidden",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Bad Gateway",
    };
    let reason = format!("{reason:?}").to_ascii_lowercase();
    let response = format!(
        "HTTP/1.1 {status} {text}\r\nConnection: close\r\nContent-Length: 0\r\nX-Arany-Refusal: {reason}\r\n\r\n"
    );
    let _ = client.write_all(response.as_bytes()).await;
}

async fn tunnel(client: Client, upstream: TcpStream, early: &[u8], host: &str, context: &Context) {
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();
    let activity = Activity {
        started: Instant::now(),
        last_ms: AtomicU64::new(0),
        idle: context.idle,
    };
    let upload = pump(
        client_read,
        upstream_write,
        early,
        (&context.uploaded, &activity),
    );
    let download = pump(
        upstream_read,
        client_write,
        &[],
        (&context.downloaded, &activity),
    );
    if let Err(reason) = tokio::try_join!(upload, download) {
        context.refusals.record(host, reason);
    }
}

async fn pump(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    early: &[u8],
    (counter, activity): (&AtomicU64, &Activity),
) -> Result<(), NetworkRefusalReason> {
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut pending = early.to_vec();
    loop {
        if pending.is_empty() {
            let Some(read) = activity.bounded(from.read(&mut buffer)).await else {
                return Err(NetworkRefusalReason::Limit);
            };
            match read {
                Ok(0) | Err(_) => {
                    let _ = activity.bounded(to.shutdown()).await;
                    return Ok(());
                }
                Ok(read) => pending = buffer[..read].to_vec(),
            }
            activity.touch();
        }
        let length = pending.len() as u64;
        if counter
            .fetch_add(length, Ordering::AcqRel)
            .saturating_add(length)
            > MAX_NETWORK_BYTES
        {
            return Err(NetworkRefusalReason::Limit);
        }
        let mut written = 0;
        while written < pending.len() {
            match activity.bounded(to.write(&pending[written..])).await {
                None => return Err(NetworkRefusalReason::Limit),
                Some(Ok(0) | Err(_)) => return Ok(()),
                Some(Ok(count)) => {
                    written += count;
                    activity.touch();
                }
            }
        }
        pending.clear();
    }
}

impl Activity {
    fn touch(&self) {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(elapsed, Ordering::Release);
    }

    // One deadline for both directions: progress on either keeps the tunnel alive.
    async fn bounded<T>(&self, operation: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::pin!(operation);
        loop {
            let left = self.idle.saturating_sub(self.idle());
            if left.is_zero() {
                return None;
            }
            tokio::select! {
                output = &mut operation => return Some(output),
                () = tokio::time::sleep(left) => {}
            }
        }
    }

    fn idle(&self) -> Duration {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(elapsed.saturating_sub(self.last_ms.load(Ordering::Acquire)))
    }
}

impl Network {
    pub(crate) fn lists(&self, host: &str) -> bool {
        canonical_host(host).is_some_and(|host| {
            self.allow.iter().any(|pattern| matches(pattern, &host))
                && !self.deny.iter().any(|pattern| matches(pattern, &host))
        })
    }

    pub(crate) fn admits(&self, host: &str, port: u16) -> bool {
        let loopback = canonical_host(host)
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        self.lists(host) && (port == HTTPS_PORT || loopback && port != 0)
    }

    // Domains are HTTPS-only under the Provider destination policy; plain HTTP is for numeric loopback.
    pub(crate) fn admits_plain(&self, host: &str, port: u16) -> bool {
        canonical_host(host)
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback())
            && port != 0
            && self.lists(host)
    }
}

fn canonical_host(host: &str) -> Option<String> {
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(address) = host.parse::<IpAddr>() {
        return Some(address.to_string());
    }
    let lower = host.to_ascii_lowercase();
    valid_domain(&lower).then_some(lower)
}

fn matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
        None => pattern == host,
    }
}

pub(crate) fn annotate(
    observation: &mut ToolObservation,
    refusals: Vec<NetworkRefusal>,
) -> Result<(), ToolError> {
    if refusals.is_empty() {
        return Ok(());
    }
    observation.output = annotated(&observation.output, refusals)?;
    Ok(())
}

// A result that cannot hold the refusal rows is a defect; plain-text results keep the rows after the text.
fn annotated(output: &str, refusals: Vec<NetworkRefusal>) -> Result<String, ToolError> {
    let Ok(mut result) = serde_json::from_str::<CommandResult>(output) else {
        let rows = serde_json::to_string(&refusals).map_err(|_| ToolError::Operation)?;
        let text = format!("{output}{REFUSAL_MARKER}{rows}");
        return (text.len() <= MAX_TOOL_RESULT_BYTES)
            .then_some(text)
            .ok_or(ToolError::Limit);
    };
    result.network_refusals = refusals;
    if !result.valid() {
        return Err(ToolError::Limit);
    }
    serde_json::to_string(&result).map_err(|_| ToolError::Operation)
}

#[derive(Default)]
pub(crate) struct Refusals(Mutex<Vec<NetworkRefusal>>);

impl Refusals {
    pub(crate) fn record(&self, host: &str, reason: NetworkRefusalReason) {
        let host = printable_host(host);
        let mut entries = self.0.lock().expect("refusal list");
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.host == host && entry.reason == reason)
        {
            entry.count = entry.count.saturating_add(1);
        } else if entries.len() < NETWORK_REFUSAL_ENTRIES {
            entries.push(NetworkRefusal {
                host,
                reason,
                count: 1,
            });
        }
    }

    pub(crate) fn take(&self) -> Vec<NetworkRefusal> {
        std::mem::take(&mut self.0.lock().expect("refusal list"))
    }
}

// Payload-chosen text reaches the model and the terminal, so only a plain hostname spelling survives.
fn printable_host(host: &str) -> String {
    host.chars()
        .take(MAX_HOST_BYTES)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | ':' | '[' | ']')
            {
                character.to_ascii_lowercase()
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
#[cfg(all(test, target_os = "macos"))]
mod tests_macos;
