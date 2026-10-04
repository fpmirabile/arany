use super::{CustomProfile, CustomProfileError};
use reqwest::{Client, Url, redirect::Policy};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

const MAX_ADDRESSES: usize = 8;
const DNS_DEADLINE: Duration = Duration::from_secs(5);
const CALL_DEADLINE: Duration = Duration::from_secs(120);

pub(super) struct PinnedDestination {
    host: String,
    endpoint: String,
    addresses: Vec<SocketAddr>,
}

impl PinnedDestination {
    pub(super) async fn resolve(profile: &CustomProfile) -> Result<Self, CustomProfileError> {
        let url =
            Url::parse(profile.endpoint()).map_err(|_| CustomProfileError::InvalidConfiguration)?;
        let host = url
            .host_str()
            .ok_or(CustomProfileError::InvalidConfiguration)?;
        let port = url
            .port_or_known_default()
            .ok_or(CustomProfileError::InvalidConfiguration)?;
        let numeric = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
        let addresses = if let Some(address) = numeric {
            vec![SocketAddr::new(address, port)]
        } else {
            let resolved =
                tokio::time::timeout(DNS_DEADLINE, tokio::net::lookup_host((host, port)))
                    .await
                    .map_err(|_| CustomProfileError::Unavailable)?
                    .map_err(|_| CustomProfileError::Unavailable)?;
            resolved.take(MAX_ADDRESSES + 1).collect()
        };
        Self::admit(profile.endpoint(), host, numeric.is_some(), addresses)
    }

    fn admit(
        endpoint: &str,
        host: &str,
        numeric: bool,
        mut addresses: Vec<SocketAddr>,
    ) -> Result<Self, CustomProfileError> {
        if addresses.is_empty() || addresses.len() > MAX_ADDRESSES {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        if addresses.iter().any(|address| {
            if numeric {
                !address.ip().is_loopback()
            } else {
                !permitted_public_address(address.ip())
            }
        }) {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        addresses.sort_unstable();
        addresses.dedup();
        Ok(Self {
            host: host.to_owned(),
            endpoint: endpoint.to_owned(),
            addresses,
        })
    }

    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(super) fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    pub(super) fn client(&self) -> Result<Client, CustomProfileError> {
        Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .redirect(Policy::none())
            .referer(false)
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(CALL_DEADLINE)
            .pool_max_idle_per_host(0)
            .resolve_to_addrs(&self.host, &self.addresses)
            .build()
            .map_err(|_| CustomProfileError::Unavailable)
    }
}

fn permitted_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => public_v6(address),
    }
}

fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    if a == 0 || a == 10 || a == 127 || a >= 224 || address.is_private() || address.is_link_local()
    {
        return false;
    }
    !((a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let first = segments[0];
    let second = segments[1];
    match first {
        0x2001 => {
            ((0x0200..=0x0fff).contains(&second)
                || (0x1200..=0x4dff).contains(&second)
                || (0x5000..=0x5fff).contains(&second)
                || (0x8000..=0xbfff).contains(&second))
                && second != 0x0db8
        }
        0x2003 => second <= 0x3fff,
        0x2400..=0x241f
        | 0x2600..=0x260f
        | 0x2630..=0x263f
        | 0x2800..=0x280f
        | 0x2a00..=0x2a1f
        | 0x2c00..=0x2c0f => true,
        0x2610 | 0x2620 => second <= 0x01ff,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn destination_admission_rejects_special_and_mixed_answers() {
        for denied in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.9",
            "192.0.2.1",
            "192.88.99.2",
            "192.168.0.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002::1",
            "2004::1",
            "2d00::1",
            "3ffe::1",
            "3fff::1",
        ] {
            let address = SocketAddr::new(IpAddr::from_str(denied).unwrap(), 443);
            assert!(
                PinnedDestination::admit(
                    "https://example.com/v1/responses",
                    "example.com",
                    false,
                    vec![address]
                )
                .is_err(),
                "{denied}"
            );
        }
        for admitted in [
            "8.8.8.8",
            "1.1.1.1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2400:3200::1",
            "2a00:1450::1",
        ] {
            let address = SocketAddr::new(IpAddr::from_str(admitted).unwrap(), 443);
            assert!(
                PinnedDestination::admit(
                    "https://example.com/v1/responses",
                    "example.com",
                    false,
                    vec![address]
                )
                .is_ok(),
                "{admitted}"
            );
        }
        let public = SocketAddr::from_str("1.1.1.1:443").unwrap();
        let private = SocketAddr::from_str("10.0.0.1:443").unwrap();
        assert!(
            PinnedDestination::admit(
                "https://example.com/v1/responses",
                "example.com",
                false,
                vec![public, private]
            )
            .is_err()
        );
        assert!(
            PinnedDestination::admit(
                "https://example.com/v1/responses",
                "example.com",
                false,
                vec![public; MAX_ADDRESSES + 1]
            )
            .is_err()
        );
        let pinned = PinnedDestination::admit(
            "https://example.com/v1/responses",
            "example.com",
            false,
            vec![public, public],
        )
        .unwrap();
        assert_eq!(pinned.addresses(), &[public]);
    }

    #[tokio::test]
    async fn numeric_loopback_uses_only_its_exact_address() {
        let profile = CustomProfile {
            name: "local".into(),
            endpoint: "http://127.0.0.1:9321/v1/responses".into(),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            capability_evidence_version: 1,
            efforts: Vec::new(),
        };
        let pinned = profile.resolve_destination().await.unwrap();
        assert_eq!(pinned.endpoint(), profile.endpoint());
        assert_eq!(
            pinned.addresses(),
            &[SocketAddr::from_str("127.0.0.1:9321").unwrap()]
        );
        pinned.client().expect("pinned client");
    }
}
