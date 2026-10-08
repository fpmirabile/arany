#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/process.rs"]
pub mod process;
use process::BoundedOutput;

use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_bounded(mut child: Server) -> Output {
    process::capture(&mut child.0, Duration::from_secs(30), 64 * 1024)
}

fn openssl() -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    let path = Path::new("/usr/bin/openssl");
    #[cfg(target_os = "macos")]
    let path = [
        "/opt/homebrew/opt/openssl@3/bin/openssl",
        "/usr/local/opt/openssl@3/bin/openssl",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.is_file())
    .expect(
        "native TLS fixture requires installed OpenSSL 3 for an explicitly loopback-bound server",
    );
    std::fs::canonicalize(path).unwrap()
}

fn certificate(dir: &Path, name: &str) {
    let configuration = dir.join(format!("{name}.cnf"));
    std::fs::write(&configuration, "[req]\ndistinguished_name=dn\nx509_extensions=ca\nreq_extensions=leaf\nprompt=no\n[dn]\nCN=arany-test-root\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n[leaf]\nsubjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n").unwrap();
    let root = Command::new(openssl())
        .env_clear()
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=arany-test-root",
        ])
        .arg("-config")
        .arg(&configuration)
        .arg("-keyout")
        .arg(dir.join(format!("{name}-ca.key")))
        .arg("-out")
        .arg(dir.join(format!("{name}-ca.pem")))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .expect("OpenSSL is required for the explicit TLS gate");
    assert!(root.status.success(), "generate test-owned root");
    let request = Command::new(openssl())
        .env_clear()
        .args([
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
        ])
        .arg("-config")
        .arg(&configuration)
        .arg("-keyout")
        .arg(dir.join(format!("{name}.key")))
        .arg("-out")
        .arg(dir.join(format!("{name}.csr")))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .expect("create test-owned certificate request");
    assert!(
        request.status.success(),
        "generate test-owned certificate request"
    );
    let signed = Command::new(openssl())
        .env_clear()
        .args(["x509", "-req", "-in"])
        .arg(dir.join(format!("{name}.csr")))
        .arg("-CA")
        .arg(dir.join(format!("{name}-ca.pem")))
        .arg("-CAkey")
        .arg(dir.join(format!("{name}-ca.key")))
        .args(["-CAcreateserial", "-days", "1"])
        .arg("-extfile")
        .arg(&configuration)
        .args(["-extensions", "leaf", "-out"])
        .arg(dir.join(format!("{name}.pem")))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .expect("sign test-owned certificate");
    assert!(signed.status.success(), "sign test-owned certificate");
}

fn server(dir: &Path, name: &str) -> (Server, u16) {
    let reservation = TcpListener::bind(("127.0.0.1", 0)).expect("reserve local port");
    let port = reservation.local_addr().expect("reserved address").port();
    drop(reservation);
    let child = Command::new(openssl())
        .env_clear()
        .args(["s_server", "-www", "-accept"])
        .arg(format!("127.0.0.1:{port}"))
        .arg("-cert")
        .arg(dir.join(format!("{name}.pem")))
        .arg("-key")
        .arg(dir.join(format!("{name}.key")))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start test-owned HTTPS peer");
    let child = Server(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "HTTPS peer did not start");
        std::thread::yield_now();
    }
    (child, port)
}

#[test]
#[ignore = "explicit native TLS chain/name gate; macOS requires OpenSSL 3 and uses a client-local test anchor"]
fn platform_tls_chain_and_name() {
    let dir = tempfile::tempdir().expect("private TLS test files");
    certificate(dir.path(), "trusted");
    certificate(dir.path(), "untrusted");
    let (_trusted_server, trusted_port) = server(dir.path(), "trusted");
    let (_wrong_name_server, wrong_name_port) = server(dir.path(), "trusted");
    let (_untrusted_server, untrusted_port) = server(dir.path(), "untrusted");
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "platform_tls_child", "--ignored", "--nocapture"])
        .env_clear()
        .env("ARANY_TLS_TEST_CHILD", "1")
        .env("ARANY_TLS_TRUSTED_PORT", trusted_port.to_string())
        .env("ARANY_TLS_WRONG_NAME_PORT", wrong_name_port.to_string())
        .env("ARANY_TLS_UNTRUSTED_PORT", untrusted_port.to_string())
        .env("SSL_CERT_FILE", dir.path().join("trusted-ca.pem"))
        .env("ARANY_TLS_TEST_CA", dir.path().join("trusted-ca.pem"))
        .env_remove("SSL_CERT_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("isolated TLS client test");
    let output = wait_bounded(Server(child));
    assert!(
        output.status.success(),
        "TLS client gate failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "running 1 test"),
        "TLS child did not run exactly one test"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "TLS child did not complete its assertions"
    );
}

#[test]
#[ignore = "helper-only: activated by platform_tls_chain_and_name"]
fn platform_tls_child() {
    assert_eq!(
        std::env::var("ARANY_TLS_TEST_CHILD").as_deref(),
        Ok("1"),
        "TLS helper requires its isolated parent"
    );
    let trusted_port: u16 = std::env::var("ARANY_TLS_TRUSTED_PORT")
        .expect("trusted peer port")
        .parse()
        .expect("numeric trusted port");
    let untrusted_port: u16 = std::env::var("ARANY_TLS_UNTRUSTED_PORT")
        .expect("untrusted peer port")
        .parse()
        .expect("numeric untrusted port");
    let wrong_name_port: u16 = std::env::var("ARANY_TLS_WRONG_NAME_PORT")
        .expect("wrong-name peer port")
        .parse()
        .expect("numeric wrong-name port");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("TLS test runtime");
    runtime.block_on(async {
        let trusted_client = client(trusted_port);
        let valid = trusted_client
            .get(format!("https://localhost:{trusted_port}/"))
            .send()
            .await;
        assert!(valid.is_ok(), "trusted chain and name: {valid:?}");
        let wrong_name = client(wrong_name_port)
            .get(format!("https://127.0.0.1:{wrong_name_port}/"))
            .send()
            .await;
        let wrong_name = format!(
            "{:?}",
            wrong_name.expect_err("wrong server name must fail TLS")
        );
        assert!(
            wrong_name.contains("InvalidCertificate") && wrong_name.contains("NotValidForName"),
            "wrong server name must fail certificate validation: {wrong_name}"
        );
        let invalid_chain = client(untrusted_port)
            .get(format!("https://localhost:{untrusted_port}/"))
            .send()
            .await;
        let invalid_chain = format!(
            "{:?}",
            invalid_chain.expect_err("untrusted chain must fail TLS")
        );
        assert!(
            invalid_chain.contains("InvalidCertificate")
                && (invalid_chain.contains("UnknownIssuer")
                    || invalid_chain.contains("BadSignature")),
            "untrusted chain must fail certificate validation: {invalid_chain}"
        );
    });
}

fn client(port: u16) -> reqwest::Client {
    let builder = reqwest::Client::builder();
    #[cfg(target_os = "macos")]
    let builder = builder.add_root_certificate(
        reqwest::Certificate::from_pem(
            &std::fs::read(
                std::env::var_os("ARANY_TLS_TEST_CA").expect("isolated native trust anchor"),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .resolve("localhost", SocketAddr::from(([127, 0, 0, 1], port)))
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .expect("default platform TLS verifier")
}
