use super::FaultPlan;
use crate::ToolCall;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum GitWriteStage {
    FramePending,
    BeforeGo,
    PartialGo,
    PublisherHeld,
    ExecutorTimeout,
    Published,
}

type Witness = (PathBuf, u32);
type Gate = (
    ToolCall,
    GitWriteStage,
    oneshot::Sender<Witness>,
    oneshot::Receiver<()>,
);
static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());
static MODES: Mutex<Vec<(ToolCall, GitWriteStage)>> = Mutex::new(Vec::new());
static DEATH: Mutex<Option<uuid::Uuid>> = Mutex::new(None);

impl FaultPlan {
    pub fn gate_git_write(
        call: ToolCall,
        stage: GitWriteStage,
    ) -> (oneshot::Receiver<Witness>, oneshot::Sender<()>) {
        let (entered, wait) = oneshot::channel();
        let (release, released) = oneshot::channel();
        if matches!(
            stage,
            GitWriteStage::PublisherHeld | GitWriteStage::ExecutorTimeout
        ) {
            MODES.lock().unwrap().push((call.clone(), stage));
        }
        GATES.lock().unwrap().push((call, stage, entered, released));
        (wait, release)
    }

    pub fn kill_git_publisher_after_effect(call: ToolCall) {
        MODES.lock().unwrap().push((call, GitWriteStage::Published));
    }
}

pub(crate) fn take_mode(call: &ToolCall, planning: bool) -> Option<GitWriteStage> {
    let mut modes = MODES.lock().unwrap();
    let index = modes.iter().position(|(candidate, stage)| {
        candidate == call && (*stage == GitWriteStage::ExecutorTimeout) == planning
    })?;
    Some(modes.remove(index).1)
}

pub(crate) async fn wait(call: &ToolCall, stage: GitWriteStage, witness: Witness) {
    let gate = {
        let mut gates = GATES.lock().unwrap();
        gates
            .iter()
            .position(|(candidate, point, ..)| candidate == call && *point == stage)
            .map(|index| gates.remove(index))
    };
    if let Some((_, _, entered, release)) = gate {
        let _ = entered.send(witness);
        let _ = release.await;
    }
}

pub(crate) fn arm_death(intent: uuid::Uuid) {
    *DEATH.lock().unwrap() = Some(intent);
}

pub(crate) fn published(intent: uuid::Uuid) {
    if *DEATH.lock().unwrap() == Some(intent) {
        std::process::exit(86);
    }
}

pub(crate) fn has_gate(call: &ToolCall, stage: GitWriteStage) -> bool {
    GATES
        .lock()
        .unwrap()
        .iter()
        .any(|(candidate, point, ..)| candidate == call && *point == stage)
}

pub(crate) fn storage_finished(root: &std::path::Path, intent: uuid::Uuid) -> std::io::Result<()> {
    use std::io::Write;
    let Ok(bytes) = std::fs::read(root.join(crate::tools::GIT_STORAGE_FAULT)) else {
        return Ok(());
    };
    let (fault, evidence): (super::GitStorageFault, PathBuf) =
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if fault != super::GitStorageFault::Exhaust {
        return Ok(());
    }
    if std::fs::read(root.join(crate::tools::GIT_STORAGE_CANARY))? != b"control" {
        return Err(std::io::Error::other("Git changed the control-root canary"));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(evidence)?;
    file.write_all(&serde_json::to_vec(&(intent, root)).map_err(std::io::Error::other)?)
}
