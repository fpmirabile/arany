use std::net::SocketAddr;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProviderEvidenceRecord {
    pub name: String,
    pub digest: [u8; 32],
    pub addresses: Vec<SocketAddr>,
    pub checked_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NativeEvidenceRecord {
    pub fingerprint: [u8; 32],
    pub context_window_tokens: Option<u32>,
    pub checked_at_ms: i64,
    pub expires_at_ms: i64,
}

pub(crate) enum EvidenceOp {
    LoadProvider(String),
    ClearProvider(String),
    #[cfg(test)]
    ClearNative([u8; 32]),
    LoadNative([u8; 32]),
}

pub(crate) enum EvidenceRecord {
    Provider(ProviderEvidenceRecord),
    Native(NativeEvidenceRecord),
}
