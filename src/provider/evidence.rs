use std::net::SocketAddr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderEvidenceRecord {
    pub name: String,
    pub digest: [u8; 32],
    pub addresses: Vec<SocketAddr>,
    pub checked_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeEvidenceRecord {
    pub fingerprint: [u8; 32],
    pub checked_at_ms: i64,
    pub expires_at_ms: i64,
}

pub(crate) enum EvidenceOp {
    ClearProvider(String),
    RecordProvider(ProviderEvidenceRecord),
    LoadProvider(String),
    ClearNative([u8; 32]),
    RecordNative(NativeEvidenceRecord),
    LoadNative([u8; 32]),
}

pub(crate) enum EvidenceRecord {
    Provider(ProviderEvidenceRecord),
    Native(NativeEvidenceRecord),
}
