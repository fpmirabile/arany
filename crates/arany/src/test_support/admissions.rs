use crate::SessionId;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

type Admissions = HashMap<SessionId, Vec<(usize, usize)>>;

static ADMISSIONS: LazyLock<Mutex<Admissions>> = LazyLock::new(Mutex::default);

pub(crate) fn record(session: SessionId, count: usize, slots: usize) {
    if let Ok(mut admissions) = ADMISSIONS.lock() {
        admissions.entry(session).or_default().push((count, slots));
    }
}

/// The (journal position, Event slots) of every admission the Store accepted for the Session.
pub fn recorded(session: SessionId) -> Vec<(usize, usize)> {
    ADMISSIONS
        .lock()
        .ok()
        .and_then(|admissions| admissions.get(&session).cloned())
        .unwrap_or_default()
}
