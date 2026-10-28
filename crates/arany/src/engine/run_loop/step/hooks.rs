use super::{DispatchSite, Grant, HookDenial, NotDispatched, StopCause, undispatched};
use crate::engine::ledger::{Action, Admission, duration_ms};
use crate::engine::run_loop::{HookRefusal, RunLoop, TOOL_CLEANUP_OVERHEAD};
use crate::engine::{EngineError, STORE_ACTION_OVERHEAD};
use crate::session::{AgentRunId, Event};
use crate::tools::permissions::{
    CommandHook, ConfigError, Decision, HOOK_RESULT_BYTES, HookEvent, HookIgnoreReason,
    HookInvocation, HookOutcome, HookResult, MAX_HOOK_FEEDBACK_BYTES, MAX_HOOK_INPUT_BYTES,
    PermissionConfig, feedback_lines, narrows, peer_name, post_tool_use_input, pre_tool_use_input,
};
use crate::tools::types::model_observation;
use crate::tools::{
    CommandTool, EffectIntent, ToolCall, ToolDisposition, ToolObservation, ToolRuntime,
};
use std::ops::ControlFlow;

mod command_tools;

const MIN_HOOK_RUNTIME_MS: u32 = 1_000;
const FEEDBACK_RESERVE_BYTES: usize = 2 * MAX_HOOK_FEEDBACK_BYTES + 256;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::engine::run_loop) enum HookStop {
    None,
    Requested,
    Uncertain,
    ActiveTime,
}

pub(super) struct HookFold {
    pub(super) restriction: Option<Decision>,
    pub(super) count: u8,
    pub(super) narrowed: Option<ToolCall>,
    pub(super) reason: Option<String>,
    pub(super) stop: HookStop,
}

pub(in crate::engine::run_loop) struct PostFold {
    pub(super) feedback: Vec<String>,
    pub(in crate::engine::run_loop) stop: HookStop,
}

#[derive(Debug, Eq, PartialEq)]
struct HookRun {
    intent_id: uuid::Uuid,
    disposition: ToolDisposition,
    result: HookResult,
    stops: bool,
    cancelled: bool,
    exhausted: bool,
}

#[derive(Debug, Eq, PartialEq)]
enum HookStart {
    Ran(HookRun),
    Exhausted,
}

pub(in crate::engine) struct LifecycleHook<'a> {
    pub(in crate::engine) agent: AgentRunId,
    pub(in crate::engine) event: HookEvent,
    pub(in crate::engine) subject: &'a str,
    pub(in crate::engine) message: &'a str,
    pub(in crate::engine) deadline: Option<tokio::time::Instant>,
    pub(in crate::engine) feedback_fits: bool,
}

struct HookOccurrence<'a> {
    agent: AgentRunId,
    event: HookEvent,
    plan: &'a HookPlan,
    digests: ([u8; 32], [u8; 32]),
}

enum LifecyclePlan {
    Complete(Option<String>),
    Refused(HookRefusal),
    Ready {
        plans: Vec<HookPlan>,
        digest: [u8; 32],
        continuations: usize,
    },
}

type LifecycleInput = ControlFlow<String, (String, ToolRuntime)>;

impl HookStart {
    fn into_run(
        self,
        event: HookEvent,
        parent: &crate::engine::RunCancellation,
    ) -> Admission<HookRun> {
        match self {
            Self::Exhausted => Admission::Exhausted,
            Self::Ran(run) => match event {
                HookEvent::CommandRewrite | HookEvent::PreToolUse | HookEvent::PostToolUse => {
                    Admission::Reserved(run)
                }
                HookEvent::Stop | HookEvent::SubagentStop | HookEvent::PreCompact => {
                    if parent.is_cancelled() {
                        Admission::Reserved(run)
                    } else if run.cancelled && run.exhausted {
                        Admission::Exhausted
                    } else {
                        Admission::Reserved(run)
                    }
                }
            },
        }
    }
}

struct PreHook<'a, 'b> {
    grant: &'a Grant<'b>,
    runtime: &'a ToolRuntime,
    intent: &'a EffectIntent,
    config: &'a PermissionConfig,
    narrowable: bool,
    interrupted: Option<&'a crate::engine::RunCancellation>,
}

struct HookTiming {
    boundary: bool,
    started: tokio::time::Instant,
    ceiling: Option<u64>,
    deadline: Option<tokio::time::Instant>,
    fixed_ms: u64,
}

enum HookClock {
    Pause(crate::engine::run_loop::ActivePause),
    Deadline(tokio::time::Instant),
}

type HookPlanRefusal = (&'static str, Option<(u8, u8)>);

struct HookPlan {
    command_subject: Option<uuid::Uuid>,
    command_tool: Option<Box<CommandTool>>,
    group: u8,
    handler: u8,
    call: ToolCall,
    timeout_ms: u32,
}

fn hook_plan(
    runtime: &ToolRuntime,
    group: u8,
    handler: u8,
    hook: &CommandHook,
) -> Option<HookPlan> {
    Some(HookPlan {
        command_subject: None,
        command_tool: None,
        group,
        handler,
        call: hook_call(runtime, hook)?,
        timeout_ms: u32::try_from(hook.timeout_seconds().saturating_mul(1000)).unwrap_or(u32::MAX),
    })
}

impl HookFold {
    pub(super) fn none() -> Self {
        Self {
            restriction: None,
            count: 0,
            narrowed: None,
            reason: None,
            stop: HookStop::None,
        }
    }

    fn deny(reason: &str, count: u8) -> Self {
        Self {
            restriction: Some(Decision::Deny),
            count,
            narrowed: None,
            reason: Some(reason.into()),
            stop: HookStop::None,
        }
    }

    fn narrow_pre_hook(&mut self, hook: &PreHook<'_, '_>, call: ToolCall) {
        let config = hook.config;
        self.restriction = Some(Decision::Deny);
        if !hook.narrowable {
            self.reason = Some(
                if hook.grant.deadline().is_some() {
                    "delegated Tool input cannot be rewritten"
                } else {
                    "hook narrowing limit"
                }
                .into(),
            );
        } else if narrowing_holds(hook.runtime, config, &hook.intent.call, &call) {
            self.narrowed = Some(call);
        } else {
            self.reason = Some("hook input update does not narrow".into());
        }
    }

    pub(super) fn exhausted(&self) -> bool {
        self.stop == HookStop::ActiveTime
    }

    pub(super) fn refusal(&self, site: DispatchSite) -> (NotDispatched, Option<StopCause>) {
        match (self.stop, site) {
            (HookStop::ActiveTime, DispatchSite::Primary) => {
                (NotDispatched::HookDenied(HookDenial::ActiveTime), None)
            }
            (HookStop::ActiveTime, DispatchSite::Researcher) => (
                NotDispatched::HookDenied(HookDenial::DelegationDeadline),
                Some(StopCause::DelegationDeadline),
            ),
            (HookStop::Uncertain, _) => (
                NotDispatched::HookDenied(HookDenial::Uncertain),
                Some(StopCause::Uncertain),
            ),
            (HookStop::Requested, _) => (
                NotDispatched::HookDenied(HookDenial::Requested),
                Some(StopCause::HookStop),
            ),
            (HookStop::None, _) => (
                NotDispatched::HookDenied(match &self.narrowed {
                    Some(call) => HookDenial::Narrowed(peer_name(call).into_owned()),
                    None => HookDenial::Denied(self.reason.clone().unwrap_or_default()),
                }),
                None,
            ),
        }
    }
}

// The replacement must narrow the call and be no more permissive than the original under the pinned rules.
fn narrowing_holds(
    runtime: &ToolRuntime,
    config: &PermissionConfig,
    original: &ToolCall,
    updated: &ToolCall,
) -> bool {
    let decision = config.decision(updated);
    narrows(original, updated)
        && config.decision(original).narrow(decision) == decision
        && runtime.allows(updated)
}

fn configured_call(hook: &CommandHook) -> ToolCall {
    match hook.args() {
        None => ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), hook.command().into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        },
        Some(args) => ToolCall::Command {
            program: hook.command().into(),
            args: args.to_vec(),
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        },
    }
}

fn hook_call(runtime: &ToolRuntime, hook: &CommandHook) -> Option<ToolCall> {
    let call = configured_call(hook);
    runtime.allows(&call).then_some(call)
}

fn occurrence_digest(
    event: HookEvent,
    agent: crate::session::AgentRunId,
    subject: &str,
    calls: usize,
) -> [u8; 32] {
    if event == HookEvent::PreCompact {
        crate::tools::permissions::compact_subject(subject, calls)
    } else {
        crate::tools::permissions::stop_subject(agent, calls.saturating_sub(1))
    }
}

impl RunLoop<'_> {
    fn hook_plans(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        event: HookEvent,
        call: &ToolCall,
    ) -> Result<Vec<HookPlan>, &'static str> {
        let matched = config
            .matched_tool_hooks(event, call)
            .map_err(|error| match error {
                ConfigError::Limit(_) => "hook limit",
                _ => "hook configuration blocks this Tool",
            })?;
        matched
            .into_iter()
            .map(|(group, handler, hook)| {
                hook_plan(runtime, group, handler, hook).ok_or("hook program is not pinned")
            })
            .collect()
    }

    // Upper bound of Events and active time one step's hooks can add: its own PreToolUse set plus the
    // PostToolUse set of the requested Tool, which runs after a Command's integration step.
    pub(in crate::engine::run_loop) fn hook_admission(
        &self,
        runtime: &ToolRuntime,
        call: &ToolCall,
        post_call: &ToolCall,
    ) -> (usize, u64, u64) {
        let Some(config) = runtime.permissions() else {
            return (0, 0, 0);
        };
        if call.is_internal() {
            return (0, 0, 0);
        }
        let (Ok(pre), Ok(post)) = (
            self.hook_plans(runtime, config, HookEvent::PreToolUse, call),
            self.hook_plans(runtime, config, HookEvent::PostToolUse, post_call),
        ) else {
            return (0, 0, 0);
        };
        let active_ms = |plans: &[HookPlan]| {
            plans
                .iter()
                .map(|plan| {
                    duration_ms(
                        runtime
                            .reservation_overhead(&plan.call)
                            .saturating_add(TOOL_CLEANUP_OVERHEAD)
                            .saturating_add(STORE_ACTION_OVERHEAD),
                    )
                    .saturating_add(u64::from(plan.timeout_ms))
                })
                .sum::<u64>()
                .min(self.ledger().active_remaining_ms().unwrap_or(u64::MAX))
        };
        (pre.len() + post.len(), active_ms(&pre), active_ms(&post))
    }

    pub(in crate::engine::run_loop) fn hook_time(
        &self,
        runtime: &ToolRuntime,
        call: &ToolCall,
        available: std::time::Duration,
    ) -> std::time::Duration {
        let (_, pre, post) = self.hook_admission(runtime, call, call);
        std::time::Duration::from_millis(pre.saturating_add(post)).min(available / 2)
    }

    // Room the next request keeps for the feedback lines PostToolUse hooks may add.
    pub(super) fn feedback_reserve(&self, runtime: &ToolRuntime, call: &ToolCall) -> usize {
        let Some(config) = runtime.permissions() else {
            return 0;
        };
        match (
            self.hook_plans(runtime, config, HookEvent::PreToolUse, call),
            self.hook_plans(runtime, config, HookEvent::PostToolUse, call),
        ) {
            (Ok(pre), Ok(post)) if !pre.is_empty() || !post.is_empty() =>
            // JSON escaping can double a line of quotes or backslashes.
            {
                FEEDBACK_RESERVE_BYTES
            }
            _ => 0,
        }
    }

    pub(in crate::engine::run_loop) fn maximum_feedback_reserve(runtime: &ToolRuntime) -> usize {
        if runtime.permissions().is_some_and(|config| {
            [HookEvent::PreToolUse, HookEvent::PostToolUse]
                .iter()
                .any(|event| !config.hooks().groups(*event).is_empty())
        }) {
            FEEDBACK_RESERVE_BYTES
        } else {
            0
        }
    }

    pub(super) async fn pre_tool_use_hooks(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        intent: &EffectIntent,
        narrowable: bool,
        interrupted: Option<&crate::engine::RunCancellation>,
    ) -> Result<HookFold, EngineError> {
        let plans = match self.hook_plans(runtime, config, HookEvent::PreToolUse, &intent.call) {
            Ok(plans) => plans,
            Err(reason) => return Ok(HookFold::deny(reason, 0)),
        };
        // A PostToolUse set that cannot run after the effect denies the Tool before it.
        let post = match self.hook_plans(runtime, config, HookEvent::PostToolUse, &intent.call) {
            Ok(plans) => plans,
            Err(reason) => return Ok(HookFold::deny(reason, 0)),
        };
        if plans.is_empty() && post.is_empty() {
            return Ok(HookFold::none());
        }
        if !post.is_empty() {
            // The bare form is the smallest stdin a post hook can get, so it must fit the Guard task.
            let Ok(bare) = post_tool_use_input(
                self.session_id,
                config.mode(),
                &intent.call,
                intent.id,
                serde_json::json!("x".repeat(MAX_HOOK_INPUT_BYTES + 1)),
                0,
            ) else {
                return Ok(HookFold::deny("hook input exceeds its limit", 0));
            };
            if !self.task_fits_all(
                &runtime.clone().with_hook_stdin(&bare),
                intent.agent_run_id,
                &post,
            ) {
                return Ok(HookFold::deny("hook input exceeds the Guard task limit", 0));
            }
        }
        if plans.is_empty() {
            return Ok(HookFold::none());
        }
        let Ok(input) = pre_tool_use_input(self.session_id, config.mode(), &intent.call, intent.id)
        else {
            return Ok(HookFold::deny("hook input exceeds its limit", 0));
        };
        let input_digest = crate::validate::digest(input.as_bytes());
        let subject_digest = intent.digest();
        let runtime = runtime.clone().with_hook_stdin(&input);
        // The complete Guard task must encode before any hook starts; a rejected task would strand it.
        if !self.task_fits_all(&runtime, intent.agent_run_id, &plans) {
            return Ok(HookFold::deny("hook input exceeds the Guard task limit", 0));
        }
        self.propose_tool(intent).await?;
        self.run_pre_tool_hooks(
            PreHook {
                grant,
                runtime: &runtime,
                intent,
                config,
                narrowable,
                interrupted,
            },
            plans,
            (subject_digest, input_digest),
        )
        .await
    }

    async fn run_pre_tool_hooks(
        &self,
        hook: PreHook<'_, '_>,
        plans: Vec<HookPlan>,
        digests: ([u8; 32], [u8; 32]),
    ) -> Result<HookFold, EngineError> {
        let mut fold = HookFold::none();
        for plan in plans {
            if hook
                .interrupted
                .is_some_and(crate::engine::RunCancellation::is_cancelled)
            {
                break;
            }
            let run = match self
                .run_hook(
                    hook.runtime,
                    HookOccurrence {
                        agent: hook.intent.agent_run_id,
                        event: HookEvent::PreToolUse,
                        plan: &plan,
                        digests,
                    },
                    hook.grant.deadline(),
                )
                .await?
                .into_run(HookEvent::PreToolUse, &self.controls.cancellation)
            {
                Admission::Reserved(run) => run,
                Admission::Exhausted => {
                    fold.stop = HookStop::ActiveTime;
                    return Ok(fold);
                }
            };
            let HookRun {
                result,
                stops,
                exhausted,
                ..
            } = run;
            fold.count += 1;
            if stops {
                fold.stop = HookStop::Uncertain;
                fold.restriction = Some(Decision::Deny);
                return Ok(fold);
            }
            if exhausted {
                fold.stop = HookStop::ActiveTime;
                fold.restriction = Some(Decision::Deny);
                return Ok(fold);
            }
            match result {
                HookResult::Stopped { .. } => {
                    fold.stop = HookStop::Requested;
                    fold.restriction = Some(Decision::Deny);
                    return Ok(fold);
                }
                HookResult::Restricted {
                    decision: Decision::Deny,
                    reason,
                } => {
                    fold.restriction = Some(Decision::Deny);
                    fold.reason = reason;
                    return Ok(fold);
                }
                HookResult::Narrowed { call } if call == hook.intent.call => {}
                HookResult::Narrowed { call } => {
                    fold.narrow_pre_hook(&hook, call);
                    return Ok(fold);
                }
                HookResult::Restricted {
                    decision: Decision::Ask,
                    ..
                } => fold.restriction = Some(Decision::Ask),
                HookResult::Passed
                | HookResult::Restricted {
                    decision: Decision::Allow,
                    ..
                }
                | HookResult::Blocked { .. }
                | HookResult::Feedback { .. }
                | HookResult::Ignored { .. } => {}
            }
        }
        Ok(fold)
    }

    // Runs after the Tool's `ToolFinished`: feedback never changes what the Tool did.
    pub(in crate::engine::run_loop) async fn post_tool_use_hooks(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        observation: &ToolObservation,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<PostFold, EngineError> {
        let mut fold = PostFold {
            feedback: Vec::new(),
            stop: HookStop::None,
        };
        let call = &observation.intent.call;
        let Ok(plans) = self.hook_plans(runtime, config, HookEvent::PostToolUse, call) else {
            return Ok(fold);
        };
        if plans.is_empty() {
            return Ok(fold);
        }
        let (input, runtime) = self.post_hook_input(runtime, config, observation, &plans)?;
        let input_digest = crate::validate::digest(input.as_bytes());
        let subject_digest = observation.intent.digest();
        let mut results = Vec::new();
        for plan in plans {
            let run = match self
                .run_hook(
                    &runtime,
                    HookOccurrence {
                        agent: observation.intent.agent_run_id,
                        event: HookEvent::PostToolUse,
                        plan: &plan,
                        digests: (subject_digest, input_digest),
                    },
                    deadline,
                )
                .await?
                .into_run(HookEvent::PostToolUse, &self.controls.cancellation)
            {
                Admission::Reserved(run) => run,
                Admission::Exhausted => {
                    fold.stop = HookStop::ActiveTime;
                    break;
                }
            };
            let stop = if run.stops {
                HookStop::Uncertain
            } else if matches!(run.result, HookResult::Stopped { .. }) {
                HookStop::Requested
            } else if run.exhausted {
                HookStop::ActiveTime
            } else {
                HookStop::None
            };
            results.push(run.result);
            if stop != HookStop::None {
                fold.stop = stop;
                break;
            }
        }
        fold.feedback = feedback_lines(&results);
        Ok(fold)
    }

    fn post_hook_input(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        observation: &ToolObservation,
        plans: &[HookPlan],
    ) -> Result<(String, ToolRuntime), EngineError> {
        // Halve the response budget until every Guard task encodes; admission proved the bare form fits.
        let call = &observation.intent.call;
        let response = model_observation(observation);
        let mut budget = MAX_HOOK_INPUT_BYTES;
        loop {
            let input = post_tool_use_input(
                self.session_id,
                config.mode(),
                call,
                observation.intent.id,
                response.clone(),
                budget,
            );
            let Ok(input) = input else {
                if budget == 0 {
                    return Err(EngineError::InvalidRequest);
                }
                budget = if budget < 2048 { 0 } else { budget / 2 };
                continue;
            };
            let sized = runtime.clone().with_hook_stdin(&input);
            if self.task_fits_all(&sized, observation.intent.agent_run_id, plans) {
                return Ok((input, sized));
            }
            if budget == 0 {
                return Err(EngineError::InvalidRequest);
            }
            budget = if budget < 2048 { 0 } else { budget / 2 };
        }
    }

    pub(in crate::engine) fn lifecycle_hooks<'a>(
        &'a self,
        hook: LifecycleHook<'a>,
    ) -> super::BoxedStep<'a, Result<Option<String>, HookRefusal>> {
        Box::pin(self.run_lifecycle_hooks(hook))
    }

    async fn run_lifecycle_hooks(
        &self,
        hook: LifecycleHook<'_>,
    ) -> Result<Result<Option<String>, HookRefusal>, EngineError> {
        use crate::tools::permissions::stop_feedback;
        let Some(runtime) = self.controls.tools else {
            return Ok(Ok(None));
        };
        let Some(config) = runtime.permissions() else {
            return Ok(Ok(None));
        };
        let (plans, digest, continuations) =
            match self.prepare_lifecycle_hook(runtime, config, &hook).await? {
                LifecyclePlan::Complete(feedback) => return Ok(Ok(feedback)),
                LifecyclePlan::Refused(refusal) => return Ok(Err(refusal)),
                LifecyclePlan::Ready {
                    plans,
                    digest,
                    continuations,
                } => (plans, digest, continuations),
            };
        self.journal
            .admit(
                self.session_id,
                2 * plans.len() + self.lifecycle_tail(hook.agent, hook.event),
                plans.len(),
            )
            .await?;
        let (input, runtime) = match self
            .lifecycle_hook_input(&hook, &plans, (digest, continuations))
            .await?
        {
            ControlFlow::Continue(prepared) => prepared,
            ControlFlow::Break(feedback) => return Ok(Ok(Some(feedback))),
        };
        for plan in plans {
            let outcome = self
                .run_hook(
                    &runtime,
                    HookOccurrence {
                        agent: hook.agent,
                        event: hook.event,
                        plan: &plan,
                        digests: (digest, crate::validate::digest(input.as_bytes())),
                    },
                    hook.deadline,
                )
                .await?;
            let run = match outcome.into_run(hook.event, &self.controls.cancellation) {
                Admission::Reserved(run) => run,
                Admission::Exhausted => return Ok(Err(HookRefusal::ActiveTime)),
            };
            if run.cancelled {
                return Ok(Err(HookRefusal::Cancelled));
            }
            if run.stops {
                return Err(crate::tools::ToolError::Uncertain.into());
            }
            if self.controls.cancellation.is_cancelled() {
                return Ok(Err(HookRefusal::Cancelled));
            }
            if matches!(run.result, HookResult::Stopped { .. }) {
                return self.idle_compact_stop(hook.event, digest).map(Ok);
            }
            if run.exhausted && hook.deadline.is_none() {
                return Ok(Err(HookRefusal::ActiveTime));
            }
            if matches!(run.result, HookResult::Blocked { .. }) {
                return Ok(Ok(Some(stop_feedback([&run.result]))));
            }
        }
        Ok(Ok(None))
    }

    async fn prepare_lifecycle_hook(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        hook: &LifecycleHook<'_>,
    ) -> Result<LifecyclePlan, EngineError> {
        use crate::tools::permissions::MAX_STOP_CONTINUATIONS;
        let (index, continuations) = self.ledger().view(|view| {
            let run = view
                .runs
                .iter()
                .find(|run| run.id == self.run_id)
                .ok_or(EngineError::InvalidHistory)?;
            let actor = run
                .agents
                .iter()
                .find(|view| view.id == hook.agent)
                .ok_or(EngineError::InvalidHistory)?;
            Ok::<_, EngineError>((
                actor.provider_calls.len(),
                run.stop_continuations(hook.agent),
            ))
        })??;
        let digest = occurrence_digest(hook.event, hook.agent, hook.subject, index);
        if hook.event != HookEvent::PreCompact && continuations >= MAX_STOP_CONTINUATIONS {
            return Ok(LifecyclePlan::Complete(None));
        }
        let matched: Vec<(u8, u8)> = config
            .matched_hooks(hook.event, |group| group.matches(hook.subject))
            .map(|matched| {
                matched
                    .into_iter()
                    .map(|(group, handler, _)| (group, handler))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(reused) = self.recorded_occurrence(hook.event, digest, &matched)? {
            return Ok(LifecyclePlan::Complete(
                self.idle_compact_stop(hook.event, digest)?.or(reused),
            ));
        }
        let plans = self.lifecycle_plans(runtime, config, hook);
        if plans.as_ref().is_ok_and(Vec::is_empty) {
            return Ok(LifecyclePlan::Complete(None));
        }
        // A block must be able to continue, so a context without room refuses before any hook journals.
        if !hook.feedback_fits {
            return Ok(LifecyclePlan::Refused(HookRefusal::ContextFull));
        }
        let plans = match plans {
            Ok(plans) => plans,
            Err((reason, culprit)) => {
                let (group, handler) = culprit.unwrap_or((0, 0));
                let first = config
                    .hooks()
                    .groups(hook.event)
                    .get(usize::from(group))
                    .and_then(|group| group.commands().get(usize::from(handler)))
                    .map(|hook| (group, handler, configured_call(hook)));
                return self
                    .block_lifecycle(hook, digest, reason, first)
                    .await
                    .map(|feedback| LifecyclePlan::Complete(Some(feedback)));
            }
        };
        Ok(LifecyclePlan::Ready {
            plans,
            digest,
            continuations,
        })
    }

    fn lifecycle_plans(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        hook: &LifecycleHook<'_>,
    ) -> Result<Vec<HookPlan>, HookPlanRefusal> {
        config
            .matched_hooks(hook.event, |group| group.matches(hook.subject))
            .map_err(|_| ("hook limit", None))
            .and_then(|matched| {
                matched
                    .into_iter()
                    .map(|(group, handler, hook)| {
                        hook_plan(runtime, group, handler, hook)
                            .ok_or(("hook program is not pinned", Some((group, handler))))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
    }

    async fn lifecycle_hook_input(
        &self,
        hook: &LifecycleHook<'_>,
        plans: &[HookPlan],
        occurrence: ([u8; 32], usize),
    ) -> Result<LifecycleInput, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let config = runtime.permissions().ok_or(EngineError::InvalidRequest)?;
        let (digest, continuations) = occurrence;
        let mut budget = MAX_HOOK_INPUT_BYTES;
        loop {
            let input = crate::tools::permissions::lifecycle_input(
                self.session_id,
                config.mode(),
                hook.event,
                continuations > 0,
                hook.message,
                hook.subject,
                budget,
            );
            let sized = runtime.clone().with_hook_stdin(&input);
            if self.task_fits_all(&sized, hook.agent, plans) {
                return Ok(ControlFlow::Continue((input, sized)));
            }
            if budget == 0 {
                let first = plans
                    .first()
                    .map(|plan| (plan.group, plan.handler, plan.call.clone()));
                return self
                    .block_lifecycle(
                        hook,
                        digest,
                        "hook input exceeds the Guard task limit",
                        first,
                    )
                    .await
                    .map(ControlFlow::Break);
            }
            budget = if budget < 2048 { 0 } else { budget / 2 };
        }
    }

    fn idle_compact_stop(
        &self,
        event: HookEvent,
        digest: [u8; 32],
    ) -> Result<Option<String>, EngineError> {
        self.ledger().view(|view| {
            (event == HookEvent::PreCompact
                && view.runs.last().is_some_and(|run| {
                    run.status != crate::session::RunStatus::Active
                        && run.lifecycle_hooks.iter().any(|hook| {
                            hook.invocation.event == event
                                && hook.invocation.subject_digest == digest
                                && hook.outcome.as_ref().is_some_and(|outcome| {
                                    matches!(outcome.result, HookResult::Stopped { .. })
                                })
                        })
                }))
            .then(|| "Hook stopped compaction".into())
        })
    }

    // An occurrence runs once: a retry reuses its journaled outcome and never reruns a hook.
    fn recorded_occurrence(
        &self,
        event: HookEvent,
        digest: [u8; 32],
        matched: &[(u8, u8)],
    ) -> Result<Option<Option<String>>, EngineError> {
        let recorded = self.ledger().view(|view| {
            view.runs
                .iter()
                .find(|run| run.id == self.run_id)
                .map(|run| {
                    run.lifecycle_hooks
                        .iter()
                        .filter(|hook| {
                            hook.invocation.event == event
                                && hook.invocation.subject_digest == digest
                        })
                        .map(|hook| {
                            (
                                (hook.invocation.group, hook.invocation.handler),
                                hook.outcome.as_ref().map(|outcome| outcome.result.clone()),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })?;
        Ok(occurrence_verdict(&recorded, matched))
    }

    // SubagentStop preserves the actor's complete team settlement tail.
    fn lifecycle_tail(&self, agent: AgentRunId, event: HookEvent) -> usize {
        let tail = if event == HookEvent::SubagentStop
            && self
                .ledger()
                .view(|v| {
                    v.runs.last().is_some_and(|r| {
                        r.agents
                            .iter()
                            .any(|a| a.id == agent && a.assignment.is_some())
                    })
                })
                .unwrap_or(false)
        {
            Action::Worker
        } else if event == HookEvent::SubagentStop {
            Action::Child {
                in_flight: self.ledger().in_flight_children(),
                research: true,
            }
        } else {
            self.primary_call()
        };
        let verdicts = self.ledger().verdicts(tail);
        tail.max_events_for_admitted(&self.routing.providers)
            + verdicts
            + usize::from(
                verdicts > 0 && matches!(event, HookEvent::Stop | HookEvent::SubagentStop),
            )
    }

    fn lifecycle_ceiling(&self, pending_ms: u64) -> Result<u64, EngineError> {
        let ceiling = self.ledger().active_ceiling(pending_ms)?;
        let remaining = self.ledger().view(|view| {
            self.goal_id.and_then(|id| view.goal(id)).and_then(|goal| {
                goal.effective_budget()
                    .max_active_ms
                    .map(|limit| limit.saturating_sub(goal.active_ms))
            })
        })?;
        Ok(ceiling.min(remaining.unwrap_or(u64::MAX)))
    }

    // A refused occurrence journals the handler's own call as a denied intent that never dispatched.
    async fn block_lifecycle(
        &self,
        hook: &LifecycleHook<'_>,
        digest: [u8; 32],
        reason: &str,
        handler: Option<(u8, u8, ToolCall)>,
    ) -> Result<String, EngineError> {
        let (group, handler, call) = handler.ok_or(EngineError::InvalidRequest)?;
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let config = runtime.permissions().ok_or(EngineError::InvalidRequest)?;
        let input = crate::tools::permissions::lifecycle_input(
            self.session_id,
            config.mode(),
            hook.event,
            false,
            "",
            hook.subject,
            MAX_HOOK_INPUT_BYTES,
        );
        let intent = runtime.intent(self.run_id, hook.agent, call);
        self.journal
            .admit(
                self.session_id,
                2 + self.lifecycle_tail(hook.agent, hook.event),
                1,
            )
            .await?;
        let active_ceiling_ms = match self
            .ledger()
            .view(|view| view.active_goal().map(|goal| goal.active_run.is_some()))?
        {
            Some(true) => Some(self.lifecycle_ceiling(0)?),
            Some(false) => Some(1),
            None => None,
        };
        self.append(Event::HookStarted {
            run_id: self.run_id,
            agent_run_id: hook.agent,
            hook: HookInvocation {
                event: hook.event,
                group,
                handler,
                subject_digest: digest,
                intent: intent.clone(),
                command_subject: None,
                command_tool: None,
                input_digest: crate::validate::digest(input.as_bytes()),
                active_ceiling_ms,
            },
        })
        .await?;
        let observation = undispatched(intent, ToolDisposition::Denied, reason);
        self.append(Event::HookFinished {
            run_id: self.run_id,
            agent_run_id: hook.agent,
            outcome: HookOutcome {
                result: HookResult::for_event(hook.event, &observation),
                observation,
                boundary_active_ms: self.ledger().view(|view| {
                    view.active_goal()
                        .filter(|goal| goal.active_run.is_none())
                        .map(|_| 0)
                })?,
            },
        })
        .await?;
        Ok(reason.into())
    }

    fn task_fits_all(
        &self,
        runtime: &ToolRuntime,
        agent_run_id: crate::session::AgentRunId,
        plans: &[HookPlan],
    ) -> bool {
        plans.iter().all(|plan| {
            let mut probe = runtime.intent(self.run_id, agent_run_id, plan.call.clone());
            probe.limits.runtime_ms = u32::MAX;
            runtime.hook_task_fits(&probe)
        })
    }

    async fn dispatch_hook(
        &self,
        runtime: &ToolRuntime,
        hook_intent: EffectIntent,
        clock: HookClock,
    ) -> Result<
        (
            ToolObservation,
            Option<crate::engine::run_loop::ActiveSegment>,
            bool,
        ),
        EngineError,
    > {
        Ok(match clock {
            HookClock::Pause(pause) => {
                let (observation, segment, exhausted, _) = Box::pin(self.dispatch_effect(
                    runtime,
                    hook_intent,
                    Some(pause),
                    &self.controls.cancellation,
                ))
                .await?;
                (observation, segment, exhausted)
            }
            HookClock::Deadline(deadline) => {
                let cancellation = crate::engine::RunCancellation::new();
                let mut parent = self.controls.cancellation.clone();
                let _active_work = self.ledger().active_work();
                let mut execution = Box::pin(runtime.execute(hook_intent, cancellation.clone()));
                let observation = tokio::select! {
                    observation = &mut execution => observation,
                    () = tokio::time::sleep_until(deadline) => { cancellation.cancel(); execution.await },
                    () = parent.cancelled() => { cancellation.cancel(); execution.await },
                };
                (observation, None, tokio::time::Instant::now() >= deadline)
            }
        })
    }

    fn hook_timing(
        &self,
        runtime: &ToolRuntime,
        plan: &HookPlan,
        mut deadline: Option<tokio::time::Instant>,
    ) -> Result<HookTiming, EngineError> {
        let fixed_ms = duration_ms(
            runtime
                .reservation_overhead(&plan.call)
                .saturating_add(TOOL_CLEANUP_OVERHEAD)
                .saturating_add(STORE_ACTION_OVERHEAD),
        );
        let boundary = self.ledger().view(|view| {
            view.runs
                .iter()
                .find(|run| run.id == self.run_id)
                .is_some_and(|run| run.status != crate::session::RunStatus::Active)
        })?;
        let boundary_goal = self.ledger().view(|view| {
            view.active_goal()
                .filter(|goal| {
                    goal.active_run.is_none() && goal.run_ids.last() == Some(&self.run_id)
                })
                .map(|goal| (goal.active_ms, goal.effective_budget().max_active_ms))
        })?;
        let started = tokio::time::Instant::now();
        let boundary_remaining =
            boundary_goal.and_then(|(used, limit)| limit.map(|limit| limit.saturating_sub(used)));
        if boundary {
            deadline = Some(
                started
                    + std::time::Duration::from_millis(
                        u64::from(plan.timeout_ms)
                            .saturating_add(fixed_ms)
                            .min(boundary_remaining.unwrap_or(u64::MAX)),
                    ),
            );
        }
        let ceiling = boundary_goal.map(|_| {
            deadline.map_or(0, |deadline| {
                duration_ms(deadline.saturating_duration_since(started))
            })
        });
        Ok(HookTiming {
            boundary,
            started,
            ceiling,
            deadline,
            fixed_ms,
        })
    }

    async fn run_hook(
        &self,
        runtime: &ToolRuntime,
        occurrence: HookOccurrence<'_>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<HookStart, EngineError> {
        let HookOccurrence {
            agent: agent_run_id,
            event,
            plan,
            digests,
        } = &occurrence;
        let timing = self.hook_timing(runtime, plan, deadline)?;
        let HookTiming {
            boundary,
            ceiling: boundary_ceiling,
            deadline,
            fixed_ms,
            ..
        } = timing;
        // The Goal ledger refuses a start that does not fit, so a shortfall never runs the hook.
        let active_ms = if let Some(deadline) = deadline {
            duration_ms(deadline.saturating_duration_since(tokio::time::Instant::now()))
                .saturating_sub(fixed_ms)
        } else {
            self.ledger()
                .active_remaining_ms()
                .map_or(u64::MAX, |remaining| remaining.saturating_sub(fixed_ms))
                .max(u64::from(MIN_HOOK_RUNTIME_MS))
        };
        if active_ms < u64::from(MIN_HOOK_RUNTIME_MS) {
            return Ok(HookStart::Exhausted);
        }
        let runtime_ms =
            u32::try_from(active_ms.min(u64::from(plan.timeout_ms))).unwrap_or(u32::MAX);
        let hook_intent = self.hook_intent(runtime, &occurrence, runtime_ms);
        let clock = match deadline {
            Some(deadline) => Admission::Reserved(HookClock::Deadline(deadline)),
            None => self.reserve_active_pause()?.map(HookClock::Pause),
        };
        let Admission::Reserved(clock) = clock else {
            return Ok(HookStart::Exhausted);
        };
        self.append(Event::HookStarted {
            run_id: self.run_id,
            agent_run_id: *agent_run_id,
            hook: HookInvocation {
                active_ceiling_ms: if boundary {
                    boundary_ceiling
                } else if matches!(
                    event,
                    HookEvent::CommandRewrite
                        | HookEvent::Stop
                        | HookEvent::SubagentStop
                        | HookEvent::PreCompact
                ) {
                    Some(self.lifecycle_ceiling(if deadline.is_some() {
                        0
                    } else {
                        fixed_ms.saturating_add(u64::from(runtime_ms))
                    })?)
                } else {
                    None
                },
                event: *event,
                group: plan.group,
                handler: plan.handler,
                subject_digest: digests.0,
                intent: hook_intent.clone(),
                command_subject: plan.command_subject,
                command_tool: plan.command_tool.clone(),
                input_digest: digests.1,
            },
        })
        .await?;
        let (observation, segment, exhausted) =
            Box::pin(self.dispatch_hook(runtime, hook_intent, clock)).await?;
        self.clear_tool_queue();
        if let Some(segment) = segment {
            self.settle_active_segment(segment)?;
        }
        self.record_hook_finished(occurrence, &timing, observation, exhausted)
            .await
    }

    fn hook_intent(
        &self,
        runtime: &ToolRuntime,
        occurrence: &HookOccurrence<'_>,
        runtime_ms: u32,
    ) -> EffectIntent {
        let mut hook_intent =
            runtime.intent(self.run_id, occurrence.agent, occurrence.plan.call.clone());
        let limits = &mut hook_intent.limits;
        limits.result_bytes = HOOK_RESULT_BYTES;
        limits.runtime_ms = runtime_ms;
        limits.cache_requested_bytes = 0;
        limits.cache_bytes = 0;
        limits.cache_candidate_bytes = 0;
        limits.cache_transfer_ms = 0;
        hook_intent
    }

    async fn record_hook_finished(
        &self,
        occurrence: HookOccurrence<'_>,
        timing: &HookTiming,
        observation: ToolObservation,
        exhausted: bool,
    ) -> Result<HookStart, EngineError> {
        let intent_id = observation.intent.id;
        let disposition = observation.disposition;
        let stops = observation.stops_run();
        let cancelled = observation.disposition == ToolDisposition::Cancelled;
        let result = HookResult::for_event(occurrence.event, &observation);
        self.append(Event::HookFinished {
            run_id: self.run_id,
            agent_run_id: occurrence.agent,
            outcome: HookOutcome {
                boundary_active_ms: if timing.boundary {
                    timing
                        .ceiling
                        .map(|ceiling| duration_ms(timing.started.elapsed()).min(ceiling))
                } else {
                    None
                },
                observation,
                result: result.clone(),
            },
        })
        .await?;
        Ok(HookStart::Ran(HookRun {
            intent_id,
            disposition,
            result,
            stops,
            cancelled,
            exhausted,
        }))
    }
}

type Recorded = ((u8, u8), Option<HookResult>);

// `None` means the occurrence has not run; `Some(None)` reuses a complete pass.
fn occurrence_verdict(recorded: &[Recorded], matched: &[(u8, u8)]) -> Option<Option<String>> {
    if recorded.is_empty() {
        return None;
    }
    if recorded
        .iter()
        .any(|(_, result)| matches!(result, Some(HookResult::Stopped { .. })))
    {
        return Some(None);
    }
    let refused = || Some("hook occurrence already attempted".to_owned());
    let verdict = recorded.iter().find_map(|(_, result)| match result {
        Some(result @ HookResult::Blocked { .. }) => {
            Some(crate::tools::permissions::stop_feedback([result]))
        }
        Some(HookResult::Ignored {
            why: HookIgnoreReason::NotCompleted,
        })
        | None => refused(),
        Some(_) => None,
    });
    // A pass is reusable only when every currently matched handler finished; a partial occurrence fails closed.
    let complete = recorded.len() == matched.len()
        && matched
            .iter()
            .all(|handler| recorded.iter().any(|(done, _)| done == handler));
    Some(verdict.or_else(|| if complete { None } else { refused() }))
}

#[cfg(test)]
mod tests;
