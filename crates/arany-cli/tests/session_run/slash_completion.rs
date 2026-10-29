use super::{
    active_terminal::{self, ProductGuard, assert_tty_restored, product_child_of, tty_settings},
    loopback, saved_ui_preferences,
    session_picker::{screen_contains, tail},
    ui_preferences,
};
use crate::pty::Pty;
use arany::{
    CollaborationPolicy, RecordKind, SessionDefaults, SessionId, StateRoot, Store, list_sessions,
};
use rustix::process::{Signal, kill_process};
use std::{
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[path = "slash_completion/completion.rs"]
mod completion;
#[path = "slash_completion/paste.rs"]
mod paste;
#[path = "slash_completion/pickers.rs"]
mod pickers;

// A selector takes keys only once its armed frame shows `screen_ready`.
#[track_caller]
pub(super) fn wait_for_frame(
    pty: &mut Pty,
    (from, columns, rows): (usize, usize, usize),
    screen_ready: impl Fn(&[String]) -> bool,
) {
    pty.pump_until("selector frame missing", |transcript| {
        transcript[from..]
            .windows(6)
            .rposition(|part| part == b"\x1b[?25l")
            .is_some_and(|end| {
                screen_ready(&crate::session_picker::screen_rows(
                    &transcript[..from + end + 6],
                    columns,
                    rows,
                ))
            })
    });
}

#[track_caller]
fn wait_armed(pty: &mut Pty, from: usize) {
    pty.pump_until("armed selector missing", |transcript| {
        crate::armed_selector(transcript, from, 40, 24).is_some()
    });
}

#[track_caller]
fn wait_for_caret(pty: &mut Pty, from: usize, column: u16) {
    let suffix = format!(";{column}H");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frames = &pty.transcript[from..];
        if frames
            .windows(b"\x1b[?25h\x1b[".len())
            .enumerate()
            .filter(|(_, part)| *part == b"\x1b[?25h\x1b[")
            .any(|(start, part)| {
                let rest = &frames[start + part.len()..];
                let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
                digits > 0 && rest[digits..].starts_with(suffix.as_bytes())
            })
        {
            return;
        }
        pty.pump();
        assert!(
            Instant::now() < deadline,
            "caret column {column} missing: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
}

pub(super) fn saved_conversation(
    state: &std::path::Path,
    workspace: &std::path::Path,
    defaults: SessionDefaults,
) -> SessionId {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("fixture runtime")
        .block_on(async {
            let id = arany::create_session(
                StateRoot::admit(state).expect("State"),
                workspace.to_owned(),
                None,
            )
            .await
            .expect("explicit existing Session");
            if defaults != SessionDefaults::default() {
                arany::set_session_defaults(
                    StateRoot::open_existing(state).expect("State"),
                    workspace.to_owned(),
                    id,
                    defaults,
                )
                .await
                .expect("existing defaults");
            }
            id
        })
}
