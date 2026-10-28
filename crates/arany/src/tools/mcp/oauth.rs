use super::super::config::{Config, MAX_MCP_SERVERS, McpAuth, Server};
use super::super::web::{MAX_URL_BYTES, valid_endpoint};
use crate::store::{RecordFormat, RecordKind, StateRoot};
use crate::validate::{parse_json_bounded, valid_name};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

pub(crate) const MAX_ACCESS_TOKEN_BYTES: usize = 4096;
const MAX_CLIENT_ID_BYTES: usize = 256;
const MAX_CLAIM_BYTES: usize = 64;
pub(crate) const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_JSON_NODES: usize = 4096;
const MAX_JSON_DEPTH: usize = 16;
const RANDOM_TEXT_BYTES: usize = 32;
const MAX_CALLBACK_HEAD_BYTES: usize = 8192;
const MAX_CALLBACK_CONNECTIONS: usize = 8;
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CALLBACK_DEADLINE: Duration = Duration::from_secs(180);
const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);
// A token this close to expiry is renewed before the call that would carry it.
#[cfg(any(target_os = "linux", test))]
const EXPIRY_SKEW_SECS: u64 = 60;
const MAX_EXPIRES_IN_SECS: u64 = 365 * 24 * 60 * 60;
const CALLBACK_PATH: &str = "/callback";

#[derive(Debug, thiserror::Error, Clone, Copy, Eq, PartialEq)]
pub enum McpAuthError {
    #[error("the server is not a configured remote MCP server with OAuth")]
    NotConfigured,
    #[error("the server or its authorization server does not offer the required OAuth features")]
    Unsupported,
    #[error("an authorization endpoint was unreachable or refused by the destination policy")]
    Unavailable,
    #[error("the authorization was declined, expired or did not match this request")]
    Rejected,
    #[error("the OAuth token record could not be read or written")]
    Storage,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tokens {
    schema: u8,
    servers: BTreeMap<String, Grant>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    url: String,
    client_id: String,
    token_endpoint: String,
    #[serde(deserialize_with = "Option::deserialize")]
    revocation_endpoint: Option<String>,
    access_token: String,
    #[serde(deserialize_with = "Option::deserialize")]
    refresh_token: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    expires_at: Option<u64>,
    renewal: Renewal,
    // Names the one in-flight renewal; a settlement for any other claim is stale.
    #[serde(deserialize_with = "Option::deserialize")]
    claim: Option<String>,
}

// A refresh in flight (or whose outcome is unknown) blocks every later use of the refresh token until a new login.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Renewal {
    Ready,
    Renewing,
    Rejected,
}

pub(crate) const MCP_OAUTH: RecordFormat = RecordFormat {
    kind: RecordKind::McpOauth,
    field: "schema",
    floor: 1,
    current: 1,
    steps: &[],
    accepts: |bytes| Tokens::decode(bytes).is_ok(),
};

fn token_text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && value.bytes().all(|byte| byte.is_ascii_graphic())
}

impl Grant {
    fn valid(&self) -> bool {
        valid_endpoint(&self.url)
            && valid_endpoint(&self.token_endpoint)
            && self
                .revocation_endpoint
                .as_deref()
                .is_none_or(valid_endpoint)
            && (self.renewal == Renewal::Renewing) == self.claim.is_some()
            && self
                .claim
                .as_deref()
                .is_none_or(|claim| token_text(claim, MAX_CLAIM_BYTES))
            && token_text(&self.client_id, MAX_CLIENT_ID_BYTES)
            && token_text(&self.access_token, MAX_ACCESS_TOKEN_BYTES)
            && self
                .refresh_token
                .as_deref()
                .is_none_or(|token| token_text(token, MAX_ACCESS_TOKEN_BYTES))
    }

    #[cfg(any(target_os = "linux", test))]
    fn fresh(&self) -> bool {
        self.expires_at
            .is_none_or(|expires| expires > now().saturating_add(EXPIRY_SKEW_SECS))
    }
}

impl Tokens {
    fn decode(bytes: &[u8]) -> Result<Self, McpAuthError> {
        let tokens: Self = serde_json::from_slice(bytes).map_err(|_| McpAuthError::Storage)?;
        (tokens.schema == 1
            && tokens.servers.len() <= MAX_MCP_SERVERS
            && tokens
                .servers
                .iter()
                .all(|(name, grant)| valid_name(name) && grant.valid()))
        .then_some(tokens)
        .ok_or(McpAuthError::Storage)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn open_account(create: bool) -> Result<Option<StateRoot>, McpAuthError> {
    let path = StateRoot::account_path().map_err(|_| McpAuthError::Storage)?;
    if create {
        return StateRoot::admit(&path)
            .map(Some)
            .map_err(|_| McpAuthError::Storage);
    }
    match StateRoot::open_existing(&path) {
        Ok(root) => Ok(Some(root)),
        Err(crate::store::StoreError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(_) => Err(McpAuthError::Storage),
    }
}

fn load(root: &StateRoot) -> Result<Tokens, McpAuthError> {
    match root
        .read_current(&MCP_OAUTH)
        .map_err(|_| McpAuthError::Storage)?
    {
        Some(bytes) => Tokens::decode(&bytes),
        None => Ok(Tokens {
            schema: 1,
            servers: BTreeMap::new(),
        }),
    }
}

fn save(root: &StateRoot, tokens: &Tokens) -> Result<(), McpAuthError> {
    let record = root.record(RecordKind::McpOauth);
    if tokens.servers.is_empty() {
        return record.remove().map_err(|_| McpAuthError::Storage);
    }
    let bytes = serde_json::to_vec(tokens).map_err(|_| McpAuthError::Storage)?;
    if bytes.len() > RecordKind::McpOauth.limit() || Tokens::decode(&bytes).is_err() {
        return Err(McpAuthError::Storage);
    }
    record.replace(&bytes).map_err(|_| McpAuthError::Storage)
}

// Every change re-reads the record under the account lock, so writers serialize and none acts on a stale inspection.
fn mutate<T>(
    root: &StateRoot,
    workspace: &Path,
    edit: impl FnOnce(&mut Tokens) -> Result<T, McpAuthError>,
) -> Result<T, McpAuthError> {
    root.with_account_replacement_lock(workspace, || {
        let mut tokens = load(root)?;
        let value = edit(&mut tokens)?;
        save(root, &tokens)?;
        Ok(value)
    })
    .map_err(|_| McpAuthError::Storage)?
}

fn random_text(bytes: usize) -> Result<String, McpAuthError> {
    let mut buffer = vec![0_u8; bytes];
    SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| McpAuthError::Unavailable)?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

// Moves a still-unchanged, ready grant to Renewing; any other state means another process or a logout got there first.
#[cfg(any(target_os = "linux", test))]
fn claim_renewal(tokens: &mut Tokens, name: &str, seen: &Grant) -> Result<Grant, McpAuthError> {
    let claim = random_text(RANDOM_TEXT_BYTES)?;
    let grant = tokens.servers.get_mut(name).ok_or(McpAuthError::Rejected)?;
    if seen.renewal != Renewal::Ready || *grant != *seen {
        return Err(McpAuthError::Rejected);
    }
    grant.renewal = Renewal::Renewing;
    grant.claim = Some(claim);
    Ok(grant.clone())
}

// Settles only the grant that was claimed; a logout or new login in between wins.
#[cfg(any(target_os = "linux", test))]
fn settle_renewal(
    tokens: &mut Tokens,
    name: &str,
    claimed: &Grant,
    outcome: Renewal,
    renewed: Option<Grant>,
) -> Result<(), McpAuthError> {
    let grant = tokens.servers.get_mut(name).ok_or(McpAuthError::Rejected)?;
    if grant.renewal != Renewal::Renewing || grant.claim != claimed.claim {
        return Err(McpAuthError::Rejected);
    }
    match renewed {
        Some(renewed) => *grant = renewed,
        None => {
            grant.renewal = outcome;
            grant.claim = None;
        }
    }
    Ok(())
}

// What the parent learned about the saved token before the Guard launches.
#[cfg(target_os = "linux")]
pub(crate) enum Access {
    Token(String),
    // Missing, expired without renewal, blocked by an earlier unconfirmed renewal, or refused.
    Unauthorized,
    // This call's own renewal may have consumed the refresh token; the effect is Uncertain.
    Unconfirmed,
}

// The token reaches the Guard over the private task pipe only. A lost or unconfirmed renewal leaves the
// grant Renewing, so the old refresh token is never submitted twice.
#[cfg(target_os = "linux")]
pub(crate) async fn access_token(server: &Server, workspace: &Path, renewing: &mut bool) -> Access {
    let Some(grant) = saved_grant(server) else {
        return Access::Unauthorized;
    };
    if grant.renewal != Renewal::Ready {
        return Access::Unauthorized;
    }
    if grant.fresh() {
        return Access::Token(grant.access_token);
    }
    if grant.refresh_token.is_none() {
        return Access::Unauthorized;
    }
    let Ok(Some(root)) = open_account(false) else {
        return Access::Unauthorized;
    };
    let name = &server.name;
    let Ok(claimed) = mutate(&root, workspace, |tokens| {
        claim_renewal(tokens, name, &grant)
    }) else {
        return Access::Unauthorized;
    };
    *renewing = true;
    match refresh(&claimed).await {
        Ok(renewed) => {
            let token = renewed.access_token.clone();
            match mutate(&root, workspace, |tokens| {
                settle_renewal(tokens, name, &claimed, Renewal::Ready, Some(renewed))
            }) {
                Ok(()) => {
                    *renewing = false;
                    Access::Token(token)
                }
                Err(_) => Access::Unconfirmed,
            }
        }
        Err(McpAuthError::Rejected) => {
            *renewing = false;
            let _ = mutate(&root, workspace, |tokens| {
                settle_renewal(tokens, name, &claimed, Renewal::Rejected, None)
            });
            Access::Unauthorized
        }
        Err(_) => Access::Unconfirmed,
    }
}

#[cfg(target_os = "linux")]
fn saved_grant(server: &Server) -> Option<Grant> {
    let url = server.url.as_deref()?;
    let root = open_account(false).ok()??;
    let grant = load(&root).ok()?.servers.remove(&server.name)?;
    (grant.url == url).then_some(grant)
}

fn oauth_server<'a>(config: &'a Config, name: &str) -> Result<&'a Server, McpAuthError> {
    config
        .mcp
        .iter()
        .find(|server| server.name == name && server.auth == Some(McpAuth::Oauth))
        .filter(|server| server.url.is_some())
        .ok_or(McpAuthError::NotConfigured)
}

pub async fn mcp_login(
    state: &StateRoot,
    workspace: &Path,
    name: &str,
    announce: impl FnOnce(&str),
) -> Result<(), McpAuthError> {
    let config = Config::load(state).map_err(|_| McpAuthError::NotConfigured)?;
    let url = oauth_server(&config, name)?
        .url
        .clone()
        .ok_or(McpAuthError::NotConfigured)?;
    let resource = Url::parse(&url).map_err(|_| McpAuthError::NotConfigured)?;
    let metadata = discover(&resource).await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| McpAuthError::Unavailable)?;
    let port = listener
        .local_addr()
        .map_err(|_| McpAuthError::Unavailable)?
        .port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let client_id = register(&metadata, &redirect).await?;
    let verifier = random_text(RANDOM_TEXT_BYTES)?;
    let challenge = challenge(&verifier);
    let expected_state = random_text(RANDOM_TEXT_BYTES)?;
    let mut authorize =
        Url::parse(&metadata.authorization_endpoint).map_err(|_| McpAuthError::Unsupported)?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &expected_state)
        .append_pair("resource", &url);
    announce(authorize.as_str());
    let code = tokio::time::timeout(CALLBACK_DEADLINE, callback(listener, &expected_state))
        .await
        .map_err(|_| McpAuthError::Rejected)??;
    let reply = request_token(
        &metadata.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
            ("resource", &url),
        ],
    )
    .await?;
    let grant = Grant {
        url,
        client_id,
        token_endpoint: metadata.token_endpoint,
        revocation_endpoint: metadata.revocation_endpoint,
        ..grant_from(&reply, None)?
    };
    if !grant.valid() {
        return Err(McpAuthError::Rejected);
    }
    let root = open_account(true)?.ok_or(McpAuthError::Storage)?;
    mutate(&root, workspace, |tokens| {
        tokens.servers.insert(name.to_owned(), grant);
        Ok(())
    })
}

// Returns whether the authorization server confirmed revocation; the local token is removed either way.
pub async fn mcp_logout(
    state: &StateRoot,
    workspace: &Path,
    name: &str,
) -> Result<bool, McpAuthError> {
    let config = Config::load(state).map_err(|_| McpAuthError::NotConfigured)?;
    oauth_server(&config, name)?;
    let Some(root) = open_account(false)? else {
        return Ok(true);
    };
    let Some(grant) = mutate(&root, workspace, |tokens| Ok(tokens.servers.remove(name)))? else {
        return Ok(true);
    };
    let revoked = match &grant.revocation_endpoint {
        Some(endpoint) => {
            let (token, hint) = match &grant.refresh_token {
                Some(refresh) => (refresh, "refresh_token"),
                None => (&grant.access_token, "access_token"),
            };
            post_form(
                endpoint,
                &[
                    ("token", token),
                    ("token_type_hint", hint),
                    ("client_id", &grant.client_id),
                ],
            )
            .await
            .is_ok()
        }
        None => false,
    };
    Ok(revoked)
}

struct Metadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
}

// RFC 9728 names the authorization server, RFC 8414 describes it; without protected-resource metadata the server origin is the authorization server.
async fn discover(resource: &Url) -> Result<Metadata, McpAuthError> {
    let issuer = match well_known(resource, "oauth-protected-resource").await {
        Some(document) => document["authorization_servers"][0]
            .as_str()
            .and_then(|issuer| Url::parse(issuer).ok())
            .filter(|issuer| valid_endpoint(issuer.as_str()))
            .ok_or(McpAuthError::Unsupported)?,
        None => origin_of(resource),
    };
    let document = well_known(&issuer, "oauth-authorization-server")
        .await
        .ok_or(McpAuthError::Unsupported)?;
    if document["issuer"]
        .as_str()
        .map(|value| value.trim_end_matches('/'))
        != Some(issuer.as_str().trim_end_matches('/'))
        || !document["code_challenge_methods_supported"]
            .as_array()
            .is_some_and(|methods| methods.iter().any(|method| method == "S256"))
    {
        return Err(McpAuthError::Unsupported);
    }
    let endpoint = |key: &str| {
        document[key]
            .as_str()
            .filter(|value| value.len() <= MAX_URL_BYTES && valid_endpoint(value))
            .map(str::to_owned)
    };
    Ok(Metadata {
        authorization_endpoint: endpoint("authorization_endpoint")
            .ok_or(McpAuthError::Unsupported)?,
        token_endpoint: endpoint("token_endpoint").ok_or(McpAuthError::Unsupported)?,
        registration_endpoint: endpoint("registration_endpoint"),
        revocation_endpoint: endpoint("revocation_endpoint"),
    })
}

// The authorization server fallback is the bare origin: no path, query or fragment of the resource.
fn origin_of(resource: &Url) -> Url {
    let mut origin = resource.clone();
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    origin
}

async fn well_known(base: &Url, document: &str) -> Option<Value> {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("/.well-known/{document}{path}"));
    url.set_query(None);
    let client = client_for(&url).await.ok()?;
    let body = read_body(client.get(url).send().await.ok()?).await.ok()?;
    parse_json_bounded(&body, MAX_METADATA_BYTES, MAX_JSON_NODES, MAX_JSON_DEPTH).ok()
}

async fn register(metadata: &Metadata, redirect: &str) -> Result<String, McpAuthError> {
    let endpoint = metadata
        .registration_endpoint
        .as_deref()
        .ok_or(McpAuthError::Unsupported)?;
    let url = Url::parse(endpoint).map_err(|_| McpAuthError::Unsupported)?;
    let client = client_for(&url).await?;
    let registration = json!({
        "client_name": "Arany",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let body = read_body(
        client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&registration).map_err(|_| McpAuthError::Unavailable)?)
            .send()
            .await
            .map_err(|_| McpAuthError::Unavailable)?,
    )
    .await?;
    parse_json_bounded(&body, MAX_METADATA_BYTES, MAX_JSON_NODES, MAX_JSON_DEPTH)
        .ok()
        .and_then(|reply| reply["client_id"].as_str().map(str::to_owned))
        .filter(|id| token_text(id, MAX_CLIENT_ID_BYTES))
        .ok_or(McpAuthError::Rejected)
}

#[cfg(target_os = "linux")]
async fn refresh(grant: &Grant) -> Result<Grant, McpAuthError> {
    let token = grant
        .refresh_token
        .as_deref()
        .ok_or(McpAuthError::Rejected)?;
    let reply = request_token(
        &grant.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
            ("client_id", &grant.client_id),
            ("resource", &grant.url),
        ],
    )
    .await?;
    let renewed = Grant {
        url: grant.url.clone(),
        client_id: grant.client_id.clone(),
        token_endpoint: grant.token_endpoint.clone(),
        revocation_endpoint: grant.revocation_endpoint.clone(),
        ..grant_from(&reply, grant.refresh_token.clone()).map_err(|_| McpAuthError::Unavailable)?
    };
    renewed
        .valid()
        .then_some(renewed)
        .ok_or(McpAuthError::Unavailable)
}

fn grant_from(reply: &Value, kept_refresh: Option<String>) -> Result<Grant, McpAuthError> {
    if !reply["token_type"]
        .as_str()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(McpAuthError::Rejected);
    }
    let text = |key: &str| reply[key].as_str().map(str::to_owned);
    Ok(Grant {
        url: String::new(),
        client_id: String::new(),
        token_endpoint: String::new(),
        revocation_endpoint: None,
        access_token: text("access_token").ok_or(McpAuthError::Rejected)?,
        refresh_token: text("refresh_token").or(kept_refresh),
        renewal: Renewal::Ready,
        claim: None,
        expires_at: reply["expires_in"]
            .as_u64()
            .map(|seconds| now().saturating_add(seconds.min(MAX_EXPIRES_IN_SECS))),
    })
}

async fn request_token(endpoint: &str, form: &[(&str, &str)]) -> Result<Value, McpAuthError> {
    let body = post_form(endpoint, form).await?;
    parse_json_bounded(&body, MAX_METADATA_BYTES, MAX_JSON_NODES, MAX_JSON_DEPTH)
        .map_err(|_| McpAuthError::Unavailable)
}

async fn post_form(endpoint: &str, form: &[(&str, &str)]) -> Result<Vec<u8>, McpAuthError> {
    let url = Url::parse(endpoint).map_err(|_| McpAuthError::Unsupported)?;
    let client = client_for(&url).await?;
    // Built before the await: the encoder's temporary is not Send and must not live across it.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    read_body(
        client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| McpAuthError::Unavailable)?,
    )
    .await
}

// Every authorization request resolves under the Provider destination policy and follows no redirect.
async fn client_for(url: &Url) -> Result<reqwest::Client, McpAuthError> {
    let mut builder = crate::provider::http_client(REQUEST_DEADLINE);
    if let Some(url::Host::Domain(host)) = url.host() {
        let port = url
            .port_or_known_default()
            .ok_or(McpAuthError::Unsupported)?;
        let addresses = crate::provider::resolve_admitted(host, port)
            .await
            .map_err(|_| McpAuthError::Unavailable)?;
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder.build().map_err(|_| McpAuthError::Unavailable)
}

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, McpAuthError> {
    let status = response.status();
    // Only a definite client refusal proves a token was not consumed; anything else may have been.
    if status.is_client_error() && !matches!(status.as_u16(), 408 | 429) {
        return Err(McpAuthError::Rejected);
    }
    if !status.is_success() {
        return Err(McpAuthError::Unavailable);
    }
    if response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(McpAuthError::Unavailable);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| McpAuthError::Unavailable)?
    {
        if body.len() + chunk.len() > MAX_METADATA_BYTES {
            return Err(McpAuthError::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn callback(listener: TcpListener, expected_state: &str) -> Result<String, McpAuthError> {
    let port = listener
        .local_addr()
        .map_err(|_| McpAuthError::Unavailable)?
        .port();
    for _ in 0..MAX_CALLBACK_CONNECTIONS {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| McpAuthError::Unavailable)?;
        let outcome = tokio::time::timeout(CONNECTION_DEADLINE, async {
            let mut head = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.ok()?;
                head.extend_from_slice(&buffer[..read]);
                if read == 0 || head.len() > MAX_CALLBACK_HEAD_BYTES {
                    return None;
                }
            }
            Some(parse_callback(&head, expected_state, port))
        })
        .await
        .ok()
        .flatten();
        let accepted = matches!(outcome, Some(Ok(_)));
        let body: &[u8] = if accepted {
            b"Authorization received. Return to Arany.\n"
        } else {
            b"Authorization was not accepted. Return to Arany.\n"
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = tokio::time::timeout(CONNECTION_DEADLINE, async {
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.shutdown().await
        })
        .await;
        if let Some(result) = outcome.filter(|_| accepted) {
            return result;
        }
    }
    Err(McpAuthError::Rejected)
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// The exact Host header keeps a DNS-rebound page from reading or driving the callback.
fn parse_callback(head: &[u8], expected_state: &str, port: u16) -> Result<String, McpAuthError> {
    let text = std::str::from_utf8(head).map_err(|_| McpAuthError::Rejected)?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or(McpAuthError::Rejected)?;
    let host = format!("127.0.0.1:{port}");
    if !lines.any(|line| {
        line.split_once(':')
            .is_some_and(|(name, value)| name.eq_ignore_ascii_case("host") && value.trim() == host)
    }) {
        return Err(McpAuthError::Rejected);
    }
    let mut parts = line.split(' ');
    let target = match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => target,
        _ => return Err(McpAuthError::Rejected),
    };
    let url =
        Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| McpAuthError::Rejected)?;
    if url.path() != CALLBACK_PATH {
        return Err(McpAuthError::Rejected);
    }
    let (mut code, mut state, mut failed) = (None, None, false);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "error" => failed = true,
            _ => {}
        }
    }
    match (failed, code, state) {
        (false, Some(code), Some(state))
            if state == expected_state && token_text(&code, MAX_ACCESS_TOKEN_BYTES) =>
        {
            Ok(code)
        }
        _ => Err(McpAuthError::Rejected),
    }
}

#[cfg(test)]
mod tests;
