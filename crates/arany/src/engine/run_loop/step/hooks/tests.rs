use super::*;

#[test]
fn a_partial_pass_fails_closed_and_a_complete_one_is_reused() {
    let passed = |handler| ((0, handler), Some(HookResult::Passed));
    assert_eq!(occurrence_verdict(&[], &[(0, 0)]), None);
    assert_eq!(
        occurrence_verdict(
            &[(
                (0, 0),
                Some(HookResult::Stopped {
                    reason: Some("halt".into())
                })
            )],
            &[(0, 0), (0, 1)]
        ),
        Some(None)
    );
    assert_eq!(
        occurrence_verdict(&[passed(0), passed(1)], &[(0, 0), (0, 1)]),
        Some(None)
    );
    assert!(matches!(
        occurrence_verdict(&[passed(0)], &[(0, 0), (0, 1)]),
        Some(Some(_))
    ));
    assert!(matches!(
        occurrence_verdict(&[passed(0), ((0, 1), None)], &[(0, 0), (0, 1)]),
        Some(Some(_))
    ));
}

#[test]
fn hook_start_preserves_event_specific_cancellation() {
    use HookEvent::{PostToolUse, PreCompact, PreToolUse, Stop, SubagentStop};
    let rows = [
        (false, false, false, false, Stop, true),
        (false, true, false, false, Stop, true),
        (true, false, false, false, Stop, true),
        (true, true, false, false, Stop, false),
        (false, false, true, false, Stop, true),
        (false, false, false, true, Stop, false),
        (true, false, false, true, Stop, false),
        (true, true, true, true, Stop, false),
        (true, true, false, false, SubagentStop, false),
        (true, true, false, false, PreCompact, false),
        (true, true, false, true, PreToolUse, true),
        (true, true, false, true, PostToolUse, true),
    ];
    for (stops, cancelled, exhausted, parent_cancelled, event, starts) in rows {
        let run = || HookRun {
            result: HookResult::Passed,
            stops,
            cancelled,
            exhausted,
        };
        let parent = crate::engine::RunCancellation::new();
        if parent_cancelled {
            parent.cancel();
        }
        let outcome = HookStart::Ran(run()).into_run(event, &parent);
        assert_eq!(
            matches!(outcome, Admission::Reserved(_)),
            starts,
            "{event:?}"
        );
        if let Admission::Reserved(actual) = outcome {
            assert_eq!(actual, run());
        }
    }
    for event in [PreToolUse, PostToolUse, Stop, SubagentStop, PreCompact] {
        let parent = crate::engine::RunCancellation::new();
        parent.cancel();
        assert!(matches!(
            HookStart::Exhausted.into_run(event, &parent),
            Admission::Exhausted
        ));
    }
}
