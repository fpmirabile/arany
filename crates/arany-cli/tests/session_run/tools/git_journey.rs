use arany::{
    Answer, CompactionRequest, CompactionResponse, Provider, ProviderError, ProviderOutcome,
    ProviderRequest, ProviderResponse, ToolCall,
};
use std::sync::{Arc, Mutex};

pub(crate) struct GitJourney {
    pub(crate) requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait::async_trait]
impl Provider for GitJourney {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "tool-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let step = requests.len();
        requests.push(request);
        let outcome = match step {
            0 => crate::tool_outcome(ToolCall::GitStatus { path: ".".into() }),
            1 => crate::tool_outcome(ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: false,
            }),
            2 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: None,
                max_count: 5,
            }),
            3 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: Some("missing-branch".into()),
                max_count: 1,
            }),
            4 => crate::tool_outcome(ToolCall::GitBranch {
                name: None,
                start_point: None,
                scope: Some("all".into()),
                base: Some("HEAD~1".into()),
                max_count: Some(16),
            }),
            5 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: Some("HEAD~1..protected".into()),
                max_count: 5,
            }),
            6 => crate::tool_outcome(ToolCall::GitDiff {
                path: ".".into(),
                base: Some("HEAD~1".into()),
                head: Some("HEAD".into()),
                staged: false,
            }),
            7 => crate::tool_outcome(ToolCall::GitLog {
                path: "denied/history.txt".into(),
                revision: Some("HEAD~1..protected".into()),
                max_count: 5,
            }),
            8 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: Some("missing-endpoint..HEAD".into()),
                max_count: 5,
            }),
            9 => ProviderOutcome::Answer(Answer {
                text: "Git state observed".into(),
            }),
            _ => return Err(ProviderError::InvalidOutcome),
        };
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        _request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}
