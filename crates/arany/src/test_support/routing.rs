use crate::{
    AdmittedProviders, Engine, EngineConfig, Event, Finish, ProviderOutcome, RunControls,
    RunRequest, SessionId, SessionView, StateRoot, Store, ToolGrant,
};
use std::path::Path;
use std::sync::Arc;

use super::scenario::*;

pub fn finish(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: text.into(),
        result: text.into(),
    })
}

pub async fn reopen(state: &Path, session: SessionId) -> (Vec<crate::EventEnvelope>, SessionView) {
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = SessionView::replay(session, &events).unwrap().unwrap();
    let previews = events
        .iter()
        .any(|event| matches!(event.event, Event::AssistantDisplayRecorded { .. }));
    super::goals::assert_action_bounds(&events, previews, false);
    store.close().await.unwrap();
    (events, view)
}

pub fn input(workspace: &Path) -> RunRequest {
    let mut input = request(workspace, "Complete the task");
    input.include_paths.clear();
    input
}

pub fn route(provider: ScriptedProvider) -> Arc<ScriptedProvider> {
    Arc::new(provider)
}

pub fn routes(primary: &Arc<ScriptedProvider>) -> AdmittedProviders {
    AdmittedProviders::single(primary.clone())
}

pub fn open(state: &Path) -> Engine {
    Engine::open(StateRoot::admit(state).unwrap(), EngineConfig::default()).unwrap()
}

/// Tool grants that read `notes/facts.txt` through the Guard.
pub fn configured() -> RunControls {
    RunControls {
        tools: ToolGrant::Configured,
        ..RunControls::default()
    }
}
