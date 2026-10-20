use crate::SessionId;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

type Admissions = HashMap<SessionId, Vec<(usize, usize, bool)>>;

static ADMISSIONS: LazyLock<Mutex<Admissions>> = LazyLock::new(Mutex::default);

pub(crate) fn record(session: SessionId, count: usize, slots: usize, sealed_data: bool) {
    if let Ok(mut admissions) = ADMISSIONS.lock() {
        admissions
            .entry(session)
            .or_default()
            .push((count, slots, sealed_data));
    }
}

// (journal position, Event slots, sealed-data allowance) per accepted Store admission.
pub fn recorded(session: SessionId) -> Vec<(usize, usize, bool)> {
    ADMISSIONS
        .lock()
        .ok()
        .and_then(|admissions| admissions.get(&session).cloned())
        .unwrap_or_default()
}
