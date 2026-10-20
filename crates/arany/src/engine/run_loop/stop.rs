#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StopCause {
    Uncertain,
    Cancelled,
    HookStop,
    Paused,
    ActiveTime,
    Budget,
    Capacity,
    ContextFull,
    DelegationDeadline,
    ToolStopped,
    InvalidCommandResult,
}

impl StopCause {
    const fn rank(self) -> u8 {
        match self {
            Self::Uncertain => 11,
            Self::Cancelled => 10,
            Self::HookStop => 9,
            Self::Paused => 8,
            Self::ActiveTime => 7,
            Self::Budget => 6,
            Self::Capacity => 5,
            Self::ContextFull => 4,
            Self::DelegationDeadline => 3,
            Self::ToolStopped => 2,
            Self::InvalidCommandResult => 1,
        }
    }

    pub(super) fn merge(self, later: Self) -> Self {
        if later.rank() > self.rank() {
            later
        } else {
            self
        }
    }
}

#[must_use]
pub(super) enum StepEnd {
    Continue,
    Stopped(StopCause),
}

pub(super) enum SizingRefusal {
    DelegationDeadline,
    ActiveTime,
    ContextFull,
}

impl From<SizingRefusal> for StopCause {
    fn from(refusal: SizingRefusal) -> Self {
        match refusal {
            SizingRefusal::DelegationDeadline => Self::DelegationDeadline,
            SizingRefusal::ActiveTime => Self::ActiveTime,
            SizingRefusal::ContextFull => Self::ContextFull,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizing_and_stop_precedence() {
        for (refusal, expected) in [
            (SizingRefusal::ActiveTime, StopCause::ActiveTime),
            (SizingRefusal::ContextFull, StopCause::ContextFull),
            (
                SizingRefusal::DelegationDeadline,
                StopCause::DelegationDeadline,
            ),
        ] {
            assert_eq!(StopCause::from(refusal), expected);
        }
        let causes = [
            StopCause::Uncertain,
            StopCause::Cancelled,
            StopCause::HookStop,
            StopCause::Paused,
            StopCause::ActiveTime,
            StopCause::Budget,
            StopCause::Capacity,
            StopCause::ContextFull,
            StopCause::DelegationDeadline,
            StopCause::ToolStopped,
            StopCause::InvalidCommandResult,
        ];
        for (index, cause) in causes.into_iter().enumerate() {
            assert_eq!(cause.merge(cause), cause);
            for (later_index, later) in causes.into_iter().enumerate() {
                let merged = cause.merge(later);
                assert!(merged.rank() >= cause.rank() && merged.rank() >= later.rank());
                assert_eq!(
                    merged,
                    if index <= later_index || cause.rank() == later.rank() {
                        cause
                    } else {
                        later
                    }
                );
                for last in causes {
                    assert_eq!(
                        cause.merge(later).merge(last),
                        cause.merge(later.merge(last))
                    );
                }
            }
        }
    }
}
