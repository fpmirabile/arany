use super::*;

const CAP: usize = 4 * WORKER_CHUNK_BYTES;

async fn relay(frame: &[u8], cap: usize) -> (Result<(), ToolError>, Vec<Vec<u8>>) {
    let (segments, mut received) = tokio::sync::mpsc::channel(1);
    let collect = async {
        let mut all = Vec::new();
        while let Some(segment) = received.recv().await {
            all.push(segment);
        }
        all
    };
    let mut reader = frame;
    let forward = async {
        let result = relay_frame(&mut reader, cap, &segments).await;
        drop(segments);
        result
    };
    let (result, all) = tokio::join!(forward, collect);
    (result, all)
}

#[tokio::test]
async fn a_relayed_frame_reassembles_chunk_by_chunk_and_reads_back_whole() {
    let payload: Vec<u8> = (0..2 * WORKER_CHUNK_BYTES + 17).map(|n| n as u8).collect();
    let frame = encode_frame(&payload, CAP).unwrap();
    let (result, segments) = relay(&frame, CAP).await;
    result.unwrap();
    assert_eq!(segments.len(), 1 + 3 + 1);
    assert!(
        segments[1..4]
            .iter()
            .all(|segment| segment.len() <= 4 + WORKER_CHUNK_BYTES)
    );
    assert_eq!(segments.concat(), frame);
    assert_eq!(
        read_frame(&mut segments.concat().as_slice(), CAP).unwrap(),
        payload
    );
}

#[tokio::test]
async fn a_hash_mismatch_never_forwards_the_end_marker() {
    let payload = vec![7_u8; WORKER_CHUNK_BYTES + 5];
    let mut frame = encode_frame(&payload, CAP).unwrap();
    let last_byte = frame.len() - 5;
    frame[last_byte] ^= 1;
    let (result, segments) = relay(&frame, CAP).await;
    assert!(matches!(result, Err(ToolError::ChangedInput)));
    assert!(
        segments
            .last()
            .is_none_or(|segment| segment.as_slice() != 0u32.to_le_bytes())
    );
    assert!(read_frame(&mut segments.concat().as_slice(), CAP).is_err());
}

#[tokio::test]
async fn oversized_truncated_and_misshapen_frames_stop_the_relay() {
    let payload = vec![1_u8; WORKER_CHUNK_BYTES + 5];
    let frame = encode_frame(&payload, CAP).unwrap();
    let (over, _) = relay(&frame, payload.len() - 1).await;
    assert!(matches!(over, Err(ToolError::Limit)));
    let (truncated, _) = relay(&frame[..frame.len() - 9], CAP).await;
    assert!(matches!(truncated, Err(ToolError::Operation)));
    let mut misshapen = frame.clone();
    misshapen[36..40].copy_from_slice(&1u32.to_le_bytes());
    let (misshapen, _) = relay(&misshapen, CAP).await;
    assert!(matches!(misshapen, Err(ToolError::Configuration)));
}

#[tokio::test]
async fn the_relay_holds_at_most_one_segment_in_flight() {
    let payload = vec![3_u8; 3 * WORKER_CHUNK_BYTES];
    let frame = encode_frame(&payload, CAP).unwrap();
    let (segments, mut received) = tokio::sync::mpsc::channel(1);
    let mut reader = frame.as_slice();
    let mut relayed = std::pin::pin!(relay_frame(&mut reader, CAP, &segments));
    assert!(
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(relayed.as_mut().poll(cx).is_pending())
        })
        .await
    );
    assert_eq!(segments.capacity(), 0);
    let _ = received.recv().await;
}
