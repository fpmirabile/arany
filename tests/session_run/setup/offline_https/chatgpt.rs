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
const ACCOUNT_ONLY_TEST: &str = "setup::offline_https::chatgpt::release_private_file_chatgpt_sign_in_keeps_account_without_check";
const CHECKED_TURN_TEST: &str =
    "setup::offline_https::chatgpt::release_private_file_chatgpt_checked_direct_and_team_turns";
const ACCESS: &str = "synthetic-chatgpt-access-token";
const REFRESH: &str = "synthetic-chatgpt-refresh-token";
const CLIENT: &str = "oaiapp_synthetic_offline";
const UNFINISHED_HOST: &str = "2e664d00-3f9a-40c3-adeb-c6447313a871";
const OTHER_PENDING_CLIENT: &str = "oaiapp_other_unfinished";
const SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --setup --state-dir /root/state-chatgpt --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const ENABLE_PLAN_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --setup --state-dir /root/state-enabled --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const SIGNED_OUT_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --state-dir /root/state-after-logout --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";

#[path = "chatgpt/checked_turn.rs"]
mod checked_turn;

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
            "sub": "synthetic-subject"
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
        assert!(bytes.len() <= 32 * 1024, "issuer request cap");
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
    assert!(length <= 32 * 1024 - header_end, "issuer body cap");
    while bytes.len() < header_end + length {
        let count = output.read(&mut chunk).expect("issuer request body");
        assert!(count > 0, "issuer peer closed during body");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 32 * 1024, "issuer request cap");
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

fn issuer(
    reply: Receiver<(Vec<u8>, Vec<u8>, String)>,
    checked_turn: bool,
    exchanges: usize,
) -> Server {
    assert!((1..=2).contains(&exchanges));
    let mut child = Command::new("/usr/bin/openssl")
        .args([
            "s_server",
            "-quiet",
            "-accept",
            "127.0.0.2:443",
            "-cert",
            "/fixture/server.pem",
            "-key",
            "/fixture/server.key",
        ])
        .current_dir("/fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("isolated synthetic issuer");
    let mut input = child.stdin.take().expect("issuer response pipe");
    let mut output = child.stdout.take().expect("issuer request pipe");
    let responder = thread::spawn(move || {
        for _ in 0..exchanges {
            let first = read_request(&mut output);
            assert!(first.starts_with(b"POST /api/accounts/oauth/token HTTP/1.1\r\n"));
            let end = first
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .expect("token request headers")
                + 4;
            let fields = url::form_urlencoded::parse(&first[end..])
                .into_owned()
                .collect::<Vec<_>>();
            for (name, value) in [
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT),
                ("code", "synthetic-code"),
                ("resource", "https://api.openai.com/v1"),
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
            let (token, jwks, redirect_uri) = reply
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
        }
        if checked_turn {
            let discovery = read_request(&mut output);
            assert!(discovery.starts_with(b"GET /.well-known/openid-configuration HTTP/1.1\r\n"));
            send_json(
                &mut input,
                br#"{"issuer":"https://auth.openai.com","revocation_endpoint":"https://auth.openai.com/revoke"}"#,
            );
            let revoke = read_request(&mut output);
            assert!(revoke.starts_with(b"POST /revoke HTTP/1.1\r\n"));
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
    let server = Server {
        child,
        responder: Some(responder),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect("127.0.0.2:443").is_err() {
        assert!(Instant::now() < deadline, "synthetic issuer did not start");
        thread::sleep(Duration::from_millis(20));
    }
    server
}

fn catalog_server(
    checked_turn: bool,
    started: Arc<AtomicBool>,
    refresh_ready: Receiver<()>,
) -> Server {
    let mut child = Command::new("/usr/bin/openssl")
        .args([
            "s_server",
            "-quiet",
            "-accept",
            "127.0.0.1:443",
            "-cert",
            "/fixture/server.pem",
            "-key",
            "/fixture/server.key",
        ])
        .current_dir("/fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("isolated ChatGPT catalog peer");
    let mut input = child.stdin.take().expect("catalog response pipe");
    let mut output = child.stdout.take().expect("catalog request pipe");
    let responder = thread::spawn(move || {
        let request = read_request(&mut output);
        started.store(true, Ordering::SeqCst);
        assert!(request.starts_with(b"GET /v1/models HTTP/1.1\r\n"));
        let headers = std::str::from_utf8(&request).expect("catalog headers");
        assert!(
            headers.lines().any(|line| line
                .trim_end_matches('\r')
                .eq_ignore_ascii_case(&format!("authorization: Bearer {ACCESS}"))),
            "selected ChatGPT access token was not sent to its compiled catalog"
        );
        let body = br#"{"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list"}]}"#;
        send_json(&mut input, body);
        if checked_turn {
            checked_turn::respond_after_catalog(&mut input, &mut output, body, refresh_ready);
        }
    });
    let server = Server {
        child,
        responder: Some(responder),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect("127.0.0.1:443").is_err() {
        assert!(
            Instant::now() < deadline,
            "ChatGPT catalog peer did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    server
}

fn wait_browser_url(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    previous: Option<&url::Url>,
) -> url::Url {
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(output, input, transcript, &mut answered);
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
                assert_eq!(answered, 0, "linear setup queried the cursor");
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

fn wait_for_stage(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut usize,
    start: usize,
    needle: &[u8],
    signed: Option<&str>,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[start..]
        .windows(needle.len())
        .any(|part| part == needle)
    {
        pump(output, input, transcript, answered);
        let mut diagnostic = tail(transcript)
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

fn callback(url: &url::Url, returning: bool, registration_retry: bool) {
    assert!(!returning || !registration_retry);
    assert_eq!(
        url.origin().ascii_serialization(),
        "https://auth.openai.com"
    );
    assert_eq!(url.path(), "/api/accounts/authorize");
    let fields = url.query_pairs().into_owned().collect::<Vec<_>>();
    let value = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .expect("authorization field")
    };
    assert_eq!(
        value("client_id"),
        if returning || registration_retry {
            CLIENT
        } else {
            "dynamic_agent_client"
        }
    );
    if returning {
        assert_eq!(value("prompt"), "consent");
        assert!(!fields.iter().any(|(name, _)| matches!(
            name.as_str(),
            "agent_name_hint" | "id_token_hint" | "force_reconsent"
        )));
    } else if registration_retry {
        assert!(!fields.iter().any(|(name, _)| matches!(
            name.as_str(),
            "prompt" | "agent_name_hint" | "id_token_hint" | "force_reconsent"
        )));
    } else {
        assert_eq!(value("agent_name_hint"), "Arany");
        assert!(!fields.iter().any(|(name, _)| name == "prompt"));
    }
    assert_eq!(
        value("scope"),
        "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
    );
    assert_eq!(value("resource"), "https://api.openai.com/v1");
    let redirect = url::Url::parse(value("redirect_uri")).expect("callback URI");
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/auth/callback");
    let port = redirect.port().expect("callback port");
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

fn isolated_stage(checked_turn: bool) {
    let stage = std::env::var(STAGE).expect("fixture stage");
    let permission_recovery = stage == "plan_disabled";
    let unfinished = matches!(stage.as_str(), "unfinished_resume" | "unfinished_cancel");
    let cancel = stage == "unfinished_cancel";
    assert!(!permission_recovery || !checked_turn);
    assert!(!unfinished || !checked_turn);
    assert_eq!(nix::unistd::geteuid().as_raw(), 0);
    assert_eq!(
        StateRoot::account_path().unwrap(),
        Path::new("/root/.local/state/arany")
    );
    let pending_record = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "host_id": UNFINISHED_HOST,
        "issued_client_id": CLIENT
    }))
    .expect("synthetic unfinished registration");
    if unfinished {
        StateRoot::admit(Path::new("/root/.local/state/arany"))
            .expect("isolated account root")
            .replace_chatgpt_registration_record(&pending_record)
            .expect("seed unfinished registration");
    }
    assert!(
        Command::new("/usr/bin/ip")
            .args(["link", "set", "lo", "up"])
            .status()
            .expect("private loopback")
            .success()
    );
    let (sender, receiver) = mpsc::sync_channel(1);
    let _issuer = (!cancel).then(|| {
        issuer(
            receiver,
            checked_turn,
            if permission_recovery { 2 } else { 1 },
        )
    });
    let catalog_started = Arc::new(AtomicBool::new(false));
    let (refresh_ready, refresh_gate) = mpsc::channel();
    let mut api =
        (!cancel).then(|| catalog_server(checked_turn, Arc::clone(&catalog_started), refresh_gate));
    let mut previous_account = None;
    let mut previous_authorization = None;
    let mut signed_tokens = Vec::with_capacity(2);
    for pass in 0..if permission_recovery { 2 } else { 1 } {
        let permission_disabled = permission_recovery && pass == 0;
        let returning = permission_recovery && pass == 1;
        let state = if returning {
            "/root/state-enabled"
        } else {
            "/root/state-chatgpt"
        };
        let mut command = Command::new("/usr/bin/script");
        command
            .args([
                "-q",
                "-e",
                "-c",
                if returning { ENABLE_PLAN_SHELL } else { SHELL },
                "/dev/null",
            ])
            .env_clear()
            .env("SHELL", "/usr/bin/sh")
            .env("TERM", "dumb")
            .env("HOME", "/root")
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/root/missing-bus")
            .env("SSL_CERT_FILE", "/fixture/ca.pem")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("ChatGPT setup PTY"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = 0;
        wait_for_stage(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            0,
            b"type a choice name: API key or ChatGPT plan",
            None,
        );
        input.write_all(b"ChatGPT plan\r").expect("select plan");
        if returning {
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                0,
                b"type a choice name: Enable plan or Connect new",
                None,
            );
            input
                .write_all(b"Enable plan\r")
                .expect("enable plan explicitly");
        } else {
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                0,
                b"type a choice name: Cancel or Use private file",
                None,
            );
            input
                .write_all(b"Use private file\r")
                .expect("select file storage");
        }
        wait_for_stage(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            0,
            b"empty Enter selects Back\r\nInput:\r\n",
            None,
        );
        input
            .write_all(b"Accept\r")
            .expect("accept plan and storage risk");
        if unfinished {
            let resume_at = transcript.len();
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                resume_at,
                b"type a choice name: Back or Resume; empty Enter selects Back\r\nInput:\r\n",
                None,
            );
            assert!(!Path::new("/root/browser-url").exists());
            input
                .write_all(if cancel { b"\r" } else { b"Resume\r" })
                .expect("explicit unfinished sign-in choice");
            if cancel {
                wait_for_stage(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    resume_at,
                    b"TTY_AFTER:",
                    None,
                );
                assert!(wait_product(child.take()).status.success());
                pump(&mut output, &mut input, &mut transcript, &mut answered);
                assert_eq!(answered, 0);
                assert!(!transcript.contains(&b'\x1b'));
                let text = std::str::from_utf8(&transcript).expect("cancel output");
                let marker = |name: &str| {
                    text.lines()
                        .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                        .expect("TTY marker")
                };
                assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
                let root = StateRoot::open_existing(Path::new("/root/.local/state/arany"))
                    .expect("account root after cancellation");
                assert_eq!(
                    root.read_chatgpt_registration_record().unwrap(),
                    Some(pending_record)
                );
                assert!(root.read_chatgpt_accounts_record().unwrap().is_none());
                assert!(!Path::new("/root/state-chatgpt").exists());
                assert!(!Path::new("/root/browser-url").exists());
                assert!(!catalog_started.load(Ordering::SeqCst));
                return;
            }
        }
        let authorization = wait_browser_url(
            &mut output,
            &mut input,
            &mut transcript,
            previous_authorization.as_ref(),
        );
        if unfinished {
            assert!(authorization.query_pairs().any(|(name, value)| {
                name == "ext_agent_host_id" && value == format!("urn:uuid:{UNFINISHED_HOST}")
            }));
        }
        if let Some(previous) = &previous_authorization {
            let host = |url: &url::Url| {
                url.query_pairs()
                    .find(|(name, _)| name == "ext_agent_host_id")
                    .expect("host identity")
                    .1
                    .into_owned()
            };
            assert_eq!(host(previous), host(&authorization));
        }
        let nonce = authorization
            .query_pairs()
            .find(|(name, _)| name == "nonce")
            .expect("nonce")
            .1
            .into_owned();
        let (jwks, signed) = identity_fixture(&nonce);
        let mut token = serde_json::json!({
            "access_token": ACCESS,
            "refresh_token": REFRESH,
            "id_token": signed,
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke"
        });
        if permission_disabled {
            token["scope"] = "openid profile email".into();
            token
                .as_object_mut()
                .expect("token fields")
                .remove("refresh_token");
        }
        let token = serde_json::to_vec(&token).expect("synthetic token exchange");
        let redirect_uri = authorization
            .query_pairs()
            .find(|(name, _)| name == "redirect_uri")
            .expect("redirect URI")
            .1
            .into_owned();
        sender
            .send((token, jwks, redirect_uri))
            .expect("issuer fixture handoff");
        callback(&authorization, returning, unfinished);
        previous_authorization = Some(authorization);
        signed_tokens.push(signed.clone());
        if permission_disabled {
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                0,
                b"Verified ChatGPT sign-in saved; plan usage is disabled",
                Some(&signed),
            );
            let submit_at = transcript.len();
            input
                .write_all(b"do not start a Run\r")
                .expect("submit without permission");
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                submit_at,
                b"ChatGPT plan permission was not granted; use /setup to enable the plan",
                Some(&signed),
            );
            assert!(
                !catalog_started.load(Ordering::SeqCst),
                "disabled setup requested a catalog"
            );
        } else {
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                0,
                b"ChatGPT ready:",
                Some(&signed),
            );
            wait_for_stage(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                0,
                b"Input:\r\n",
                Some(&signed),
            );
            assert!(
                !transcript
                    .windows(b"Choose model effort".len())
                    .any(|part| part == b"Choose model effort")
            );
            assert!(
                !transcript
                    .windows(b"Plan-consuming check".len())
                    .any(|part| part == b"Plan-consuming check")
            );
            let ready = session(state);
            assert_eq!(ready.defaults.model.as_deref(), Some("gpt-6.1-sol"));
            assert_eq!(ready.defaults.effort, Some(arany::Effort::Low));
            assert!(ready.runs.is_empty(), "setup must not start a Run");
            if checked_turn {
                checked_turn::accept_check_and_run(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    &signed,
                    &refresh_ready,
                );
            }
        }
        let quit_at = transcript.len();
        input.write_all(b"/quit\r").expect("quit ChatGPT Session");
        wait_for_stage(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            quit_at,
            b"TTY_AFTER:",
            Some(&signed),
        );
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "ChatGPT setup product exit");
        assert_eq!(answered, 0, "screen-reader queried the cursor");
        assert!(
            !transcript.contains(&b'\x1b'),
            "screen-reader emitted escapes"
        );
        for secret in [ACCESS, REFRESH]
            .into_iter()
            .chain(signed_tokens.iter().map(String::as_str))
        {
            assert!(
                !transcript
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "credential entered terminal output"
            );
        }
        let text = std::str::from_utf8(&transcript).expect("screen-reader UTF-8");
        assert!(text.contains("remote output-token limit"));
        assert!(text.contains("private account file is not encrypted"));
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("TTY marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        if checked_turn {
            api.as_mut()
                .expect("checked server")
                .responder
                .take()
                .expect("checked peer")
                .join()
                .expect("checked HTTPS requests");
        }
        let account_root =
            StateRoot::open_existing(Path::new("/root/.local/state/arany")).expect("account root");
        let record = account_root
            .read_chatgpt_accounts_record()
            .expect("account index")
            .expect("saved account");
        let metadata = std::fs::metadata(account_root.path().join("chatgpt-accounts.json"))
            .expect("account index metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let index: serde_json::Value = serde_json::from_slice(&record).expect("account index JSON");
        assert_eq!(index["accounts"].as_array().expect("accounts").len(), 1);
        let checks = index["model_checks"].as_array().expect("model checks");
        assert_eq!(checks.len(), if checked_turn { 1 } else { 0 });
        let entry = &index["accounts"][0];
        assert_eq!(entry["storage"], "private_file");
        assert_eq!(entry["client_id"], CLIENT);
        assert_eq!(entry["subject"], "synthetic-subject");
        if unfinished {
            assert_eq!(entry["host_id"], UNFINISHED_HOST);
        }
        if permission_disabled {
            assert!(
                entry["token"].is_null(),
                "disabled account retained a token"
            );
            assert_eq!(entry["disconnected"], true);
            assert_eq!(entry["plan_permission_missing"], true);
            for secret in [ACCESS, REFRESH, signed.as_str()] {
                assert!(
                    !record
                        .windows(secret.len())
                        .any(|part| part == secret.as_bytes()),
                    "disabled metadata contains a token"
                );
            }
        } else {
            assert!(entry["plan_permission_missing"].is_null());
            assert!(
                entry["token"]["credentials"]["access_token"] == ACCESS,
                "stored access token differs"
            );
            assert!(
                entry["token"]["credentials"]["refresh_token"] == REFRESH,
                "stored refresh token differs"
            );
        }
        let account_id: Uuid = entry["id"]
            .as_str()
            .expect("account UUID")
            .parse()
            .expect("account UUID");
        assert_eq!(index["selected"], account_id.to_string());
        if let Some(previous) = previous_account {
            assert_eq!(
                account_id, previous,
                "enablement replaced the saved account"
            );
        }
        previous_account = Some(account_id);
        let registration = account_root
            .read_chatgpt_registration_record()
            .expect("registration")
            .expect("saved registration");
        let registration: serde_json::Value =
            serde_json::from_slice(&registration).expect("registration JSON");
        assert_eq!(registration["host_id"], entry["host_id"]);
        if returning {
            assert_eq!(registration["issued_client_id"], OTHER_PENDING_CLIENT);
            assert!(!text.contains("Resume sign-in?"));
        } else {
            assert!(registration["issued_client_id"].is_null());
        }
        let view = session(state);
        if checked_turn {
            checked_turn::assert_runs(&view, account_id, &checks[0]);
            assert!(!text.contains(checked_turn::FAILURE_BODY_CANARY));
        } else {
            assert!(view.runs.is_empty(), "setup started a Run");
        }
        assert_eq!(view.defaults.provider.as_deref(), Some("chatgpt"));
        assert_eq!(view.defaults.account_id, Some(account_id));
        if checked_turn {
            assert_eq!(view.defaults.model.as_deref(), Some("gpt-6.1-sol"));
            assert_eq!(view.defaults.effort, Some(arany::Effort::Medium));
        } else if permission_disabled {
            assert!(view.defaults.model.is_none() && view.defaults.effort.is_none());
        } else {
            assert_eq!(view.defaults.model.as_deref(), Some("gpt-6.1-sol"));
            assert_eq!(view.defaults.effort, Some(arany::Effort::Low));
        }
        let journal =
            std::fs::read(Path::new(state).join("events.sqlite3")).expect("journal bytes");
        for secret in [ACCESS, REFRESH, checked_turn::FAILURE_BODY_CANARY]
            .into_iter()
            .chain(signed_tokens.iter().map(String::as_str))
        {
            assert!(
                !journal
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "credential entered SQLite"
            );
        }
        if permission_disabled {
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
            ] {
                let output = Command::new("/arany")
                    .args(args)
                    .env_clear()
                    .env("HOME", "/root")
                    .env("SSL_CERT_FILE", "/fixture/ca.pem")
                    .current_dir("/root/workspace")
                    .bounded_output()
                    .expect("disabled public Provider operation");
                assert_eq!(output.status.code(), Some(1));
                assert!(output.stdout.is_empty());
                assert!(
                    output.stderr == b"error: ChatGPT plan permission was not granted; use /setup to enable the plan or explicitly choose an API account\n",
                    "disabled operation must report the fixed permission rejection"
                );
                assert!(
                    account_root
                        .read_chatgpt_accounts_record()
                        .unwrap()
                        .unwrap()
                        == record,
                    "disabled operation changed the account"
                );
                for secret in [ACCESS, REFRESH, signed.as_str()] {
                    assert!(
                        !output
                            .stderr
                            .windows(secret.len())
                            .any(|part| part == secret.as_bytes())
                    );
                }
            }
            let reopened = run_pty(SIGNED_OUT_SHELL, &[], None);
            assert!(!String::from_utf8_lossy(&reopened).contains("Choose access method"));
            for secret in [ACCESS, REFRESH, signed.as_str()] {
                assert!(
                    !reopened
                        .windows(secret.len())
                        .any(|part| part == secret.as_bytes())
                );
            }
            let unconfigured = session("/root/state-after-logout");
            assert!(unconfigured.runs.is_empty());
            assert!(unconfigured.defaults.provider.is_none());
            assert!(unconfigured.defaults.account_id.is_none());
            account_root
                .replace_chatgpt_registration_record(
                    &serde_json::to_vec(&serde_json::json!({
                        "schema": 1,
                        "host_id": entry["host_id"],
                        "issued_client_id": OTHER_PENDING_CLIENT
                    }))
                    .expect("independent unfinished registration"),
                )
                .expect("retain unrelated registration during permission recovery");
            assert!(
                !catalog_started.load(Ordering::SeqCst),
                "disabled account requested a catalog"
            );
            continue;
        }
        let second_output = run_pty(BARE_SHELL, &[], None);
        assert!(!String::from_utf8_lossy(&second_output).contains("Choose access method"));
        for secret in [ACCESS, REFRESH, signed.as_str()] {
            assert!(
                !second_output
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "reopened Session exposed a credential"
            );
        }
        let second = session("/root/state-two");
        assert_ne!(view.id, second.id);
        assert!(second.runs.is_empty(), "bare restart started a Run");
        assert_eq!(second.defaults.provider.as_deref(), Some("chatgpt"));
        assert_eq!(second.defaults.account_id, Some(account_id));
        assert!(second.defaults.model.is_none() && second.defaults.effort.is_none());
        if checked_turn {
            let output = Command::new("/arany")
                .args(["provider", "logout", "chatgpt"])
                .env_clear()
                .env("HOME", "/root")
                .env("SSL_CERT_FILE", "/fixture/ca.pem")
                .current_dir("/root/workspace")
                .bounded_output()
                .expect("product ChatGPT logout");
            assert!(output.status.success(), "product logout failed");
            assert_eq!(output.stdout, b"ChatGPT account disconnected locally\n");
            assert!(output.stderr.is_empty(), "confirmed logout emitted warning");
            let record = account_root
                .read_chatgpt_accounts_record()
                .expect("account index after logout")
                .expect("retained registration mapping");
            let index: serde_json::Value =
                serde_json::from_slice(&record).expect("account index JSON");
            assert_eq!(index["selected"], account_id.to_string());
            assert_eq!(index["accounts"][0]["client_id"], CLIENT);
            assert_eq!(index["accounts"][0]["disconnected"], true);
            assert!(index["accounts"][0]["token"].is_null());
            assert!(index["model_checks"].as_array().unwrap().is_empty());
            let output = Command::new("/arany")
                .args(["provider", "models", "chatgpt"])
                .env_clear()
                .env("HOME", "/root")
                .env("SSL_CERT_FILE", "/fixture/ca.pem")
                .current_dir("/root/workspace")
                .bounded_output()
                .expect("disconnected catalog rejection");
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("run arany --setup"));
            for secret in [ACCESS, REFRESH, signed.as_str()] {
                assert!(
                    !record
                        .windows(secret.len())
                        .any(|part| part == secret.as_bytes())
                );
                assert!(
                    !output
                        .stderr
                        .windows(secret.len())
                        .any(|part| part == secret.as_bytes())
                );
            }
            let view = session("/root/state-chatgpt");
            checked_turn::assert_runs(&view, account_id, &checks[0]);
            let after_logout = run_pty(SIGNED_OUT_SHELL, &[], None);
            assert!(!String::from_utf8_lossy(&after_logout).contains("Choose saved access"));
            let unconfigured = session("/root/state-after-logout");
            assert!(unconfigured.runs.is_empty());
            assert!(unconfigured.defaults.provider.is_none());
            assert!(unconfigured.defaults.account_id.is_none());
        }
    }
    drop(api);
}

#[test]
#[ignore = "native Linux networkless ChatGPT OAuth fixture; requires OpenSSL, ip, unshare, bwrap, and script"]
fn release_private_file_chatgpt_sign_in_keeps_account_without_check() {
    if std::env::var_os(STAGE).is_some() {
        isolated_stage(false);
        return;
    }
    for stage in [
        "unfinished_resume",
        "unfinished_cancel",
        "account_only",
        "plan_disabled",
    ] {
        run_isolated(ACCOUNT_ONLY_TEST, stage);
    }
}

#[test]
#[ignore = "native Linux networkless checked ChatGPT turns; requires OpenSSL, ip, unshare, bwrap, and script"]
fn release_private_file_chatgpt_checked_direct_and_team_turns() {
    if std::env::var_os(STAGE).is_some() {
        isolated_stage(true);
        return;
    }
    run_isolated(CHECKED_TURN_TEST, "checked_turn");
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
        b"127.0.0.1 api.openai.com\n127.0.0.2 auth.openai.com\n",
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
            "--symlink",
            "usr/lib",
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}
