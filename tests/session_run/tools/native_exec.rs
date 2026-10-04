use super::*;
use crate::{loopback::ChildGuard, process::BoundedOutput};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    time::Instant,
};

fn request(pipe: &mut impl Read) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "native Tool request deadline");
        let mut chunk = [0; 4096];
        match pipe.read(&mut chunk) {
            Ok(0) => panic!("native Tool peer ended before the expected request"),
            Ok(count) => {
                assert!(bytes.len() + count <= 64 * 1024);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|size| size.parse::<usize>().ok())
                        })
                        .expect("native request Content-Length");
                    if bytes.len() == end + 4 + length {
                        return bytes;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::yield_now()
            }
            Err(error) => panic!("native Tool peer read: {error}"),
        }
    }
}

#[test]
#[ignore = "native Linux shipped exec --tools, both native adapters, local TLS and actual Guard"]
fn shipped_exec_continues_after_guarded_command_and_replays_closed_events() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let shim = root.join("loopback.so");
    assert!(
        Command::new("/usr/bin/gcc")
            .args(["-Wall", "-Wextra", "-Werror", "-shared", "-fPIC"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-tls-loopback.c"))
            .args(["-ldl", "-o"])
            .arg(&shim)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    let cert = root.join("server.pem");
    let key = root.join("server.key");
    let ca = root.join("ca.pem");
    let ca_key = root.join("ca.key");
    let csr = root.join("server.csr");
    assert!(
        Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=arany-native-tool-fixture",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-keyout"
            ])
            .arg(&ca_key)
            .arg("-out")
            .arg(&ca)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-subj",
                "/CN=api.openai.com",
                "-addext",
                "subjectAltName=DNS:api.openai.com,DNS:api.anthropic.com",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-keyout"
            ])
            .arg(&key)
            .arg("-out")
            .arg(&csr)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("/usr/bin/openssl")
            .args(["x509", "-req", "-in"])
            .arg(&csr)
            .arg("-CA")
            .arg(&ca)
            .arg("-CAkey")
            .arg(&ca_key)
            .args([
                "-CAcreateserial",
                "-days",
                "1",
                "-copy_extensions",
                "copy",
                "-out"
            ])
            .arg(&cert)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    for profile in ["openai", "anthropic"] {
        let workspace = root.join(format!("workspace-{profile}"));
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        private_file(&workspace.join("src/check.sh"), b"set -eu\ntest -z \"${LD_PRELOAD-}\"\ntest -z \"${OPENAI_API_KEY-}\"\ntest -z \"${ANTHROPIC_API_KEY-}\"\nprintf 'native command passed'\n");
        let state = root.join(format!("state-{profile}"));
        StateRoot::admit(&state).unwrap();
        basic_config(&state);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut server = ChildGuard::new(
            Command::new("/usr/bin/openssl")
                .args(["s_server", "-quiet", "-accept"])
                .arg(format!("127.0.0.1:{port}"))
                .arg("-cert")
                .arg(&cert)
                .arg("-key")
                .arg(&key)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "native TLS fixture start deadline"
            );
            std::thread::yield_now();
        }
        let mut output = server.child().stdout.take().unwrap();
        let flags = rustix::fs::fcntl_getfl(&output).unwrap();
        rustix::fs::fcntl_setfl(&output, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
        let mut input = server.child().stdin.take().unwrap();
        let responder = std::thread::spawn(move || {
            let model = if profile == "openai" {
                "gpt-5.4"
            } else {
                "claude-sonnet-5"
            };
            let mut contexts = Vec::new();
            for step in 0..2 {
                let bytes = request(&mut output);
                let end = bytes
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let header = std::str::from_utf8(&bytes[..end]).unwrap();
                assert!(header.starts_with(if profile == "openai" {
                    "POST /v1/responses HTTP/1.1\r\n"
                } else {
                    "POST /v1/messages HTTP/1.1\r\n"
                }));
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("authorization: bearer synthetic-native-key\r\n")
                );
                let body: Value = serde_json::from_slice(&bytes[end..]).unwrap();
                assert_eq!(body["model"], model);
                let context: Value = serde_json::from_str(if profile == "openai" {
                    body["input"].as_str().unwrap()
                } else {
                    body["messages"][0]["content"].as_str().unwrap()
                })
                .unwrap();
                assert_eq!(context["phase"], "root_plan");
                assert_eq!(context["collaboration"], json!({"mode":"single"}));
                assert_eq!(context["objective"], "Native Tool CLI");
                assert_eq!(context["history"], json!([]));
                assert_eq!(context["includes"], json!([]));
                assert!(context["workspace_guidance"].is_null());
                let observations = context["tools"]["observations"].as_array().unwrap();
                assert_eq!(observations.len(), step);
                if step == 1 {
                    let command: Value =
                        serde_json::from_str(observations[0]["output"].as_str().unwrap()).unwrap();
                    assert_eq!(
                        command,
                        json!({"exit_code":0,"stdout":"native command passed","stderr":"","workspace_changes":"discarded"})
                    );
                }
                contexts.push(context);
                let outcome = if step == 0 {
                    json!({"outcome":{"type":"tool","call":{"operation":"command","program":"bash","args":["src/check.sh"],"cwd":""}}})
                } else {
                    json!({"outcome":{"type":"finish","summary":"command complete","result":"Native command passed."}})
                };
                let response = if profile == "openai" {
                    json!({"id":format!("resp_native_{step}"),"status":"completed","model":model,"output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":outcome.to_string()}]}],"usage":{"input_tokens":12,"output_tokens":8}})
                } else {
                    json!({"id":format!("msg_native_{step}"),"type":"message","role":"assistant","model":model,"stop_reason":"end_turn","stop_sequence":null,"content":[{"type":"text","text":outcome.to_string()}],"usage":{"input_tokens":12,"output_tokens":8}})
                }.to_string();
                write!(input, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                input.flush().unwrap();
            }
            contexts
        });
        let output = Command::new(env!("CARGO_BIN_EXE_arany"))
            .args([
                "exec",
                "--tools",
                "--provider",
                profile,
                "--model",
                if profile == "openai" {
                    "gpt-5.4"
                } else {
                    "claude-sonnet-5"
                },
                "--effort",
                "low",
                "--collaboration",
                "single",
                "--output",
                "jsonl",
            ])
            .arg("--state-dir")
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .arg("Native Tool CLI")
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LD_PRELOAD", &shim)
            .env("ARANY_TEST_TLS_PORT", port.to_string())
            .env("SSL_CERT_FILE", &ca)
            .env(
                if profile == "openai" {
                    "OPENAI_API_KEY"
                } else {
                    "ANTHROPIC_API_KEY"
                },
                "synthetic-native-key",
            )
            .bounded_output()
            .unwrap();
        server.child().kill().unwrap();
        server.child().wait().unwrap();
        assert!(
            output.status.success(),
            "native exec failed: stderr={} events={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(4096)
                .flat_map(char::escape_default)
                .collect::<String>()
        );
        let contexts = responder.join().expect("strict native request semantics");
        assert!(output.stderr.is_empty());
        let lines: Vec<Value> = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let session_id = serde_json::from_value(lines[0]["session_id"].clone()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(session_id).await.unwrap();
            let run = store
                .load_view(session_id)
                .await
                .unwrap()
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(run.status, RunStatus::Finished);
            assert_eq!(
                run.assistant_message.as_deref(),
                Some("Native command passed.")
            );
            assert_eq!(run.tools.len(), 1);
            assert_eq!(
                run.tools[0].observation.as_ref().unwrap().disposition,
                ToolDisposition::Succeeded
            );
            assert_eq!(contexts.len(), 2);
            assert_eq!(run.agents[0].provider_calls.len(), 2);
            assert_eq!(
                output.stdout,
                arany::render_exec(&run, &events, arany::Output::Jsonl, true).into_bytes()
            );
            store.close().await.unwrap();
        });
    }
}
