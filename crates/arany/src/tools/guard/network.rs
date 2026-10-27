use super::{Task, ToolError};
use crate::tools::types::{NetworkGrant, ToolLimits};

pub(super) fn validate(task: &Task) -> Result<(), ToolError> {
    if task.intent.limits.network == NetworkGrant::Proxy
        && (task.config.network.is_none() || cfg!(not(target_os = "linux")))
    {
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn set_environment(command: &mut tokio::process::Command, limits: &ToolLimits) {
    if limits.network != NetworkGrant::Proxy {
        return;
    }
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, crate::tools::network::PROXY_ENDPOINT);
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
