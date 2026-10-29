use super::{
    active_terminal::{self, ProductGuard, product_child_of, tty_settings},
    loopback::{self, ChildGuard},
    saved_ui_preferences,
    session_picker::{
        checkpoint_screen, pump, screen_contains, tail, wait_for, wait_for_after, wait_for_screen,
    },
    ui_preferences,
};
use arany::{
    CollaborationPolicy, RecordKind, SessionDefaults, SessionId, StateRoot, Store, list_sessions,
};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Signal, kill_process};
use std::{
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "slash_completion/completion.rs"]
mod completion;
#[path = "slash_completion/paste.rs"]
mod paste;
#[path = "slash_completion/pickers.rs"]
mod pickers;

// Each step waits only on bytes after this point, so a fresh window keeps this long corpus under the capture bound.
fn fresh_capture(transcript: &mut Vec<u8>, answered: &mut crate::session_picker::PtyResponses) {
    transcript.clear();
    *answered = crate::session_picker::PtyResponses::default();
}

// A selector takes keys only once its armed frame shows `screen_ready`.
#[track_caller]
pub(super) fn wait_for_frame(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    (from, columns, rows): (usize, usize, usize),
    screen_ready: impl Fn(&[String]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[from..]
        .windows(6)
        .rposition(|part| part == b"\x1b[?25l")
        .is_some_and(|end| {
            screen_ready(&crate::session_picker::screen_rows(
                &transcript[..from + end + 6],
                columns,
                rows,
            ))
        })
    {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "selector frame missing: {}",
            tail(&transcript[from..])
        );
        thread::yield_now();
    }
}

#[track_caller]
fn wait_armed(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    from: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::armed_selector(transcript, from, 40, 24).is_none() {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "armed selector missing: {}",
            tail(&transcript[from..])
        );
        thread::yield_now();
    }
}

#[track_caller]
fn wait_for_caret(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    from: usize,
    column: u16,
) {
    let suffix = format!(";{column}H");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frames = &transcript[from..];
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
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "caret column {column} missing: {}",
            tail(transcript)
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
