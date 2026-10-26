use super::tests::{run, session};
use super::*;

#[test]
fn find_step_and_rebuild_agree_at_a_literal_wrap_boundary() {
    let width = 16;
    for kind in [MessageKind::User, MessageKind::Notice] {
        let text = format!("{}needle\nneedle", " ".repeat(width - row_indent(kind)));
        let mut view = session();
        let objective = if kind == MessageKind::User {
            &text
        } else {
            "objective"
        };
        view.runs.push(run(objective, "answer"));
        let mut history = History::default();
        history.sync(&view, width, 100);
        if kind == MessageKind::Notice {
            history.record_notice(super::super::TerminalNotice::History(&text));
        }
        assert!(history.search(&view, "needle"));
        let first = history.matched_row.unwrap();
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.matched && row.text.contains("needle"))
        );
        history.last_find = Some("needle".into());
        assert!(history.step_match(&view, false));
        assert_eq!(history.matched_row, Some(first + 1));
        assert!(history.step_match(&view, true));
        assert_eq!(history.matched_row, Some(first));
        history.dirty = true;
        history.sync(&view, width, 100);
        assert_eq!(history.matched_row, Some(first));
        history.sync(&view, 40, 100);
        let relocated = history.matched_row;
        assert!(history.search(&view, "needle"));
        assert_eq!(history.matched_row, relocated);
        assert!(
            history
                .visible(&view)
                .iter()
                .any(|row| row.matched && row.text.contains("needle"))
        );
    }
}
