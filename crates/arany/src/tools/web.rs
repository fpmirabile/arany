use super::ToolError;
use super::catalog::string;
use super::config::{Config, WebSearch};
use super::guard::GuardContext;
use super::network::{PROXY_ENDPOINT, WEB_CA_TARGET};
use super::types::{EffectIntent, NetworkGrant, ToolCall};
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const MAX_URL_BYTES: usize = 2048;
pub(crate) const MAX_QUERY_BYTES: usize = 512;
pub(crate) const WEB_FETCH_CHARS: usize = 100_000;
pub(crate) const WEB_SEARCH_RESULTS: usize = 8;
pub(crate) const MAX_SESSION_SEARCHES: usize = 200;
pub(crate) const WEB_DOWNLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_CA_BUNDLE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ENTITY_BYTES: usize = 10;
const MAX_SEARCH_FIELD_CHARS: usize = 512;
#[cfg(target_os = "linux")]
const MAX_KEY_BYTES: usize = 512;
const TRUNCATION_MARKER: &str = "\n[web_fetch truncated]";
#[cfg(target_os = "linux")]
const CA_BUNDLES: [&str; 4] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

pub(super) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::WebFetch { url } => url.len() <= MAX_URL_BYTES && target(url).is_some(),
        ToolCall::WebSearch { query } => {
            !query.trim().is_empty()
                && query.len() <= MAX_QUERY_BYTES
                && !query.chars().any(char::is_control)
        }
        _ => false,
    }
}

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    Some(match operation {
        "web_fetch" => vec![("url", string())],
        "web_search" => vec![("query", string())],
        _ => return None,
    })
}

// Domains need HTTPS on the default port; plain HTTP and ports are for numeric loopback only.
fn target(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return None;
    }
    let host = parsed.host_str()?;
    let loopback = host
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    let admitted = match parsed.scheme() {
        "https" => loopback || parsed.port().is_none() && super::network::valid_pattern(host),
        "http" => loopback,
        _ => false,
    };
    admitted.then_some(parsed)
}

// Exact host or one leading `*.` label, as in the network grant.
pub(super) fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
        None => pattern == host,
    }
}

pub(super) fn host(call: &ToolCall) -> Option<String> {
    match call {
        ToolCall::WebFetch { url } => {
            Some(target(url)?.host_str()?.trim_matches(['[', ']']).to_owned())
        }
        _ => None,
    }
}

impl super::ToolRuntime {
    pub(crate) fn allows_research_web(&self, call: &ToolCall) -> bool {
        self.guard_available && self.config.allows_research_web(call)
    }

    pub(crate) fn lists_web_host(&self, call: &ToolCall) -> bool {
        self.config.lists_web_host(call)
    }

    // Web calls carry the proxy class like commands; their digest binds it without any process profile.
    pub(crate) fn bind_web_network(&self, intent: &mut EffectIntent) -> Result<(), ToolError> {
        if self.worker.is_none() && self.config.admits_web(&intent.call) {
            intent.limits.network = NetworkGrant::Proxy;
            intent.enforcement_digest = self.web_digest(intent)?;
        }
        Ok(())
    }

    pub(super) fn web_digest(&self, intent: &EffectIntent) -> Result<[u8; 32], ToolError> {
        if intent.limits.network != NetworkGrant::Proxy {
            return Ok(self.enforcement_digest);
        }
        super::guard::intent_enforcement_digest(
            false,
            &[],
            &intent.limits,
            #[cfg(target_os = "macos")]
            self.system_programs.as_deref(),
        )
    }
}

impl Config {
    // Fetch reaches any non-denied host once the permission flow approves it; search reaches only its backend.
    pub(super) fn admits_web(&self, call: &ToolCall) -> bool {
        match call {
            #[cfg(target_os = "linux")]
            ToolCall::WebFetch { .. } => host(call).is_some_and(|host| {
                super::network::Network {
                    allow: vec![host.clone()],
                    deny: self.network.as_ref().map(|n| n.deny.clone()).unwrap_or_default(),
                }
                .lists(&host)
            }),
            #[cfg(target_os = "linux")]
            ToolCall::WebSearch { .. } => self.web_search.is_some(),
            _ => false,
        }
    }

    pub(super) fn worker_web_tools(&self) -> impl Iterator<Item = crate::worker::WorkerTool> {
        use crate::worker::WorkerTool;
        [
            (self.network.is_some(), WorkerTool::WebFetch),
            (self.web_search.is_some(), WorkerTool::WebSearch),
        ]
        .into_iter()
        .filter_map(|(available, tool)| {
            (self.child_web && cfg!(target_os = "linux") && available).then_some(tool)
        })
    }

    pub(crate) fn allows_research_web(&self, call: &ToolCall) -> bool {
        self.child_web
            && match call {
                ToolCall::WebFetch { .. } => self.lists_web_host(call),
                ToolCall::WebSearch { .. } => self.admits_web(call),
                _ => false,
            }
    }

    // A listed host needs no per-call approval.
    pub(super) fn lists_web_host(&self, call: &ToolCall) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.network
                .as_ref()
                .zip(host(call))
                .is_some_and(|(network, host)| network.lists(&host))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = call;
            false
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn grant(
    config: &Config,
    call: &ToolCall,
) -> Result<super::network::Network, ToolError> {
    match call {
        ToolCall::WebFetch { .. } => {
            let mut grant = config.network.clone().unwrap_or_default();
            let host = host(call).ok_or(ToolError::Configuration)?;
            if !grant.allow.contains(&host) {
                grant.allow.push(host);
            }
            Ok(grant)
        }
        ToolCall::WebSearch { .. } => {
            let backend = config.web_search.as_ref().ok_or(ToolError::Configuration)?;
            Ok(super::network::Network {
                allow: vec![backend.host().ok_or(ToolError::Configuration)?],
                deny: Vec::new(),
            })
        }
        _ => Err(ToolError::Configuration),
    }
}

#[cfg(target_os = "linux")]
// The key crosses the Guard task pipe only; it is never part of an intent, Event or digest.
pub(super) fn secret(config: &Config, call: &ToolCall) -> Result<Option<String>, ToolError> {
    let (ToolCall::WebSearch { .. }, Some(backend)) = (call, &config.web_search) else {
        return Ok(None);
    };
    let key = std::env::var(&backend.key).map_err(|_| ToolError::Configuration)?;
    (!key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key.bytes().all(|byte| byte.is_ascii_graphic()))
    .then_some(Some(key))
    .ok_or(ToolError::Configuration)
}

#[cfg(target_os = "linux")]
pub(super) fn ca_bundle() -> Option<PathBuf> {
    std::env::var_os("SSL_CERT_FILE")
        .map(PathBuf::from)
        .into_iter()
        .chain(CA_BUNDLES.iter().map(PathBuf::from))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_CA_BUNDLE_BYTES)
        })
}

pub(super) async fn run(
    site: &GuardContext<'_>,
    secret: Option<&str>,
) -> Result<String, ToolError> {
    let timeout = site.remaining_runtime();
    let budget = site.intent.limits.result_bytes as usize;
    match &site.intent.call {
        ToolCall::WebFetch { url } => fetch(url, timeout, budget, &site.captured).await,
        ToolCall::WebSearch { query } => {
            let backend = site
                .config
                .web_search
                .as_ref()
                .ok_or(ToolError::Configuration)?;
            search(
                backend,
                secret.ok_or(ToolError::Configuration)?,
                query,
                timeout,
                budget,
            )
            .await
        }
        _ => Err(ToolError::Configuration),
    }
}

fn failure(message: &str) -> Result<String, ToolError> {
    Ok(json!({ "error": message }).to_string())
}

fn client(https: bool, timeout: Duration) -> Result<reqwest::Client, ToolError> {
    let mut builder = crate::provider::http_client(timeout)
        .proxy(reqwest::Proxy::all(PROXY_ENDPOINT).map_err(|_| ToolError::Configuration)?);
    if https {
        let bytes = read_bounded(WEB_CA_TARGET)?;
        let roots = reqwest::Certificate::from_pem_bundle(&bytes)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        builder = builder.tls_certs_only(roots);
    } else {
        // No ambient root loading: the Guard has no certificate directory, and plain HTTP needs none.
        builder = builder.tls_certs_only(Vec::<reqwest::Certificate>::new());
    }
    builder.build().map_err(|_| ToolError::Operation)
}

fn read_bounded(path: &str) -> Result<Vec<u8>, ToolError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_CA_BUNDLE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if bytes.len() as u64 > MAX_CA_BUNDLE_BYTES {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(bytes)
}

struct Page {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    cut: bool,
}

async fn get(
    url: url::Url,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<Result<Page, &'static str>, ToolError> {
    let client = client(url.scheme() == "https", timeout)?;
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let Ok(mut response) = request.send().await else {
        return Ok(Err(
            "request failed: the host is not listed, unreachable or refused by the egress proxy",
        ));
    };
    let status = response.status();
    if status.is_redirection() {
        return Ok(Err(
            "redirects are refused; fetch the final URL if it is listed",
        ));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let encoded = response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"));
    if encoded {
        return Ok(Err("compressed responses are not supported"));
    }
    let mut body = Vec::new();
    let mut cut = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = WEB_DOWNLOAD_BYTES - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if chunk.len() > room {
                    cut = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => return Ok(Err("the response ended before it completed")),
        }
    }
    Ok(Ok(Page {
        status: status.as_u16(),
        content_type,
        body,
        cut,
    }))
}

async fn fetch(
    raw: &str,
    timeout: Duration,
    budget: usize,
    captured: &std::sync::Mutex<Option<Vec<u8>>>,
) -> Result<String, ToolError> {
    let url = target(raw).ok_or(ToolError::Configuration)?;
    let page = match get(
        url,
        &[(
            "Accept",
            "text/html, text/markdown, text/plain, application/json",
        )],
        timeout,
    )
    .await?
    {
        Ok(page) => page,
        Err(message) => return failure(message),
    };
    let html = page.content_type == "text/html" || page.content_type == "application/xhtml+xml";
    let text = String::from_utf8_lossy(&page.body);
    let text = if html {
        extract_text(&text)
    } else if page.content_type.starts_with("text/") || page.content_type == "application/json" {
        text.into_owned()
    } else {
        return failure("unsupported content type; only text, HTML and JSON are read");
    };
    let total = json!({"bytes": text.len(), "lines": text.lines().count()});
    let rendered = fit(
        |shown, truncated| {
            let mut result = json!({
                "url": raw,
                "status": page.status,
                "content_type": page.content_type,
                "truncated": truncated,
                "text": shown,
            });
            if truncated {
                result["output_total"] = total.clone();
            }
            result.to_string()
        },
        &text,
        page.cut,
        budget,
    );
    if serde_json::from_str::<Value>(&rendered).is_ok_and(|value| value["truncated"] == true) {
        let retained = text.len().min(super::spill::MAX_SPILL_RESULT_BYTES);
        *captured.lock().map_err(|_| ToolError::Operation)? =
            Some(text.as_bytes()[..retained].to_vec());
    }
    Ok(rendered)
}

// Cuts to the character cap, then to the result budget, and marks every cut.
fn fit(render: impl Fn(&str, bool) -> String, text: &str, cut: bool, budget: usize) -> String {
    let mut end = text
        .char_indices()
        .nth(WEB_FETCH_CHARS)
        .map_or(text.len(), |(index, _)| index);
    let mut truncated = cut || end < text.len();
    loop {
        let shown = if truncated {
            format!("{}{TRUNCATION_MARKER}", &text[..end])
        } else {
            text[..end].to_owned()
        };
        let rendered = render(&shown, truncated);
        if rendered.len() <= budget || end == 0 {
            return rendered;
        }
        end = end / 4 * 3;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        truncated = true;
    }
}

async fn search(
    backend: &WebSearch,
    key: &str,
    query: &str,
    timeout: Duration,
    budget: usize,
) -> Result<String, ToolError> {
    let mut url = url::Url::parse(&backend.endpoint).map_err(|_| ToolError::Configuration)?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("count", &WEB_SEARCH_RESULTS.to_string());
    let page = match get(
        url,
        &[
            ("Accept", "application/json"),
            ("X-Subscription-Token", key),
        ],
        timeout,
    )
    .await?
    {
        Ok(page) => page,
        Err(message) => return failure(message),
    };
    if !(200..300).contains(&page.status) {
        return failure(&format!("the search backend answered HTTP {}", page.status));
    }
    if String::from_utf8_lossy(&page.body).contains(key) {
        return failure("the search backend returned an invalid response");
    }
    let Some(mut results) = results(&page.body) else {
        return failure("the search backend returned an invalid response");
    };
    if results.iter().any(|row| reflects_key(row, key)) {
        return failure("the search backend returned an invalid response");
    }
    loop {
        let rendered = json!({"query": query, "results": results}).to_string();
        if rendered.len() <= budget || results.is_empty() {
            return Ok(rendered);
        }
        results.pop();
    }
}

// Checked on decoded text, so escaped spellings of the key are caught too.
fn reflects_key(row: &Value, key: &str) -> bool {
    ["title", "url", "description"]
        .iter()
        .any(|name| row[*name].as_str().is_some_and(|text| text.contains(key)))
}

fn results(body: &[u8]) -> Option<Vec<Value>> {
    let parsed = serde_json::from_slice::<Value>(body).ok()?;
    let field = |row: &Value, name: &str| {
        row[name]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(MAX_SEARCH_FIELD_CHARS)
            .collect::<String>()
    };
    Some(
        parsed["web"]["results"]
            .as_array()?
            .iter()
            .take(WEB_SEARCH_RESULTS)
            .map(|row| {
                json!({
                    "title": field(row, "title"),
                    "url": field(row, "url"),
                    "description": field(row, "description"),
                })
            })
            .collect(),
    )
}

fn extract_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(WEB_DOWNLOAD_BYTES) / 2);
    let mut rest = html;
    let mut skip: Option<&'static str> = None;
    while let Some(open) = rest.find('<') {
        if skip.is_none() {
            push_text(&mut out, &rest[..open]);
        }
        rest = &rest[open..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        let Some(close) = rest.find('>') else {
            rest = "";
            break;
        };
        let tag = rest[1..close].trim_start_matches('/');
        let name: String = tag
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        let closing = rest[1..].starts_with('/');
        match (skip, name.as_str()) {
            (Some(active), _) if closing && name == active => skip = None,
            (None, "script") if !closing => skip = Some("script"),
            (None, "style") if !closing => skip = Some("style"),
            (None, "head") if !closing => skip = Some("head"),
            (
                None,
                "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre"
                | "ul" | "ol" | "table" | "section" | "article",
            ) => {
                out.push('\n');
            }
            _ => {}
        }
        rest = &rest[close + 1..];
    }
    if skip.is_none() {
        push_text(&mut out, rest);
    }
    collapse(&out)
}

fn push_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        match rest
            .as_bytes()
            .iter()
            .take(MAX_ENTITY_BYTES + 1)
            .position(|byte| *byte == b';')
            .and_then(|end| entity(&rest[1..end]).map(|c| (c, end)))
        {
            Some((decoded, end)) => {
                out.push(decoded);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code).filter(|c| !c.is_control())
        }
    }
}

// Runs of spaces collapse to one; blank-line runs collapse to one blank line.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests;
