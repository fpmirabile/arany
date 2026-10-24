use super::Admission;
use crate::terminal::Composer;
use arany::{FileSuggestionSource, RunCancellation, SuggestionIndex, ToolGrant};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const BUILD_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
const BUILD_CONCURRENCY: usize = 1;
static BUILD_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(BUILD_CONCURRENCY);

async fn build_index(
    source: FileSuggestionSource,
    folder: String,
    generation: [u8; 32],
    mut cancellation: RunCancellation,
) -> Result<SuggestionIndex, arany::ToolError> {
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(arany::ToolError::Operation),
        permit = tokio::time::timeout(BUILD_WAIT, BUILD_SLOTS.acquire()) => {
            permit.map_err(|_| arany::ToolError::QueueFull)?
                .map_err(|_| arany::ToolError::Operation)?
        }
    };
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        source.read(folder, generation, cancellation)
    })
    .await
    .map_err(|_| arany::ToolError::Operation)?
}

enum BuildState {
    Active,
    Superseded,
}

struct Build {
    state: BuildState,
    folder: String,
    authority: [u8; 32],
    generation: u64,
    cancellation: RunCancellation,
    task: JoinHandle<Result<SuggestionIndex, arany::ToolError>>,
}

impl Drop for Build {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

struct Snapshot {
    folder: String,
    authority: [u8; 32],
    expires: Instant,
    index: SuggestionIndex,
    query: Option<String>,
}

fn query_parts(query: &str) -> (&str, &str) {
    query.rsplit_once('/').unwrap_or(("", query))
}

#[derive(Default)]
pub(super) struct Files {
    source: Option<FileSuggestionSource>,
    configured: Option<bool>,
    trusted: Option<bool>,
    build: Option<Build>,
    snapshot: Option<Snapshot>,
    generation: u64,
    attempted: Option<(String, [u8; 32], String)>,
    query: Option<String>,
    reported: Option<(Option<String>, String)>,
}

impl Files {
    pub(super) async fn refresh(
        &mut self,
        admission: &Admission,
        composer: &mut Composer,
    ) -> Option<String> {
        let query = composer.file_query();
        if query.is_none() {
            self.clear();
            composer.set_file_candidates(None, Vec::new());
            return None;
        }
        let changed = composer.file_query_changed();
        self.query = query.clone();
        let result = self
            .update_source(admission)
            .await
            .and_then(|(source, authority)| {
                let query = query.as_deref().ok_or(arany::ToolError::Operation)?;
                let (folder, prefix) = query_parts(query);
                self.refresh_index(source, folder, query, authority)?;
                if let Some(snapshot) = &mut self.snapshot
                    && (changed || snapshot.query.as_deref() != Some(query))
                {
                    snapshot.query = Some(query.into());
                    let items = snapshot.index.refine(folder, prefix)?;
                    composer.set_file_candidates(Some(query.into()), items);
                    self.reported = None;
                }
                Ok(())
            });
        if let Err(error) = result {
            if !matches!(error, arany::ToolError::Timeout(_)) {
                self.invalidate();
            }
            composer.set_file_candidates(query, Vec::new());
            return self.report_unavailable(error.to_string());
        }
        if self.snapshot.is_none() {
            composer.set_file_candidates(query, Vec::new());
        }
        None
    }

    async fn update_source(
        &mut self,
        admission: &Admission,
    ) -> Result<(FileSuggestionSource, [u8; 32]), arany::ToolError> {
        let trusted = admission
            .workspace_permissions
            .as_ref()
            .is_some_and(arany::WorkspacePermissions::is_trusted);
        let source = if self.configured == Some(admission.tools) && self.trusted == Some(trusted) {
            self.source.clone()
        } else {
            self.invalidate();
            None
        };
        let grant = if admission.tools {
            ToolGrant::Configured
        } else {
            admission
                .workspace_permissions
                .as_ref()
                .filter(|access| access.is_trusted())
                .cloned()
                .map_or(ToolGrant::None, ToolGrant::Trusted)
        };
        let state_dir = admission.state_dir.clone();
        let workspace = admission.workspace.clone();
        let (source, authority) = tokio::task::spawn_blocking(move || {
            let source = match source {
                Some(source) => source,
                None => FileSuggestionSource::new(
                    arany::StateRoot::open_existing(&state_dir)
                        .map_err(|_| arany::ToolError::Configuration)?,
                    &workspace,
                    grant,
                )?,
            };
            let authority = source.generation()?;
            Ok::<_, arany::ToolError>((source, authority))
        })
        .await
        .map_err(|_| arany::ToolError::Operation)??;
        self.source = Some(source.clone());
        self.configured = Some(admission.tools);
        self.trusted = Some(trusted);
        Ok((source, authority))
    }

    fn report_unavailable(&mut self, error: String) -> Option<String> {
        let report = (self.query.clone(), error);
        if self.reported.as_ref() == Some(&report) {
            return None;
        }
        let notice = format!("File suggestions unavailable: {}", report.1);
        self.reported = Some(report);
        Some(notice)
    }

    fn refresh_index(
        &mut self,
        source: FileSuggestionSource,
        folder: &str,
        query: &str,
        authority: [u8; 32],
    ) -> Result<(), arany::ToolError> {
        if self.snapshot.as_ref().is_some_and(|s| {
            s.folder != folder || s.authority != authority || Instant::now() >= s.expires
        }) {
            self.invalidate();
        }
        if self
            .build
            .as_ref()
            .is_some_and(|b| b.folder != folder || b.authority != authority)
        {
            self.invalidate();
        }
        if self.snapshot.is_some() || self.build.is_some() {
            return Ok(());
        }
        let attempt = (folder.to_owned(), authority, query.to_owned());
        if self.attempted.as_ref() == Some(&attempt) {
            return Ok(());
        }
        self.attempted = Some(attempt);
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(arany::ToolError::Limit)?;
        let cancellation = RunCancellation::new();
        let task = tokio::spawn(build_index(
            source,
            folder.into(),
            authority,
            cancellation.clone(),
        ));
        self.build = Some(Build {
            state: BuildState::Active,
            folder: folder.into(),
            authority,
            generation: self.generation,
            cancellation,
            task,
        });
        Ok(())
    }

    fn invalidate(&mut self) {
        self.snapshot = None;
        self.attempted = None;
        if let Some(build) = &mut self.build {
            build.state = BuildState::Superseded;
            build.cancellation.cancel();
        }
    }

    pub(super) fn clear(&mut self) {
        self.invalidate();
        self.query = None;
        self.reported = None;
    }

    pub(super) async fn next_update(&mut self) -> Option<String> {
        let expiry = self.snapshot.as_ref().map(|snapshot| snapshot.expires);
        let result = tokio::select! {
            result = async {
                match &mut self.build {
                    Some(build) => (&mut build.task).await,
                    None => std::future::pending().await,
                }
            } => Some(result),
            () = async {
                match expiry {
                    Some(expiry) => tokio::time::sleep_until(expiry).await,
                    None => std::future::pending().await,
                }
            } => None,
        };
        let Some(result) = result else {
            self.invalidate();
            return None;
        };
        let build = self.build.take().expect("completed build is present");
        if self.query.is_none() || build.generation != self.generation {
            return None;
        }
        match build.state {
            BuildState::Active => {}
            BuildState::Superseded => return None,
        }
        match result {
            Ok(Ok(index)) => {
                self.reported = None;
                self.snapshot = Some(Snapshot {
                    folder: build.folder.clone(),
                    authority: build.authority,
                    expires: Instant::now() + arany::SUGGESTION_LIFETIME,
                    index,
                    query: None,
                });
                None
            }
            Ok(Err(error)) => self.report_unavailable(error.to_string()),
            Err(_) => self.report_unavailable("discovery stopped".into()),
        }
    }

    pub(super) async fn close(&mut self) {
        self.clear();
        if let Some(mut build) = self.build.take() {
            let _ = (&mut build.task).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gated_build(
        result: Result<SuggestionIndex, arany::ToolError>,
    ) -> (Files, tokio::sync::oneshot::Sender<()>) {
        let (release, gate) = tokio::sync::oneshot::channel();
        let build = Build {
            state: BuildState::Active,
            folder: String::new(),
            authority: [1; 32],
            generation: 1,
            cancellation: RunCancellation::new(),
            task: tokio::spawn(async move {
                let _ = gate.await;
                result
            }),
        };
        (
            Files {
                build: Some(build),
                generation: 1,
                query: Some("n".into()),
                ..Files::default()
            },
            release,
        )
    }

    #[tokio::test]
    async fn repeated_admission_errors_are_reported_once_per_query_and_popup() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let admission = Admission {
            telemetry: arany::Telemetry::disabled(),
            state_dir: fixture.path().join("missing-state"),
            explicit_state_dir: false,
            roster: None,
            workspace,
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: arany::SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: super::super::EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let mut files = Files::default();
        let mut composer = Composer::default();
        composer.insert_literal("@src").unwrap();
        assert!(
            files
                .refresh(&admission, &mut composer)
                .await
                .unwrap()
                .contains("unavailable")
        );
        for _ in 0..3 {
            assert!(files.refresh(&admission, &mut composer).await.is_none());
        }
        composer.insert_literal("m").unwrap();
        assert!(files.refresh(&admission, &mut composer).await.is_some());
        assert!(files.refresh(&admission, &mut composer).await.is_none());
        composer.take();
        assert!(files.refresh(&admission, &mut composer).await.is_none());
        composer.insert_literal("@srcm").unwrap();
        assert!(files.refresh(&admission, &mut composer).await.is_some());
        files.close().await;
    }

    #[tokio::test]
    async fn local_suggestions_filter_names_without_creating_events() {
        use std::os::unix::fs::PermissionsExt;

        arany::test_support::isolate_account_root();
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        for name in [
            "visible-record.txt",
            "private-record.txt",
            "ignored-record.cache",
        ] {
            std::fs::write(workspace.join(name), b"payload must not enter the index").unwrap();
        }
        std::fs::write(workspace.join(".gitignore"), b"*.cache\n").unwrap();
        let state_dir = fixture.path().join("state");
        let root = arany::StateRoot::admit(&state_dir).unwrap();
        let settings = state_dir.join("permissions.json");
        std::fs::write(
            &settings,
            br#"{"permissions":{"deny":["Read(./private-record.txt)"]}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        arany::Store::open(root).unwrap().close().await.unwrap();
        let source = FileSuggestionSource::new(
            arany::StateRoot::open_existing(&state_dir).unwrap(),
            &workspace,
            ToolGrant::None,
        )
        .unwrap();
        let generation = source.generation().unwrap();
        let index = build_index(
            source.clone(),
            String::new(),
            generation,
            RunCancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(index.refine("", "record").unwrap(), ["visible-record.txt"]);
        let cancellation = RunCancellation::new();
        cancellation.cancel();
        assert!(matches!(
            build_index(source.clone(), String::new(), generation, cancellation).await,
            Err(arany::ToolError::Operation)
        ));
        std::fs::write(&settings, br#"{"permissions":{"deny":["Read"]}}"#).unwrap();
        let generation = source.generation().unwrap();
        assert!(matches!(
            build_index(source, String::new(), generation, RunCancellation::new()).await,
            Err(arany::ToolError::FilesystemDenied)
        ));
        let connection = rusqlite::Connection::open_with_flags(
            state_dir.join("events.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let events: i64 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_indices_clear_candidates_before_rebuilding() {
        let index = serde_json::from_value(serde_json::json!({"names":["visible.txt"]})).unwrap();
        let start = Instant::now();
        let mut files = Files {
            snapshot: Some(Snapshot {
                folder: String::new(),
                authority: [1; 32],
                expires: start + arany::SUGGESTION_LIFETIME,
                index,
                query: None,
            }),
            attempted: Some((String::new(), [1; 32], "visible".into())),
            query: Some("visible".into()),
            ..Files::default()
        };
        assert!(files.next_update().await.is_none());
        assert_eq!(Instant::now() - start, arany::SUGGESTION_LIFETIME);
        assert!(files.snapshot.is_none() && files.attempted.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn folder_suffix_reuses_one_queued_build_in_an_oversized_workspace() {
        const LARGE_WORKSPACE_FILES: usize = 3000;
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        for index in 0..LARGE_WORKSPACE_FILES {
            std::fs::write(workspace.join(format!("record-{index}.txt")), "").unwrap();
        }
        std::fs::write(workspace.join("src/main.rs"), "").unwrap();
        arany::test_support::isolate_account_root();
        let source = FileSuggestionSource::new(
            arany::StateRoot::admit(&fixture.path().join("state")).unwrap(),
            &workspace,
            ToolGrant::None,
        )
        .unwrap();
        let authority = source.generation().unwrap();
        let index = serde_json::from_value(serde_json::json!({"names":["src/main.rs"]})).unwrap();
        let (mut files, release) = gated_build(Ok(index));
        let build = files.build.as_mut().unwrap();
        build.folder = "src".into();
        build.authority = authority;
        for query in ["src/", "src/m"] {
            let (folder, _) = query_parts(query);
            files
                .refresh_index(source.clone(), folder, query, authority)
                .unwrap();
            let build = files.build.as_ref().unwrap();
            assert!(matches!(build.state, BuildState::Active));
            assert_eq!(files.generation, 1);
            files.query = Some(query.into());
        }
        release.send(()).unwrap();
        assert!(files.next_update().await.is_none());
        let (folder, prefix) = query_parts("src/m");
        assert_eq!(
            files
                .snapshot
                .as_ref()
                .unwrap()
                .index
                .refine(folder, prefix)
                .unwrap(),
            ["src/main.rs"]
        );
        files.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn queued_discovery_keeps_typing_responsive_and_discards_cancelled_generations() {
        let index =
            serde_json::from_value(serde_json::json!({"names":["stale-canary.txt"]})).unwrap();
        let (mut files, release) = gated_build(Ok(index));
        for query in ["no", "not", "notes", "notes/"] {
            tokio::select! {
                biased;
                _ = files.next_update() => panic!("build must remain queued until its gate"),
                _ = std::future::ready(()) => files.query = Some(query.into()),
            }
        }
        assert_eq!(files.generation, 1);
        files.clear();
        files.query = Some("another".into());
        release.send(()).unwrap();
        assert!(files.next_update().await.is_none());
        assert!(files.snapshot.is_none() && files.build.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn busy_suggestions_are_reported_without_a_background_retry() {
        let (mut files, release) = gated_build(Err(arany::ToolError::QueueFull));
        files.attempted = Some((String::new(), [1; 32], "n".into()));
        release.send(()).unwrap();
        let notice = files.next_update().await.unwrap();
        assert!(notice.contains("unavailable") && notice.contains("queue"));
        assert!(files.build.is_none() && files.snapshot.is_none());
        assert!(files.attempted.is_some());
        files.close().await;
    }
}
