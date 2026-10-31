use super::*;
use crate::process::BoundedOutput;
use aws_lc_rs::{
    rand::SystemRandom,
    rsa::KeySize,
    signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
};

const STAGE: &str = "ARANY_TEST_OFFLINE_CHATGPT_STAGE";
const SIGN_IN_TEST: &str = "setup::offline_https::chatgpt::release_private_file_chatgpt_sign_in_selects_a_backend_model_without_probe";
const RENEWAL_TEST: &str =
    "setup::offline_https::chatgpt::release_private_file_chatgpt_renews_before_a_backend_turn";
const ACCESS: &str = "synthetic-chatgpt-access-token";
const RENEWED_ACCESS: &str = "synthetic-renewed-chatgpt-access-token";
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const REFRESH: &str = "synthetic-chatgpt-refresh-token";
const CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ACCOUNT: &str = "synthetic-remote-account";
const UNFINISHED_HOST: &str = "2e664d00-3f9a-40c3-adeb-c6447313a871";
const SHELL: &str = "stty rows 8 cols 16; printf 'SHELL_PID:%s\nTTY_PATH:%s\n' \"$$\" \"$(tty)\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --setup --state-dir /root/state-chatgpt --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";

#[path = "chatgpt/checked_turn.rs"]
mod checked_turn;

#[cfg(debug_assertions)]
#[path = "chatgpt/inspection.rs"]
mod inspection;

fn der_value<'a>(cursor: &mut &'a [u8], tag: u8) -> &'a [u8] {
    assert_eq!(cursor[0], tag);
    let first = cursor[1];
    let (offset, length) = if first & 0x80 == 0 {
        (2, usize::from(first))
    } else {
        let count = usize::from(first & 0x7f);
        assert!((1..=2).contains(&count));
        let mut length = 0usize;
        for byte in &cursor[2..2 + count] {
            length = (length << 8) | usize::from(*byte);
        }
        (2 + count, length)
    };
    assert!(cursor.len() >= offset + length);
    let value = &cursor[offset..offset + length];
    *cursor = &cursor[offset + length..];
    value
}

fn identity_fixture(nonce: &str) -> (Vec<u8>, String) {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("synthetic identity key");
    let mut public = pair.public_key().as_ref();
    let mut sequence = der_value(&mut public, 0x30);
    let modulus = der_value(&mut sequence, 0x02)
        .strip_prefix(&[0])
        .expect("unsigned RSA modulus");
    let exponent = der_value(&mut sequence, 0x02);
    assert!(public.is_empty() && sequence.is_empty());
    let jwks = serde_json::to_vec(&serde_json::json!({"keys": [{
        "kty": "RSA", "kid": "offline-key", "use": "sig", "alg": "RS256",
        "n": URL_SAFE_NO_PAD.encode(modulus), "e": URL_SAFE_NO_PAD.encode(exponent)
    }]}))
    .expect("synthetic JWKS");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"offline-key","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "iss": "https://auth.openai.com",
            "aud": CLIENT,
            "iat": now,
            "exp": now + 3600,
            "nonce": nonce,
            "sub": "synthetic-subject",
            "https://api.openai.com/auth": {"chatgpt_account_id": ACCOUNT}
        }))
        .expect("signed claims"),
    );
    let signed = format!("{header}.{claims}");
    let mut signature = vec![0; pair.public_modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .expect("sign synthetic identity");
    let token = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature));
    (jwks, token)
}

pub(super) fn read_request(output: &mut impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let count = output.read(&mut chunk).expect("issuer request read");
        assert!(count > 0, "issuer peer closed before request");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= MAX_REQUEST_BYTES, "HTTP request cap");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).expect("issuer request headers");
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map_or(Ok(0), |value| value.trim().parse())
        .expect("issuer request content length");
    let request_end = header_end
        .checked_add(length)
        .filter(|end| *end <= MAX_REQUEST_BYTES)
        .expect("issuer body cap");
    while bytes.len() < request_end {
        let count = output.read(&mut chunk).expect("issuer request body");
        assert!(count > 0, "issuer peer closed during body");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= MAX_REQUEST_BYTES, "HTTP request cap");
    }
    bytes
}

pub(super) fn send_json(input: &mut impl Write, body: &[u8]) {
    input
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .expect("issuer response headers");
    input.write_all(body).expect("issuer response body");
}

struct IssuerReply {
    token: Vec<u8>,
    jwks: Vec<u8>,
    redirect_uri: String,
    nonce: String,
}

const MAX_PEER_CONNECTIONS: usize = 10;
const PEER_IO_SECONDS: u64 = 20;
const HTTPS_PEER: &str = r#"
import socket, ssl, sys
address, port = sys.argv[1].rsplit(':', 1)
limit, connections, deadline = map(int, sys.argv[2:])
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain('/fixture/server.pem', '/fixture/server.key')
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind((address, int(port)))
listener.listen()
listener.settimeout(deadline)
sys.stdout.buffer.write(b'READY\n')
sys.stdout.buffer.flush()
def message(stream):
    head = bytearray()
    while not head.endswith(b'\r\n\r\n'):
        byte = stream.read(1)
        if not byte:
            return None
        head.extend(byte)
        assert len(head) <= limit
    length = 0
    for line in head.split(b'\r\n')[1:]:
        name, separator, value = line.partition(b':')
        if separator and name.lower() == b'content-length':
            length = int(value.strip())
    assert 0 <= length <= limit - len(head)
    body = stream.read(length)
    assert len(body) == length
    return bytes(head) + body
for _ in range(connections):
    socket_peer, _ = listener.accept()
    socket_peer.settimeout(deadline)
    try:
        with context.wrap_socket(socket_peer, server_side=True) as peer:
            with peer.makefile('rb') as source:
                request = message(source)
                if request is None:
                    continue
                sys.stdout.buffer.write(request)
                sys.stdout.buffer.flush()
                response = message(sys.stdin.buffer)
                if response is None:
                    break
                peer.sendall(response)
    except (ConnectionError, ssl.SSLEOFError):
        socket_peer.close()
"#;

fn tls_server(address: &str) -> Child {
    let mut child = Command::new("/usr/bin/python3")
        .args([
            "-I",
            "-c",
            HTTPS_PEER,
            address,
            &MAX_REQUEST_BYTES.to_string(),
            &MAX_PEER_CONNECTIONS.to_string(),
            &PEER_IO_SECONDS.to_string(),
        ])
        .env_clear()
        .current_dir("/fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("isolated HTTPS peer");
    let mut ready = [0; b"READY\n".len()];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .expect("HTTPS peer ready");
    assert_eq!(&ready, b"READY\n");
    child
}

fn issuer(reply: Receiver<IssuerReply>, revoke: bool, exchanges: usize, renew: bool) -> Server {
    assert!((1..=2).contains(&exchanges));
    let mut child = tls_server("127.0.0.2:443");
    let mut input = child.stdin.take().expect("issuer response pipe");
    let mut output = child.stdout.take().expect("issuer request pipe");
    let responder = thread::spawn(move || {
        for _ in 0..exchanges {
            let first = read_request(&mut output);
            assert!(first.starts_with(b"POST /oauth/token HTTP/1.1\r\n"));
            let end = first
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .expect("token request headers")
                + 4;
            let fields = url::form_urlencoded::parse(&first[end..])
                .into_owned()
                .collect::<Vec<_>>();
            assert_eq!(fields.len(), 5);
            for (name, value) in [
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT),
                ("code", "synthetic-code"),
            ] {
                assert!(
                    fields
                        .iter()
                        .any(|field| field == &(name.into(), value.into())),
                    "token exchange field missing: {name}"
                );
            }
            assert!(
                fields
                    .iter()
                    .any(|(name, value)| name == "code_verifier" && !value.is_empty())
            );
            let IssuerReply {
                token,
                jwks,
                redirect_uri,
                nonce,
            } = reply
                .recv_timeout(Duration::from_secs(10))
                .expect("signed identity fixture");
            assert!(
                fields
                    .iter()
                    .any(|(name, value)| name == "redirect_uri" && value == &redirect_uri)
            );
            send_json(&mut input, &token);
            let second = read_request(&mut output);
            assert!(second.starts_with(b"GET /.well-known/jwks.json HTTP/1.1\r\n"));
            send_json(&mut input, &jwks);
            if renew {
                checked_turn::renew_once(&mut input, &mut output, &nonce);
            }
        }
        if revoke {
            let discovery = read_request(&mut output);
            assert!(discovery.starts_with(b"GET /.well-known/openid-configuration HTTP/1.1\r\n"));
            send_json(
                &mut input,
                br#"{"issuer":"https://auth.openai.com","revocation_endpoint":"https://auth.openai.com/oauth/revoke"}"#,
            );
            let revoke = read_request(&mut output);
            assert!(revoke.starts_with(b"POST /oauth/revoke HTTP/1.1\r\n"));
            let end = revoke
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .expect("revocation headers")
                + 4;
            let fields = url::form_urlencoded::parse(&revoke[end..])
                .into_owned()
                .collect::<Vec<_>>();
            assert_eq!(
                fields,
                [
                    ("token".into(), REFRESH.into()),
                    ("token_type_hint".into(), "refresh_token".into()),
                    ("client_id".into(), CLIENT.into()),
                ]
            );
            input
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("revocation confirmation");
        }
    });
    Server {
        child,
        responder: Some(responder),
    }
}

fn wait_browser_url(pty: &mut Pty, previous: Option<&url::Url>) -> url::Url {
    // A fresh responder counts only this wait's cursor queries; the journey's resumes after it.
    let journey = std::mem::take(&mut pty.answered);
    let deadline = Instant::now() + PTY_STAGE_WAIT;
    loop {
        pty.pump();
        if let Ok(file) = std::fs::File::open("/root/browser-url") {
            let mut bytes = Vec::new();
            file.take(4097)
                .read_to_end(&mut bytes)
                .expect("browser URL read");
            assert!(bytes.len() <= 4096, "browser URL cap");
            if let Ok(url) =
                url::Url::parse(std::str::from_utf8(&bytes).expect("browser URL UTF-8"))
            {
                if previous == Some(&url)
                    || !url
                        .query_pairs()
                        .any(|(name, value)| name == "code_challenge" && value.len() == 43)
                {
                    continue;
                }
                assert_eq!(
                    pty.answered.cursor_queries, 0,
                    "linear setup queried the cursor"
                );
                pty.answered = journey;
                return url;
            }
        }
        assert!(
            Instant::now() < deadline,
            "browser launcher did not receive authorization URL"
        );
        thread::yield_now();
    }
}

fn wait_for_stage(pty: &mut Pty, start: usize, needle: &[u8], signed: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::process::pty_text_matches(&pty.transcript[start..], needle)
        .next()
        .is_none()
    {
        pty.pump();
        let mut diagnostic = tail(&pty.transcript)
            .replace(ACCESS, "[redacted]")
            .replace(REFRESH, "[redacted]");
        if let Some(signed) = signed {
            diagnostic = diagnostic.replace(signed, "[redacted]");
        }
        assert!(
            Instant::now() < deadline,
            "ChatGPT setup stage missing: {diagnostic}"
        );
        thread::yield_now();
    }
}

fn advance_setup_warning_pages(pty: &mut Pty, (start, resize_confirmation): (usize, bool)) {
    let confirmation = b"empty Enter selects 1. Back\r\nInput:\r\n";
    pty.acknowledge_linear_pages((start, confirmation));
    if resize_confirmation {
        let text = String::from_utf8_lossy(&pty.transcript);
        let tty_path = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("TTY_PATH:"))
            .expect("terminal path");
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tty_path)
            .expect("open setup terminal");
        let queued_at = pty.send(b"Accept");
        wait_for_stage(pty, queued_at, b"Accept", None);
        let resized_at = pty.transcript.len();
        rustix::termios::tcsetwinsize(
            &tty,
            rustix::termios::Winsize {
                ws_row: 12,
                ws_col: 40,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .expect("resize setup terminal");
        pty.acknowledge_linear_pages((resized_at, confirmation));
        assert!(
            !String::from_utf8_lossy(&pty.transcript[resized_at..])
                .contains("Notice: Read every page before accepting"),
            "queued confirmation input reached the restarted warning"
        );
    }
}

fn callback(url: &url::Url) {
    assert_eq!(
        url.origin().ascii_serialization(),
        "https://auth.openai.com"
    );
    assert_eq!(url.path(), "/oauth/authorize");
    let fields = url.query_pairs().into_owned().collect::<Vec<_>>();
    let value = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .expect("authorization field")
    };
    assert_eq!(fields.len(), 11);
    assert_eq!(value("client_id"), CLIENT);
    assert_eq!(value("scope"), "openid profile email offline_access");
    assert_eq!(value("originator"), "arany");
    assert_eq!(value("id_token_add_organizations"), "true");
    assert_eq!(value("codex_cli_simplified_flow"), "true");
    let redirect = url::Url::parse(value("redirect_uri")).expect("callback URI");
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/auth/callback");
    let port = redirect.port().expect("callback port");
    assert!([1455, 1457].contains(&port));
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("callback listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("callback read deadline");
    let request = format!(
        "GET /auth/callback?code=synthetic-code&state={}&client_id={CLIENT} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
        value("state")
    );
    stream
        .write_all(request.as_bytes())
        .expect("callback request");
    let mut response = Vec::new();
    stream
        .take(8192)
        .read_to_end(&mut response)
        .expect("callback response");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "callback was not accepted"
    );
}

fn isolated_stage(near_expiry: bool) {
    let stage = std::env::var(STAGE).expect("fixture stage");
    assert_eq!(nix::unistd::geteuid().as_raw(), 0);
    assert!(
        Command::new("/usr/bin/ip")
            .args(["link", "set", "lo", "up"])
            .status()
            .unwrap()
            .success()
    );
    if stage == "archived_records" {
        let root = StateRoot::admit(Path::new("/root/.local/state/arany")).unwrap();
        let id = Uuid::now_v7();
        for (kind, record) in [
            (
                RecordKind::ChatgptRegistration,
                serde_json::json!({
                    "schema": 1, "host_id": UNFINISHED_HOST, "issued_client_id": "oaiapp_retired"
                }),
            ),
            (
                RecordKind::ChatgptAccounts,
                serde_json::json!({
                    "schema": 3, "selected": id, "model_checks": [],
                    "accounts": [{
                        "id": id, "host_id": UNFINISHED_HOST,
                        "client_id": "oaiapp_retired", "subject": "synthetic-subject",
                        "chatgpt_account_id": null, "keyring_slot": null,
                        "storage": "private_file", "renewal_pending": false,
                        "signout_pending": false, "disconnected": true,
                        "plan_permission_missing": false, "token": null
                    }]
                }),
            ),
        ] {
            root.record(kind)
                .replace(&serde_json::to_vec(&record).unwrap())
                .unwrap();
        }
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let mut auth = issuer(receiver, true, 1, near_expiry);
    let catalog_started = Arc::new(AtomicBool::new(false));
    let mut backend = checked_turn::backend_server(
        Arc::clone(&catalog_started),
        if near_expiry { RENEWED_ACCESS } else { ACCESS },
    );
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(crate::product::Isolation::Caller, SHELL)
            .env("SHELL", "/usr/bin/sh")
            .env("HOME", "/root")
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/root/missing-bus")
            .env("SSL_CERT_FILE", "/fixture/ca.pem")
            .spawn(),
    );
    wait_for_stage(
        &mut pty,
        0,
        b"empty Enter selects 1. API accounts\r\nInput:\r\n",
        None,
    );
    let accounts_at = pty.send(b"API accounts\r");
    pty.acknowledge_linear_pages((accounts_at, API_ACCOUNTS_PROMPT));
    pty.send(b"Manage ChatGPT account\r");
    wait_for_stage(
        &mut pty,
        0,
        b"empty Enter selects 1. Cancel\r\nInput:\r\n",
        None,
    );
    let warning_at = pty.send(b"Use private file\r");
    advance_setup_warning_pages(&mut pty, (warning_at, !near_expiry));
    pty.send(b"Accept\r");
    let authorization = wait_browser_url(&mut pty, None);
    let nonce = authorization
        .query_pairs()
        .find(|(name, _)| name == "nonce")
        .unwrap()
        .1
        .into_owned();
    let redirect = authorization
        .query_pairs()
        .find(|(name, _)| name == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let (jwks, signed) = identity_fixture(&nonce);
    let token = serde_json::to_vec(&serde_json::json!({
        "access_token": ACCESS, "refresh_token": REFRESH, "id_token": signed,
        "expires_in": if near_expiry { 1 } else { 3600 }
    }))
    .unwrap();
    sender
        .send(IssuerReply {
            token,
            jwks,
            redirect_uri: redirect,
            nonce,
        })
        .unwrap();
    let configured_at = pty.transcript.len();
    callback(&authorization);
    wait_for_stage(&mut pty, configured_at, b"gpt-6.1-sol", Some(&signed));
    wait_for_stage(&mut pty, configured_at, b"Input:\r\n", Some(&signed));
    let quit_at = pty.send(b"/exit\r");
    wait_for_stage(&mut pty, quit_at, b"TTY_AFTER:", Some(&signed));
    assert!(pty.finish().status.success());
    assert_eq!(pty.answered.cursor_queries, 0);
    assert!(!pty.transcript.contains(&b'\x1b'));
    let text = std::str::from_utf8(&pty.transcript).unwrap();
    assert_tty_restored(text);
    assert_eq!(
        text.contains("Saved records this version cannot read"),
        stage == "archived_records"
    );
    let view = session("/root/state-chatgpt");
    assert!(view.runs.is_empty());
    assert!(view.pending_provider_setup_edit.is_none());
    assert!(!view.provider_setup_uncertain);
    assert_eq!(view.defaults.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(view.defaults.effort, Some(arany::Effort::Low));
    let account_id = view.defaults.account_id.unwrap();
    let root = StateRoot::open_existing(Path::new("/root/.local/state/arany")).unwrap();
    let record = root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let index: serde_json::Value = serde_json::from_slice(&record).unwrap();
    assert_eq!(index["selected"], account_id.to_string());
    assert!(index["accounts"][0]["chatgpt_account_id"].as_str() == Some(ACCOUNT));
    assert_eq!(index["accounts"][0]["client_id"], CLIENT);
    assert_eq!(index["accounts"][0]["renewal_pending"], false);
    let journal = std::fs::read("/root/state-chatgpt/events.sqlite3").unwrap();
    for secret in [ACCESS, RENEWED_ACCESS, REFRESH, signed.as_str(), ACCOUNT] {
        for bytes in [&pty.transcript, &journal] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes())
            );
        }
    }
    checked_turn::assert_backend_turn(&root, &record);
    assert!(catalog_started.load(Ordering::SeqCst));
    backend.responder.take().unwrap().join().unwrap();
    let logout = Command::new("/arany")
        .args(["provider", "logout", "chatgpt"])
        .env_clear()
        .env("HOME", "/root")
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .current_dir("/root/workspace")
        .bounded_output()
        .unwrap();
    assert!(logout.status.success());
    auth.responder.take().unwrap().join().unwrap();
    let after = root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let after: serde_json::Value = serde_json::from_slice(&after).unwrap();
    assert_eq!(after["accounts"][0]["disconnected"], true);
    assert!(after["accounts"][0]["token"].is_null());
}

#[test]
#[ignore = "native Linux networkless ChatGPT OAuth fixture; requires OpenSSL, Python, ip, unshare, bwrap, and script"]
fn release_private_file_chatgpt_sign_in_selects_a_backend_model_without_probe() {
    if std::env::var_os(STAGE).is_some() {
        isolated_stage(false);
        return;
    }
    for stage in ["archived_records", "account_only"] {
        run_isolated(SIGN_IN_TEST, stage);
    }
}

#[test]
#[ignore = "native Linux networkless ChatGPT renewal and backend turn; requires OpenSSL, Python, ip, unshare, bwrap, and script"]
fn release_private_file_chatgpt_renews_before_a_backend_turn() {
    if std::env::var_os(STAGE).is_some() {
        isolated_stage(true);
        return;
    }
    run_isolated(RENEWAL_TEST, "near_expiry");
}

fn run_isolated(test_name: &str, stage: &str) {
    let temp = tempfile::tempdir().expect("private ChatGPT fixture");
    let fixture_dir = temp.path().join("fixture");
    let home = temp.path().join("home");
    std::fs::create_dir(&fixture_dir).expect("fixture directory");
    std::fs::create_dir(&home).expect("private home");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
        .expect("owner-only home");
    std::fs::create_dir(home.join("workspace")).expect("Workspace");
    fixture(&fixture_dir);
    let browser = temp.path().join("browser");
    std::fs::write(
        &browser,
        b"#!/usr/bin/sh\n[ \"$1\" = open ] || exit 2\nprintf '%s' \"$2\" > /root/browser-url\n",
    )
    .expect("browser stub");
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755))
        .expect("browser stub mode");
    let hosts = temp.path().join("hosts");
    std::fs::write(
        &hosts,
        b"127.0.0.1 api.openai.com chatgpt.com\n127.0.0.2 auth.openai.com\n",
    )
    .expect("private hosts");
    let nsswitch = temp.path().join("nsswitch.conf");
    std::fs::write(&nsswitch, b"passwd: files\nhosts: files\n").expect("private name service");
    let passwd = temp.path().join("passwd");
    std::fs::write(&passwd, b"owner:x:0:0::/root:/usr/bin/sh\n").expect("private passwd");
    let output = Command::new("/usr/bin/timeout")
        .args([
            "-k",
            "2s",
            "60s",
            "/usr/bin/unshare",
            "--map-auto",
            "--map-user",
            "0",
            "--map-group",
            "0",
            "--user",
            "--mount",
            "--net",
            "--fork",
            "--kill-child",
            "/usr/bin/bwrap",
            "--unshare-pid",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--ro-bind",
        ])
        .arg(&passwd)
        .args(["/etc/passwd", "--ro-bind"])
        .arg(&hosts)
        .args(["/etc/hosts", "--ro-bind"])
        .arg(&nsswitch)
        .args(["/etc/nsswitch.conf", "--chmod", "0755", "/etc", "--bind"])
        .arg(&home)
        .args(["/root", "--ro-bind"])
        .arg(&fixture_dir)
        .args(["/fixture", "--ro-bind"])
        .arg(&browser)
        .args(["/usr/bin/gio", "--ro-bind"])
        .arg(std::env::current_exe().expect("test executable"))
        .args([
            "/test",
            "--ro-bind",
            env!("CARGO_BIN_EXE_arany"),
            "/arany",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--clearenv",
            "--setenv",
            STAGE,
            stage,
            "--",
            "/test",
            "--exact",
            test_name,
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .bounded_output_for(Duration::from_secs(65), 64 * 1024)
        .expect("isolated ChatGPT setup gate");
    assert!(
        output.status.success(),
        "isolated ChatGPT setup failed: {}",
        format!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .replace(ACCESS, "[redacted]")
        .replace(REFRESH, "[redacted]")
        .chars()
        .take(4096)
        .flat_map(char::escape_default)
        .collect::<String>()
    );
    crate::process::assert_single_test_completed(&output.stdout, stage);
}
