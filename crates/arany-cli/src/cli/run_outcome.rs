use arany::{EngineError, GoalLimit, RunStatus, RunStop};
use std::time::Duration;

// Why an autonomous loop stopped after its committed answer: the failure that ended it, else its
// projected stop. A pause the caller asked for is no stop.
pub(in crate::cli) fn stop_after_answer(
    status: RunStatus,
    stop: Option<RunStop>,
    continuation_error: Option<&EngineError>,
    pause_requested: bool,
) -> Option<String> {
    if status != RunStatus::Finished {
        return None;
    }
    if let Some(error) = continuation_error {
        return Some(error.to_string());
    }
    match stop? {
        RunStop::Paused if pause_requested => None,
        stop @ (RunStop::GoalExhausted(
            GoalLimit::Calls | GoalLimit::Runs | GoalLimit::ActiveTime | GoalLimit::Tokens,
        )
        | RunStop::Paused
        | RunStop::Cancelled
        | RunStop::StoppedByHook
        | RunStop::Uncertain
        | RunStop::Interrupted
        | RunStop::Failed(_)) => Some(stop_text(stop).to_owned()),
    }
}

pub(in crate::cli) fn stop_text(stop: RunStop) -> &'static str {
    match stop {
        RunStop::GoalExhausted(
            GoalLimit::Calls | GoalLimit::Runs | GoalLimit::ActiveTime | GoalLimit::Tokens,
        ) => "Goal budget is exhausted",
        RunStop::Paused => "Goal paused before completion",
        RunStop::Uncertain => "Tool effect completion is uncertain",
        RunStop::Interrupted => "Run interrupted without a terminal receipt; check Session history",
        RunStop::Cancelled => "Run cancelled",
        RunStop::StoppedByHook => "Run stopped by hook",
        RunStop::Failed(_) => "Run failed",
    }
}

pub(in crate::cli) const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::cli) enum StopSignal {
    Interrupt,
    Hangup,
    Terminate,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::cli) enum StopStep {
    Cancel,
    Force(&'static str),
}

pub(in crate::cli) fn stop_step(cancelling: bool, signal: StopSignal) -> StopStep {
    match (signal, cancelling) {
        (StopSignal::Interrupt, false) => StopStep::Cancel,
        (StopSignal::Interrupt, true) => StopStep::Force("forced shutdown after second Ctrl+C"),
        (StopSignal::Hangup, _) => StopStep::Force("terminated by SIGHUP"),
        (StopSignal::Terminate, _) => StopStep::Force("terminated by SIGTERM"),
    }
}

pub(in crate::cli) struct StopSignals {
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl StopSignals {
    pub(in crate::cli) fn install() -> std::io::Result<Self> {
        // Tokio never restores default signal dispositions, so a SIGINT after the Run is absorbed.
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    pub(in crate::cli) async fn next(&mut self) -> StopSignal {
        tokio::select! {
            _ = self.interrupt.recv() => StopSignal::Interrupt,
            _ = self.hangup.recv() => StopSignal::Hangup,
            _ = self.terminate.recv() => StopSignal::Terminate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StopSignal, StopStep, stop_step};

    #[test]
    fn stop_signals_cancel_once_then_force() {
        assert_eq!(stop_step(false, StopSignal::Interrupt), StopStep::Cancel);
        assert_eq!(
            stop_step(true, StopSignal::Interrupt),
            StopStep::Force("forced shutdown after second Ctrl+C")
        );
        assert_eq!(
            stop_step(false, StopSignal::Terminate),
            StopStep::Force("terminated by SIGTERM")
        );
        assert_eq!(
            stop_step(true, StopSignal::Hangup),
            StopStep::Force("terminated by SIGHUP")
        );
    }
}
