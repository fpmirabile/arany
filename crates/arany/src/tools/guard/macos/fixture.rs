#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    ToolAdmission,
    Catalogue,
    Admission,
    Recovery,
    Connect,
    ControllerReady,
    ControllerReceipt,
    ControllerCoalition,
    ControllerBootout,
    ControllerStorage,
    SupervisorStorage,
    WorkerReady,
    WorkerGo,
    WorkerReceipt,
    WorkerReap,
    SupervisorCoalition,
    SupervisorDetach,
}

#[cfg(any(debug_assertions, test))]
impl Stage {
    fn file(self) -> &'static str {
        match self {
            Self::ToolAdmission => "tool_admission.json",
            Self::Catalogue => "catalogue.json",
            Self::Admission => "admission.json",
            Self::Recovery => "recovery.json",
            Self::Connect => "connect.json",
            Self::ControllerReady => "controller_ready.json",
            Self::ControllerReceipt => "controller_receipt.json",
            Self::ControllerCoalition => "controller_coalition.json",
            Self::ControllerBootout => "controller_bootout.json",
            Self::ControllerStorage => "controller_storage.json",
            Self::SupervisorStorage => "supervisor_storage.json",
            Self::WorkerReady => "worker_ready.json",
            Self::WorkerGo => "worker_go.json",
            Self::WorkerReceipt => "worker_receipt.json",
            Self::WorkerReap => "worker_reap.json",
            Self::SupervisorCoalition => "supervisor_coalition.json",
            Self::SupervisorDetach => "supervisor_detach.json",
        }
    }
}

pub(crate) struct Span {
    #[cfg(any(debug_assertions, test))]
    capture: Option<Capture>,
}

impl Span {
    pub(crate) fn begin(stage: Stage) -> Self {
        #[cfg(any(debug_assertions, test))]
        {
            Self::in_directory(stage, directory())
        }
        #[cfg(not(any(debug_assertions, test)))]
        {
            let _ = stage;
            Self {}
        }
    }

    #[cfg(any(debug_assertions, test))]
    fn in_directory(stage: Stage, directory: Option<crate::private_fs::PrivateDirectory>) -> Self {
        let capture = directory.and_then(|directory| {
            let capture = Capture {
                directory,
                stage,
                started: std::time::Instant::now(),
            };
            capture.write("active", None).ok()?;
            Some(capture)
        });
        Self { capture }
    }

    pub(crate) fn finish(self, completed: bool) {
        if completed {
            self.complete();
        }
    }

    pub(crate) fn complete(mut self) {
        #[cfg(any(debug_assertions, test))]
        if let Some(capture) = self.capture.take() {
            let _ = capture.finish("complete");
        }
        #[cfg(not(any(debug_assertions, test)))]
        let _ = &mut self;
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        #[cfg(any(debug_assertions, test))]
        if let Some(capture) = self.capture.take() {
            let _ = capture.finish("interrupted");
        }
    }
}

#[cfg(any(debug_assertions, test))]
pub(super) fn directory() -> Option<crate::private_fs::PrivateDirectory> {
    let account = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT")?;
    let path = std::env::var_os("ARANY_TEST_GUARD_TRACE")?;
    if std::path::Path::new(&path) != std::path::Path::new(&account).join("guard-trace") {
        return None;
    }
    crate::private_fs::open_directory(std::path::Path::new(&account), false).ok()?;
    crate::private_fs::open_directory(std::path::Path::new(&path), false).ok()
}

#[cfg(any(debug_assertions, test))]
struct Capture {
    directory: crate::private_fs::PrivateDirectory,
    stage: Stage,
    started: std::time::Instant,
}

#[cfg(any(debug_assertions, test))]
impl Capture {
    fn write(
        &self,
        state: &'static str,
        elapsed_us: Option<u64>,
    ) -> Result<(), crate::tools::ToolError> {
        let name = self.stage.file();
        for name in [name.to_owned(), format!("{name}.next")] {
            if let Some(file) =
                super::super::admission::open_file(&self.directory.dir, &name, false)?
                && file
                    .metadata()
                    .map_err(|_| crate::tools::ToolError::Path)?
                    .len()
                    > 128
            {
                return Err(crate::tools::ToolError::Limit);
            }
        }
        let bytes = serde_json::to_vec(
            &serde_json::json!({"stage":self.stage,"state":state,"elapsed_us":elapsed_us}),
        )
        .map_err(|_| crate::tools::ToolError::Operation)?;
        if bytes.len() > 128 {
            return Err(crate::tools::ToolError::Limit);
        }
        use cap_fs_ext::OpenOptionsFollowExt;
        use cap_std::fs::OpenOptionsExt;
        use std::io::Write;
        let temporary = format!("{name}.next");
        let directory = &self.directory.dir;
        if super::super::admission::open_file(directory, &temporary, false)?.is_some() {
            directory
                .remove_file(&temporary)
                .map_err(|_| crate::tools::ToolError::Operation)?;
        }
        let mut file = directory
            .open_with(
                &temporary,
                cap_std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .follow(cap_fs_ext::FollowSymlinks::No),
            )
            .map_err(|_| crate::tools::ToolError::Operation)?;
        file.write_all(&bytes)
            .map_err(|_| crate::tools::ToolError::Operation)?;
        directory
            .rename(&temporary, directory, name)
            .map_err(|_| crate::tools::ToolError::Operation)
    }

    fn finish(self, state: &'static str) -> Result<(), crate::tools::ToolError> {
        self.write(
            state,
            Some(self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_fs::open_directory;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test(start_paused = true)]
    async fn fixture_capture_survives_cancelled_observation_and_refuses_unsafe_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("trace");
        open_directory(&path, true).unwrap();
        let record = path.join("controller_receipt.json");
        let read = || {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&record).unwrap()).unwrap()
        };
        let (entered, gate) = tokio::sync::oneshot::channel();
        let observation = async {
            let _capture = Span::in_directory(
                Stage::ControllerReceipt,
                Some(open_directory(&path, false).unwrap()),
            );
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        };
        let mut observation = Box::pin(observation);
        tokio::select! {
            () = &mut observation => panic!("held observation completed"),
            result = gate => result.unwrap(),
        }
        assert_eq!(
            read(),
            serde_json::json!({"stage":"controller_receipt","state":"active","elapsed_us":null})
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), observation)
                .await
                .is_err()
        );
        let interrupted = read();
        assert_eq!(interrupted["state"], "interrupted");
        assert!(interrupted["elapsed_us"].as_u64().is_some());
        for _ in 0..64 {
            Span::in_directory(
                Stage::ControllerReceipt,
                Some(open_directory(&path, false).unwrap()),
            )
            .complete();
        }
        assert_eq!(read()["state"], "complete");
        assert!(std::fs::metadata(&record).unwrap().len() <= 128);
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);

        let canary = path.join("canary");
        std::fs::write(&canary, b"must remain unchanged").unwrap();
        for hostile in ["symlink", "hard-link", "oversized", "public"] {
            std::fs::remove_file(&record).unwrap();
            match hostile {
                "symlink" => std::os::unix::fs::symlink(&canary, &record).unwrap(),
                "hard-link" => std::fs::hard_link(&canary, &record).unwrap(),
                "oversized" => {
                    std::fs::write(&record, [0; 129]).unwrap();
                    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                "public" => {
                    std::fs::write(&record, b"public").unwrap();
                    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::symlink_metadata(&record).unwrap();
            Span::in_directory(
                Stage::ControllerReceipt,
                Some(open_directory(&path, false).unwrap()),
            )
            .complete();
            use std::os::unix::fs::MetadataExt;
            let after = std::fs::symlink_metadata(&record).unwrap();
            assert_eq!(
                (before.dev(), before.ino(), before.mode(), before.len()),
                (after.dev(), after.ino(), after.mode(), after.len()),
                "{hostile}"
            );
            assert_eq!(std::fs::read(&canary).unwrap(), b"must remain unchanged");
        }
    }
}
