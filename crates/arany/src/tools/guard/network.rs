use super::{GuardContext, Task, ToolError};
use crate::tools::types::NetworkGrant;

pub(super) fn validate(task: &Task) -> Result<(), ToolError> {
    let proxied = task.intent.limits.network == NetworkGrant::Proxy;
    if proxied && task.config.network.is_none() {
        return Err(ToolError::Configuration);
    }
    #[cfg(target_os = "macos")]
    if proxied != task.proxy_port.is_some() {
        return Err(ToolError::Configuration);
    }
    Ok(())
}

pub(super) fn bind(profile: [u8; 32]) -> Result<[u8; 32], ToolError> {
    #[cfg(target_os = "linux")]
    {
        use super::profile;
        use crate::tools::network::{
            MAX_NETWORK_BYTES, MAX_NETWORK_CONNECTIONS, MAX_NETWORK_PATTERNS, NETWORK_IDLE_MS,
            PROXY_ENDPOINT,
        };
        use crate::validate::digest;
        let mut bytes = b"arany-egress-proxy-v1:parent-unix-socket-loopback-bridge-in-private-netns;inet-socket-families-only;connect-tunnel-and-loopback-plain-allowlisted-hosts;no-tls-termination;no-redirects;public-address-admission;refusals-recorded".to_vec();
        bytes.extend_from_slice(PROXY_ENDPOINT.as_bytes());
        for value in [
            u64::from(MAX_NETWORK_CONNECTIONS),
            MAX_NETWORK_BYTES,
            NETWORK_IDLE_MS,
            MAX_NETWORK_PATTERNS as u64,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&profile::profile_digest(profile::Routing::Proxy)?);
        bytes.extend_from_slice(&profile);
        Ok(digest(&bytes))
    }
    #[cfg(target_os = "macos")]
    {
        use crate::tools::network::{
            MAX_NETWORK_BYTES, MAX_NETWORK_CONNECTIONS, MAX_NETWORK_PATTERNS, NETWORK_IDLE_MS,
        };
        use crate::validate::digest;
        let mut bytes = b"arany-egress-proxy-macos-v1:parent-loopback-listener-ephemeral-port-in-unit-profile;one-numeric-loopback-port-destination-filter;inet-socket-domain-only;bind-listen-delegate-denied;routes-attested-by-refused-connects;connect-tunnel-and-loopback-plain-allowlisted-hosts;no-tls-termination;no-redirects;public-address-admission;refusals-recorded".to_vec();
        for value in [
            u64::from(MAX_NETWORK_CONNECTIONS),
            MAX_NETWORK_BYTES,
            NETWORK_IDLE_MS,
            MAX_NETWORK_PATTERNS as u64,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(super::macos::proxy_contract().as_bytes());
        bytes.extend_from_slice(&profile);
        Ok(digest(&bytes))
    }
}

pub(in crate::tools) fn set_environment(
    command: &mut tokio::process::Command,
    site: &GuardContext<'_>,
) {
    if site.intent.limits.network != NetworkGrant::Proxy {
        return;
    }
    #[cfg(target_os = "linux")]
    let endpoint = crate::tools::network::PROXY_ENDPOINT.to_owned();
    #[cfg(target_os = "macos")]
    let Some(endpoint) = site.proxy_port.map(crate::tools::network::proxy_endpoint) else {
        return;
    };
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, &endpoint);
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
}

#[cfg(target_os = "linux")]
pub(super) fn internal_mode() -> Option<std::process::ExitCode> {
    match std::env::args_os().nth(2)?.to_str()? {
        "--supervisor" => Some(super::linux::supervisor::main()),
        "--network-bridge" => Some(super::linux::bridge::main()),
        _ => None,
    }
}

// The bridge starts before the filter lands: afterwards neither helper nor payload may create its sockets.
#[cfg(target_os = "linux")]
pub(super) fn harden(task: &Task) -> Result<(), ToolError> {
    use super::profile;
    let routing = if task.intent.limits.network == NetworkGrant::Proxy {
        profile::Routing::Proxy
    } else {
        profile::Routing::Offline
    };
    if routing == profile::Routing::Proxy {
        super::linux::bridge::spawn()?;
    }
    super::linux::harden(routing)
}
