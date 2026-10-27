use super::*;

#[tokio::test]
async fn handoff_rejects_non_directories_and_overflow_and_acknowledges_known_refusal() {
    use super::super::super::super::CacheCandidate;
    use tokio::io::AsyncReadExt;
    let file = tempfile::tempfile().unwrap();
    let metadata = file.metadata().unwrap();
    let descriptor: Result<Handoff, CaptureFailure> = Ok(Handoff {
        pid: std::process::id(),
        fd: file.as_raw_fd(),
        device: metadata.dev(),
        inode: metadata.ino(),
    });
    for mut record in [
        serde_json::to_vec(&descriptor).unwrap(),
        vec![b' '; HANDOFF_BYTES + 1],
    ] {
        record.push(b'\n');
        let (mut sender, receiver) = tokio::io::duplex(HANDOFF_BYTES * 2);
        sender.write_all(&record).await.unwrap();
        sender.shutdown().await.unwrap();
        let (reader, mut writer) = tokio::io::split(receiver);
        assert!(
            receive(&mut BufReader::new(reader), &mut writer)
                .await
                .is_err()
        );
    }
    let report: Result<Handoff, CaptureFailure> = Err(CaptureFailure::Limit);
    let mut record = serde_json::to_vec(&report).unwrap();
    record.push(b'\n');
    let (mut sender, receiver) = tokio::io::duplex(HANDOFF_BYTES * 2);
    sender.write_all(&record).await.unwrap();
    let (reader, mut writer) = tokio::io::split(receiver);
    let (upper, candidate) = receive(&mut BufReader::new(reader), &mut writer)
        .await
        .unwrap();
    assert!(upper.is_none());
    assert!(candidate == CacheCandidate::Rejected);
    let mut ack = [0; ACK.len()];
    sender.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack, ACK);
}
