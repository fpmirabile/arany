use super::*;

pub(super) fn assert_route_refusals(root: &StateRoot, expected_index: &[u8]) {
    for args in [
        &["provider", "models", "chatgpt"][..],
        &[
            "provider",
            "check",
            "chatgpt",
            "gpt-6.1-sol",
            "--effort",
            "medium",
            "--accept-cost",
        ][..],
        &[
            "exec",
            "--provider",
            "chatgpt",
            "--model",
            "gpt-6.1-sol",
            "--effort",
            "low",
            "--state-dir",
            "/root/state-exec",
            "--workspace",
            "/root/workspace",
            "must refuse",
        ][..],
    ] {
        let result = Command::new("/arany")
            .args(args)
            .env_clear()
            .env("HOME", "/root")
            .env("SSL_CERT_FILE", "/fixture/ca.pem")
            .current_dir("/root/workspace")
            .bounded_output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert!(
            result.stderr
                == b"error: ChatGPT backend route not admitted; update Arany before continuing\n"
        );
        let actual_index = root
            .record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            .unwrap();
        assert!(actual_index == expected_index);
    }
}
