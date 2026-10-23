use crate::SessionId;
pub use crate::store::AdmissionKind;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

type Admissions = HashMap<SessionId, Vec<(usize, usize, AdmissionKind)>>;

static ADMISSIONS: LazyLock<Mutex<Admissions>> = LazyLock::new(Mutex::default);

pub(crate) fn record(session: SessionId, count: usize, slots: usize, kind: AdmissionKind) {
    if let Ok(mut admissions) = ADMISSIONS.lock() {
        admissions
            .entry(session)
            .or_default()
            .push((count, slots, kind));
    }
}

// (journal position, Event slots, admission kind) per accepted Store admission.
pub fn recorded(session: SessionId) -> Vec<(usize, usize, AdmissionKind)> {
    ADMISSIONS
        .lock()
        .ok()
        .and_then(|admissions| admissions.get(&session).cloned())
        .unwrap_or_default()
}
