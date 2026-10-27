use super::super::GitStage;
use super::unit::{Unit, run_unit};
use super::{PREPARATION_DEADLINE, TEARDOWN_WAIT};
use crate::tools::{Cancellation, EffectIntent, ToolError, ToolObservation, ToolRuntime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};

// The controller's acknowledgement that it relayed the frame; the executor helper stays alive until it arrives.
pub(super) const RELAYED: &[u8; 8] = b"RELAYED\n";
// The helper may wait while the publisher unit bootstraps, then for teardown.
const RELAY_ACK_WAIT: Duration = PREPARATION_DEADLINE.saturating_add(TEARDOWN_WAIT);

#[derive(Default)]
pub(super) struct GitLink {
    pub(super) stage: Option<GitStage>,
    frames_out: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    frames_in: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    go_out: Option<tokio::sync::oneshot::Sender<()>>,
    go_in: Option<tokio::sync::oneshot::Receiver<()>>,
    handed_off: Arc<AtomicBool>,
}

impl GitLink {
    pub(super) async fn feed_publisher(
        &mut self,
        stream: &mut tokio::net::UnixStream,
    ) -> Result<(), ToolError> {
        let mut segments = self.frames_in.take().ok_or(ToolError::Configuration)?;
        while let Some(segment) = segments.recv().await {
            stream
                .write_all(&segment)
                .await
                .map_err(|_| ToolError::Operation)?;
        }
        self.go_in
            .take()
            .ok_or(ToolError::Configuration)?
            .await
            .map_err(|_| ToolError::Operation)?;
        Ok(())
    }

    // After the executor unit stopped, its coalition must be empty before the publisher may take any lock.
    pub(super) fn release_publisher(&mut self, unit: &mut Unit) -> Result<(), ToolError> {
        if !self.handed_off.load(Ordering::Acquire) {
            return Ok(());
        }
        if !unit
            .scope
            .as_mut()
            .ok_or(ToolError::ProtectionUnavailable)?
            .is_empty()?
        {
            return Err(ToolError::GuardRejected(
                super::super::GuardStage::ProcessCleanup,
            ));
        }
        if let Some(go) = self.go_out.take() {
            let _ = go.send(());
        }
        Ok(())
    }
}

// The controller relays the executor's frame to the publisher one chunk at a time, after checking only the helper remains.
pub(super) async fn relay_plan(
    stream: &mut BufReader<tokio::net::UnixStream>,
    intent: &EffectIntent,
    unit: &mut Unit,
    link: &mut GitLink,
) -> Result<ToolObservation, ToolError> {
    let (observation, framed) = super::super::read_plan_report(stream, intent).await?;
    if !framed {
        return Ok(observation);
    }
    if observation.disposition != crate::tools::ToolDisposition::Succeeded {
        return Err(ToolError::Operation);
    }
    let helper = unit.supervisor()?;
    if !unit
        .scope
        .as_mut()
        .ok_or(ToolError::ProtectionUnavailable)?
        .holds_only(helper)?
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let segments = link.frames_out.take().ok_or(ToolError::Configuration)?;
    segments
        .send(crate::tools::guard::transfer::encode_frame(
            observation.output.as_bytes(),
            intent.limits.result_bytes as usize,
        )?)
        .await
        .map_err(|_| ToolError::Operation)?;
    crate::tools::guard::transfer::relay_frame(
        stream,
        crate::tools::git::WRITE_FRAME_BYTES,
        &segments,
    )
    .await?;
    stream
        .get_mut()
        .write_all(RELAYED)
        .await
        .map_err(|_| ToolError::Operation)?;
    if !super::super::read_capped(&mut *stream, 1).await?.is_empty() {
        return Err(ToolError::Operation);
    }
    link.handed_off.store(true, Ordering::Release);
    Ok(observation)
}

pub(super) async fn receive_plan(
    input: &mut tokio::net::unix::OwnedReadHalf,
    result_bytes: usize,
) -> Result<(Vec<u8>, Vec<u8>), ToolError> {
    let result = crate::tools::guard::transfer::receive_frame_capped(input, result_bytes).await?;
    let frame = crate::tools::guard::transfer::receive_frame_capped(
        input,
        crate::tools::git::WRITE_FRAME_BYTES,
    )
    .await?;
    Ok((result, frame))
}

pub(super) async fn send_plan(
    output: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &[u8],
    relayed: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), ToolError> {
    crate::tools::guard::transfer::send_frame_capped(
        output,
        frame,
        crate::tools::git::WRITE_FRAME_BYTES,
    )
    .await?;
    tokio::time::timeout(RELAY_ACK_WAIT, relayed)
        .await
        .map_err(|_| ToolError::Limit)?
        .map_err(|_| ToolError::Operation)
}

pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: Cancellation,
    dispatched: &AtomicBool,
) -> Result<ToolObservation, ToolError> {
    let ((executor, publisher), window) = admit_pair(runtime, intent, &mut cancellation).await?;
    let (segments, frames) = tokio::sync::mpsc::channel(1);
    let (go_out, go_in) = tokio::sync::oneshot::channel();
    let handed_off = Arc::new(AtomicBool::new(false));
    let mut plan_link = GitLink {
        stage: Some(GitStage::Plan),
        frames_out: Some(segments),
        go_out: Some(go_out),
        handed_off: handed_off.clone(),
        ..GitLink::default()
    };
    let mut publish_link = GitLink {
        stage: Some(GitStage::Publish),
        frames_in: Some(frames),
        go_in: Some(go_in),
        ..GitLink::default()
    };
    let planned = AtomicBool::new(false);
    let plan = run_unit(
        runtime,
        intent,
        cancellation.clone(),
        &planned,
        (executor, window),
        &mut plan_link,
    );
    let publish = run_unit(
        runtime,
        intent,
        cancellation,
        dispatched,
        (publisher, window),
        &mut publish_link,
    );
    tokio::pin!(plan, publish);
    let planned = tokio::select! {
        result = &mut plan => result,
        result = &mut publish => {
            // The publisher cannot finish before GO, so any early result is a failure.
            return result.and(Err(ToolError::Operation));
        }
    };
    match planned {
        Ok(observation) if !handed_off.load(Ordering::Acquire) => Ok(observation),
        Ok(_) => publish.await,
        Err(error) => Err(error),
    }
}

async fn admit_pair(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    cancellation: &mut Cancellation,
) -> Result<((Unit, Unit), Option<crate::tools::ActiveToolWindow>), ToolError> {
    let ids = [intent.id, uuid::Uuid::now_v7()];
    let reserved = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ToolError::Operation),
        permits = super::super::admission::acquire_all(
            &ids,
            intent.limits.runtime_ms,
            |queued| runtime.queue_changed(intent.id, queued),
        ) => permits?,
    };
    let [publisher, executor] = reserved;
    let window = runtime
        .begin_active_action(tokio::time::Instant::now())
        .await?;
    let parent = std::path::Path::new(super::unit::UNIT_ROOT_PARENT);
    let mut executor = Unit::create(executor, parent)?;
    let mut publisher = Unit::create(publisher, parent)?;
    executor.limits = Some(intent.limits.clone());
    publisher.limits = Some(intent.limits.clone());
    Ok(((executor, publisher), window))
}
