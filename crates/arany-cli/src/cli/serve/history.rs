use super::{
    items::{self, notification},
    server::{Fault, emit, params, response, send},
    threads::{self, Threads},
};
use arany::{Engine, EngineError, SessionId, Tapped};
use arany_cli::protocol::{EventLine, methods::*, rpc::*};
use serde_json::Value;
use std::{collections::HashMap, future::Future, path::Path, task::Poll};
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct History {
    cursors: HashMap<SessionId, u64>,
}

impl History {
    pub async fn dispatch(
        &mut self,
        engine: &Engine,
        state: &Path,
        threads: &mut Threads,
        request: Request,
        outbound: &mpsc::Sender<Message>,
        opt_out: &[String],
    ) -> Result<Option<Value>, Fault> {
        match request.method.as_str() {
            "events/subscribe" => {
                let input: EventsSubscribeParams = params(request.params)?;
                require_thread(threads, input.thread_id)?;
                let store = threads::read_store(state)?;
                let result = store.load_session(input.thread_id).await;
                let closed = store.close().await;
                let events = result.map_err(EngineError::from).map_err(Fault::engine)?;
                closed.map_err(EngineError::from).map_err(Fault::engine)?;
                let head = events.last().map_or(0, |event| event.sequence);
                if input.since > head {
                    return Err(Fault::params());
                }
                let result = event_page(&request.id, &events, input.since)?;
                if result.next_since.is_none() {
                    self.cursors.insert(input.thread_id, head);
                } else {
                    self.cursors.remove(&input.thread_id);
                }
                Ok(Some(
                    serde_json::to_value(result).expect("typed Event page"),
                ))
            }
            "receipt/get" => {
                let input: ReceiptGetParams = params(request.params)?;
                require_thread(threads, input.thread_id)?;
                let view = threads::load_view(state, input.thread_id).await?;
                let goal = view
                    .goals
                    .iter()
                    .find(|goal| goal.id == input.goal_id)
                    .ok_or_else(|| Fault::engine(EngineError::GoalUnavailable))?;
                let result = receipt(goal);
                bounded_result(request.id, result)
            }
            "thread/fork" => {
                let input: ThreadForkParams = params(request.params)?;
                let mut loaded = require_thread(threads, input.thread_id)?.clone();
                threads.check_capacity(None)?;
                let (workspace, _) =
                    crate::cli::admit_workspace(Some(input.cwd.into()), Some(state.into()))
                        .map_err(|_| Fault::params())?;
                let id = engine
                    .fork_session(input.thread_id, workspace.clone(), input.title)
                    .await
                    .map_err(Fault::engine)?;
                let view = threads::load_view(state, id).await?;
                let created = threads::created_at_ms(state, id).await?;
                let result = ThreadResult {
                    thread: threads::thread(&view, created, &workspace),
                };
                let reply = response(request.id, &result);
                if encode_line(&reply).is_err() {
                    return Err(Fault::engine(EngineError::ContextTooLarge));
                }
                loaded.workspace = workspace;
                threads.map.insert(id, loaded);
                send(outbound, reply)
                    .await
                    .map_err(|_| Fault::engine(EngineError::CoordinatorFailed))?;
                emit(outbound, opt_out, notification("thread/started", result))
                    .await
                    .map_err(|_| Fault::engine(EngineError::CoordinatorFailed))?;
                Ok(None)
            }
            _ => unreachable!("history method dispatched by server"),
        }
    }

    pub async fn drain(
        &mut self,
        committed: &mut arany::Subscription,
        outbound: &mpsc::Sender<Message>,
        opt_out: &[String],
    ) -> Result<(), String> {
        loop {
            let event = {
                let mut next = std::pin::pin!(committed.next());
                std::future::poll_fn(|cx| {
                    Poll::Ready(match next.as_mut().poll(cx) {
                        Poll::Ready(event) => event,
                        Poll::Pending => None,
                    })
                })
                .await
            };
            let Some(event) = event else { return Ok(()) };
            self.observe(event, outbound, opt_out).await?;
        }
    }

    pub async fn observe(
        &mut self,
        tapped: Tapped,
        outbound: &mpsc::Sender<Message>,
        opt_out: &[String],
    ) -> Result<(), String> {
        match tapped {
            Tapped::Committed(envelope) => {
                let Some(cursor) = self.cursors.get_mut(&envelope.session_id) else {
                    return Ok(());
                };
                if envelope.sequence <= *cursor {
                    return Ok(());
                }
                let note = notification(
                    "events/committed",
                    EventsCommittedNotification {
                        thread_id: envelope.session_id,
                        event: EventLine::from(envelope.as_ref()),
                    },
                );
                if encode_line(&note).is_err() {
                    emit(
                        outbound,
                        opt_out,
                        notification(
                            "events/resync",
                            EventsResyncNotification {
                                thread_id: Some(envelope.session_id),
                                missed: 1,
                                last_sequence: *cursor,
                            },
                        ),
                    )
                    .await?;
                    self.cursors.remove(&envelope.session_id);
                } else {
                    emit(outbound, opt_out, note).await?;
                    *cursor = envelope.sequence;
                }
            }
            Tapped::Resync { missed } => {
                // Sort so multi-thread recovery is independent of HashMap iteration order.
                let mut cursors: Vec<_> = self.cursors.iter().collect();
                cursors.sort_unstable_by_key(|(id, _)| id.to_string());
                for (&id, &cursor) in cursors {
                    emit(
                        outbound,
                        opt_out,
                        notification(
                            "events/resync",
                            EventsResyncNotification {
                                thread_id: Some(id),
                                missed,
                                last_sequence: cursor,
                            },
                        ),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
}

fn require_thread(threads: &Threads, id: SessionId) -> Result<&threads::LoadedThread, Fault> {
    threads
        .map
        .get(&id)
        .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))
}

fn event_page(
    id: &RequestId,
    events: &[arany::EventEnvelope],
    since: u64,
) -> Result<EventsSubscribeResult, Fault> {
    let mut result = EventsSubscribeResult {
        events: Vec::new(),
        next_since: Some(u64::MAX),
    };
    let mut bytes = encode_line(&response(id.clone(), &result))
        .map_err(|_| Fault::engine(EngineError::ContextTooLarge))?
        .len();
    for envelope in events.iter().filter(|event| event.sequence > since) {
        let event = EventLine::from(envelope);
        // A line's newline reserves the comma separating page entries.
        let size = encode_line(&event).map_or(MAX_LINE_BYTES + 1, |line| line.len());
        if result.events.len() == MAX_EVENT_PAGE || size > MAX_LINE_BYTES - bytes {
            let last = result
                .events
                .last()
                .ok_or_else(|| Fault::engine(EngineError::ContextTooLarge))?;
            result.next_since = Some(last.sequence);
            return Ok(result);
        }
        bytes += size;
        result.events.push(event);
    }
    result.next_since = None;
    Ok(result)
}

fn bounded_result(id: RequestId, result: impl serde::Serialize) -> Result<Option<Value>, Fault> {
    let Message::Response(reply) = response(id, result) else {
        unreachable!("response envelope")
    };
    encode_line(&reply).map_err(|_| Fault::engine(EngineError::ContextTooLarge))?;
    Ok(Some(reply.result))
}

fn receipt(goal: &arany::GoalView) -> ReceiptGetResult {
    ReceiptGetResult {
        goal: ReceiptGoal {
            id: goal.id,
            status: items::goal_summary(goal).status,
            objective: goal.objective.clone(),
            budget: ReceiptBudget {
                max_provider_calls: goal.budget.max_provider_calls,
                max_tokens: goal.budget.max_tokens,
                max_active_ms: goal.budget.max_active_ms,
                max_runs: goal.budget.max_runs,
            },
            usage: ReceiptUsage {
                runs: goal.run_ids.len(),
                provider_calls: goal.provider_calls,
                active_ms: goal.active_ms,
                uncertain_active_ms: goal.uncertain_active_ms,
                known_tokens: goal.known_tokens,
                known_cached_tokens: goal.known_cached_tokens,
                token_usage_unknown: goal.token_usage_unknown,
            },
            report: goal.report.as_ref().map(|report| ReceiptReport {
                summary: report.summary.clone(),
                changes: report.changes.clone(),
                evidence: report.evidence.clone(),
                unresolved: report.unresolved.clone(),
                cached_input_tokens: report.cached_input_tokens,
            }),
        },
    }
}
