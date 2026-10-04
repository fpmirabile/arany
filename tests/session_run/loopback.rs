#[cfg(target_os = "linux")]
use arany::StateRoot;
use std::{
    io::Read,
    process::{Child, Output},
    time::Duration,
};
#[cfg(target_os = "linux")]
use std::{
    io::Write,
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

pub(super) struct ChildGuard(Option<Child>);

impl ChildGuard {
    pub(super) fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub(super) fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("supervised child")
    }

    #[cfg(target_os = "linux")]
    pub(super) fn take(&mut self) -> Child {
        self.0.take().expect("supervised child")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) fn wait_product(child: Child) -> Output {
    let mut child = ChildGuard::new(child);
    super::process::capture(child.child(), Duration::from_secs(10), 64 * 1024)
}

#[cfg(target_os = "linux")]
pub(super) fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("request bytes");
        assert!(count > 0 && request.len() + count <= 64 * 1024);
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = end + 4;
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let body_len = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("Content-Length");
            if request.len() >= header_end + body_len {
                return request[header_end..header_end + body_len].to_vec();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn send_response(stream: &mut TcpStream, index: usize, text: serde_json::Value) {
    let response = serde_json::json!({
        "id": format!("resp_{}", index + 1),
        "status": "completed",
        "model": "model-1",
        "output": [{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text.to_string()}]}],
        "usage": {"input_tokens":20,"output_tokens":10}
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    )
    .expect("synthetic response");
}

#[cfg(target_os = "linux")]
pub(super) fn write_profile(state: &Path, port: u16) {
    StateRoot::admit(state).expect("state root");
    let profile = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "local",
            "protocol": "openai-responses",
            "endpoint": format!("http://127.0.0.1:{port}/v1/responses"),
            "model": "model-1",
            "credential_env": "ARANY_PROVIDER_LOCAL_KEY",
            "outcome_encoding": "json_schema",
            "privacy": "user_authorized",
            "max_output_tokens": 4096,
            "capability_evidence_version": 1
        }]
    });
    let path = state.join("provider-profiles.json");
    std::fs::write(&path, serde_json::to_vec(&profile).expect("profile JSON"))
        .expect("profile file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("private profile file");
}

#[cfg(target_os = "linux")]
pub(super) fn check_profile(workspace: &Path, state: &Path) {
    check_profile_with_binary(workspace, state, Path::new(env!("CARGO_BIN_EXE_arany")));
}

#[cfg(target_os = "linux")]
pub(super) fn check_profile_with_binary(workspace: &Path, state: &Path, executable: &Path) {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(workspace)
        .args(["provider", "check", "local", "--state-dir"])
        .arg(state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let result = wait_product(command.spawn().expect("provider check process"));
    assert!(result.status.success(), "data-free custom conformance");
    assert_eq!(result.stdout, b"Provider: local\nStatus: custom verified\n");
    assert_eq!(result.stderr, b"");
}
