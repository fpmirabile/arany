use arany::ImageAttachment;
use tokio::{sync::Semaphore, task::JoinHandle};

use super::sixel::{self, CellPixels};
use crate::terminal::{GraphicsPlacement, GraphicsProtocol, view::PreviewInterior};

static DECODE_SLOT: Semaphore = Semaphore::const_new(1);

type DecodeResult = (u64, Option<Vec<u8>>);

pub(in crate::terminal) fn preview_interior(
    protocol: Option<GraphicsProtocol>,
    published: Option<GraphicsPlacement>,
    ready: Option<GraphicsPlacement>,
    next: GraphicsPlacement,
) -> PreviewInterior {
    if matches!(protocol, Some(GraphicsProtocol::Sixel(_)))
        && published != Some(next)
        && ready != Some(next)
    {
        PreviewInterior::Repaint
    } else {
        PreviewInterior::Graphics
    }
}

#[derive(Default)]
pub(in crate::terminal) struct SixelPreview {
    generation: u64,
    desired: Option<PreviewRequest>,
    task: Option<JoinHandle<DecodeResult>>,
    ready: Option<Vec<u8>>,
}

struct PreviewRequest {
    placement: GraphicsPlacement,
    image: ImageAttachment,
    cell: CellPixels,
}

impl SixelPreview {
    pub(in crate::terminal) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.desired = None;
        self.ready = None;
    }

    pub(in crate::terminal) fn request(
        &mut self,
        placement: GraphicsPlacement,
        image: &ImageAttachment,
        cell: CellPixels,
    ) {
        if self
            .desired
            .as_ref()
            .is_some_and(|request| request.placement == placement)
        {
            return;
        }
        self.clear();
        self.desired = Some(PreviewRequest {
            placement,
            image: image.clone(),
            cell,
        });
        self.start();
    }

    fn start(&mut self) {
        if self.task.is_some() {
            return;
        }
        let Some(request) = &self.desired else {
            return;
        };
        let Ok(slot) = DECODE_SLOT.try_acquire() else {
            return;
        };
        let generation = self.generation;
        let image = request.image.clone();
        let inner = request.placement.inner;
        let cell = request.cell;
        self.task = Some(tokio::task::spawn_blocking(move || {
            let _slot = slot;
            (generation, sixel::image(&image, inner, cell))
        }));
    }

    pub(in crate::terminal) async fn wait(&mut self) {
        loop {
            let Some(task) = self.task.as_mut() else {
                return std::future::pending().await;
            };
            let result = task.await;
            self.task = None;
            match result {
                Ok((generation, bytes)) => {
                    if self.complete(generation, bytes) {
                        return;
                    }
                }
                Err(_) => return,
            }
            self.start();
        }
    }

    fn complete(&mut self, generation: u64, bytes: Option<Vec<u8>>) -> bool {
        if generation != self.generation || self.desired.is_none() {
            return false;
        }
        self.ready = bytes;
        true
    }

    pub(in crate::terminal) fn take_ready(&mut self) -> Option<Vec<u8>> {
        self.ready.take()
    }

    pub(in crate::terminal) fn ready_placement(&self) -> Option<GraphicsPlacement> {
        self.ready.as_ref()?;
        self.desired.as_ref().map(|request| request.placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{GraphicsProtocol, graphics_protocol};
    use ratatui::layout::Rect;

    #[test]
    fn sixel_detection_requires_cells_and_yields_to_kitty() {
        let cell = CellPixels::new(8, 16).expect("cell size");
        for (kitty, tmux, program, expected) in [
            (false, false, None, Some(GraphicsProtocol::Sixel(cell))),
            (true, false, None, Some(GraphicsProtocol::Kitty)),
            (
                false,
                false,
                Some("iTerm.app"),
                Some(GraphicsProtocol::Sixel(cell)),
            ),
            (false, true, None, None),
        ] {
            assert_eq!(
                graphics_protocol(kitty, Some(cell), tmux, program, None),
                expected
            );
        }
    }

    #[tokio::test]
    async fn late_decode_cannot_publish_after_leave_reenter_or_geometry_change() {
        let image = arany::test_support::test_image();
        let cell = CellPixels::new(1, 1).expect("cell size");
        let placement = GraphicsPlacement {
            image_index: 0,
            digest: image.digest(),
            inner: Rect::new(0, 0, 1, 1),
        };
        let mut preview = SixelPreview {
            desired: Some(PreviewRequest {
                placement,
                image: image.clone(),
                cell,
            }),
            ..SixelPreview::default()
        };
        let old = preview.generation;
        let (finish, finished) = tokio::sync::oneshot::channel();
        preview.task = Some(tokio::spawn(async move {
            finished.await.expect("release old decode");
            (old, Some(b"stale graphics".to_vec()))
        }));
        preview.clear();
        assert!(!preview.complete(old, Some(b"stale graphics".to_vec())));
        preview.request(placement, &image, cell);
        assert!(!preview.complete(old, Some(b"stale graphics".to_vec())));
        let mut moved = placement;
        moved.inner.x += 1;
        preview.request(moved, &image, cell);
        finish.send(()).expect("old decode waits");
        preview.wait().await;
        let expected = sixel::image(&image, moved.inner, cell).expect("current preview encodes");
        assert_eq!(preview.take_ready(), Some(expected));
        assert!(preview.task.is_none());
        preview.request(moved, &image, cell);
        assert!(preview.task.is_none(), "same hover does not retransmit");
        assert!(preview.take_ready().is_none());
    }
}
