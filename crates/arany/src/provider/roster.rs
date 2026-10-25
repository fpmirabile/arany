use super::{Provider, ProviderIdentity, ProviderRoutes, Providers};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub(super) const MAX_PROFILE_NAME_BYTES: usize = 64;

pub fn valid_profile_name(name: &str) -> bool {
    (1..=MAX_PROFILE_NAME_BYTES).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedProviderRoutes {
    pub name: String,
    pub routes: Vec<ProviderIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterSnapshot {
    pub preset: String,
    pub selection_digest: [u8; 32],
    pub profiles: Vec<NamedProviderRoutes>,
    pub providers: Providers<String>,
    pub child_profiles: Vec<String>,
}

impl RosterSnapshot {
    pub fn chain(&self, name: &str) -> Option<&[ProviderIdentity]> {
        self.profiles
            .iter()
            .find(|profile| profile.name == name)
            .map(|profile| profile.routes.as_slice())
    }

    pub fn role_routes(&self) -> Option<ProviderRoutes> {
        Some(Providers {
            primary: self.chain(&self.providers.primary)?.to_vec(),
            child: self.chain(&self.providers.child)?.to_vec(),
            review: self.chain(&self.providers.review)?.to_vec(),
            compaction: self.chain(&self.providers.compaction)?.to_vec(),
        })
    }

    pub fn digest(&self) -> Option<[u8; 32]> {
        let profiles: Vec<_> = self
            .profiles
            .iter()
            .map(|profile| {
                let routes: Vec<_> = profile
                    .routes
                    .iter()
                    .map(|identity| {
                        (
                            &identity.profile,
                            &identity.model,
                            identity.effort,
                            identity
                                .saved_api_account_id()
                                .or_else(|| identity.chatgpt_provenance().map(|p| p.account_id)),
                        )
                    })
                    .collect();
                (&profile.name, routes)
            })
            .collect();
        let bytes = serde_json::to_vec(&(
            &self.preset,
            profiles,
            &self.providers,
            &self.child_profiles,
        ))
        .ok()?;
        let mut hash = Sha256::new();
        hash.update(b"arany-roster-selection-v1\0");
        hash.update(bytes);
        Some(hash.finalize().into())
    }

    pub fn valid(&self) -> bool {
        valid_profile_name(&self.preset)
            && (1..=8).contains(&self.profiles.len())
            && (1..=8).contains(&self.child_profiles.len())
            && self
                .profiles
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
            && self.profiles.iter().all(|profile| {
                valid_profile_name(&profile.name)
                    && Providers::all(profile.routes.clone()).valid()
                    && (self.providers.values().contains(&&profile.name)
                        || self.child_profiles.contains(&profile.name))
            })
            && self
                .providers
                .values()
                .iter()
                .all(|name| self.chain(name).is_some())
            && self.child_profiles.iter().enumerate().all(|(index, name)| {
                self.chain(name).is_some() && !self.child_profiles[..index].contains(name)
            })
            && self.child_profiles.contains(&self.providers.child)
            && self.digest() == Some(self.selection_digest)
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= 16 * 1024)
    }
}

#[derive(Clone)]
pub struct AdmittedRoster {
    pub snapshot: RosterSnapshot,
    pub profiles: Vec<(String, Vec<Arc<dyn Provider>>)>,
}

impl AdmittedRoster {
    pub(crate) fn valid(&self) -> bool {
        self.snapshot.valid()
            && self.profiles.len() == self.snapshot.profiles.len()
            && self.profiles.iter().zip(&self.snapshot.profiles).all(
                |((name, providers), profile)| {
                    name == &profile.name
                        && providers
                            .iter()
                            .map(|provider| provider.identity())
                            .eq(profile.routes.iter())
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CustomProfileProvenance, Provenance};

    fn snapshot() -> RosterSnapshot {
        let mut snapshot = RosterSnapshot {
            preset: "coding".into(),
            selection_digest: [0; 32],
            profiles: (0..8)
                .map(|index| NamedProviderRoutes {
                    name: format!("profile-{index}"),
                    routes: (0..3)
                        .map(|route| {
                            ProviderIdentity::new(
                                "custom:exact",
                                format!("model-{route}"),
                                Provenance::Custom(CustomProfileProvenance {
                                    endpoint: "https://example.com/responses".into(),
                                    evidence_digest: [255; 32],
                                    capability_evidence_version: 2,
                                }),
                            )
                        })
                        .collect(),
                })
                .collect(),
            providers: Providers::all("profile-0".into()),
            child_profiles: (0..8).map(|index| format!("profile-{index}")).collect(),
        };
        snapshot.selection_digest = snapshot.digest().unwrap();
        snapshot
    }

    #[test]
    fn roster_snapshot_rejects_tampering_and_exact_byte_overflow() {
        let original = snapshot();
        assert!(original.valid());
        let mut at_limit = original.clone();
        let mut remaining = 16 * 1024 - serde_json::to_vec(&at_limit).unwrap().len();
        for profile in &mut at_limit.profiles {
            for route in &mut profile.routes {
                let Provenance::Custom(provenance) = &mut route.provenance else {
                    unreachable!()
                };
                let extra = remaining.min(512 - provenance.endpoint.len());
                provenance.endpoint.push_str(&"x".repeat(extra));
                remaining -= extra;
            }
        }
        assert_eq!(remaining, 0);
        assert_eq!(serde_json::to_vec(&at_limit).unwrap().len(), 16 * 1024);
        assert!(at_limit.valid());
        let Provenance::Custom(provenance) = &mut at_limit
            .profiles
            .last_mut()
            .unwrap()
            .routes
            .last_mut()
            .unwrap()
            .provenance
        else {
            unreachable!()
        };
        provenance.endpoint.push('x');
        assert!(provenance.endpoint.len() <= 512);
        assert_eq!(serde_json::to_vec(&at_limit).unwrap().len(), 16 * 1024 + 1);
        assert!(!at_limit.valid());
        for change in 0..9 {
            let mut invalid = original.clone();
            match change {
                0 => invalid.selection_digest[0] ^= 1,
                1 => invalid.profiles.swap(0, 1),
                2 => invalid.providers.child = "missing".into(),
                3 => invalid.child_profiles.push("profile-0".into()),
                4 => {
                    invalid.child_profiles.remove(0);
                }
                5 => invalid.profiles[0].routes[0].model = "changed".into(),
                6 => invalid.profiles[0].routes.clear(),
                7 => invalid.profiles.push(invalid.profiles[0].clone()),
                _ => invalid.profiles[0].name = "../outside".into(),
            }
            assert!(!invalid.valid(), "tamper {change}");
        }
        let mut fresh = original.clone();
        let Provenance::Custom(provenance) = &mut fresh.profiles[0].routes[0].provenance else {
            unreachable!()
        };
        provenance.evidence_digest = [1; 32];
        assert_eq!(fresh.digest(), original.digest());
        assert!(fresh.valid());
        fresh.profiles[0].routes[0].effort = Some(super::super::Effort::Low);
        assert_ne!(fresh.digest(), original.digest());
    }
}
