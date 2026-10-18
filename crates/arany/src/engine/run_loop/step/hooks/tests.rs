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
