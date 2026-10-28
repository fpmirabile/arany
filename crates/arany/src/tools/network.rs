mod proxy;

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub(crate) use proxy::{Proxy, annotate};

pub(crate) const MAX_NETWORK_PATTERNS: usize = 32;
pub(crate) const MAX_NETWORK_CONNECTIONS: u32 = 64;
pub(crate) const MAX_NETWORK_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const NETWORK_IDLE_MS: u64 = 120_000;
pub(crate) const NETWORK_REFUSAL_ENTRIES: usize = 8;
pub(crate) const NETWORK_REFUSAL_RESERVE_BYTES: usize = 4 * 1024;
// Open connections, including ones still sending a header, before accept-time refusal.
pub(crate) const OPEN_CONNECTIONS: usize = 2 * MAX_NETWORK_CONNECTIONS as usize;
// Sent by the bridge when its own relay bound is reached, so the proxy still records the overflow.
#[cfg(target_os = "linux")]
pub(crate) const BRIDGE_OVERFLOW: &str = "ARANY-BRIDGE-OVERFLOW";
#[cfg(target_os = "linux")]
pub(crate) const PROXY_PORT: u16 = 3128;
#[cfg(target_os = "linux")]
pub(crate) const PROXY_ENDPOINT: &str = "http://127.0.0.1:3128";
#[cfg(target_os = "linux")]
pub(crate) const PROXY_SOCKET_TARGET: &str = "/arany-egress.sock";
#[cfg(target_os = "macos")]
pub(crate) fn proxy_endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}
// Follows a capture refusal's plain text so the rows stay machine-readable.
pub(crate) const REFUSAL_MARKER: &str = "\nnetwork_refusals: ";
pub(crate) const MAX_HOST_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;
const MIN_DOMAIN_LABELS: usize = 2;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Network {
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl Network {
    pub(crate) fn valid(&self) -> bool {
        let patterns = |list: &[String]| {
            list.len() <= 2 * MAX_NETWORK_PATTERNS && list.iter().all(|item| valid_pattern(item))
        };
        patterns(&self.allow)
            && patterns(&self.deny)
            && distinct(&self.allow)
            && distinct(&self.deny)
    }

    pub(crate) fn merged(mut self, allow: &[String], deny: &[String]) -> Self {
        for (list, extra) in [(&mut self.allow, allow), (&mut self.deny, deny)] {
            for pattern in extra {
                if !list.contains(pattern) {
                    list.push(pattern.clone());
                }
            }
        }
        self
    }
}

fn distinct(list: &[String]) -> bool {
    list.iter().collect::<std::collections::BTreeSet<_>>().len() == list.len()
}

pub(crate) fn valid_pattern(pattern: &str) -> bool {
    if let Ok(address) = pattern.parse::<IpAddr>() {
        return address.is_loopback() && address.to_string() == pattern;
    }
    let domain = pattern.strip_prefix("*.").unwrap_or(pattern);
    valid_domain(domain)
}

fn valid_domain(domain: &str) -> bool {
    let labels: Vec<_> = domain.split('.').collect();
    domain.len() <= MAX_HOST_BYTES
        && labels.len() >= MIN_DOMAIN_LABELS
        && labels.iter().all(|label| {
            (1..=MAX_LABEL_BYTES).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        && labels
            .last()
            .is_some_and(|label| !label.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests;
