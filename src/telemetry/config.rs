use super::TelemetryConfigError;
use reqwest::Url;
use std::{ffi::OsString, net::IpAddr};

const MAX_ENDPOINT_BYTES: usize = 512;
const UNSUPPORTED_VARIABLES: &[&str] = &[
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_COMPRESSION",
    "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_CERTIFICATE",
    "OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE",
    "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE",
    "OTEL_EXPORTER_OTLP_TRACES_CLIENT_CERTIFICATE",
    "OTEL_EXPORTER_OTLP_CLIENT_KEY",
    "OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY",
    "OTEL_EXPORTER_OTLP_INSECURE",
    "OTEL_EXPORTER_OTLP_TRACES_INSECURE",
];

pub(super) struct LocalEndpoint(pub(super) String);

pub(super) fn resolve_endpoint(
    cli_endpoint: Option<&str>,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<LocalEndpoint>, TelemetryConfigError> {
    let selected = if let Some(value) = cli_endpoint {
        Some((value.to_owned(), false))
    } else if let Some(value) =
        read_endpoint_variable(&lookup, "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")?
    {
        Some((value, true))
    } else {
        read_endpoint_variable(&lookup, "OTEL_EXPORTER_OTLP_ENDPOINT")?.map(|value| (value, false))
    };
    let Some((value, exact)) = selected else {
        return Ok(None);
    };
    if let Some(&name) = UNSUPPORTED_VARIABLES
        .iter()
        .find(|&&name| lookup(name).is_some())
    {
        return Err(TelemetryConfigError::UnsupportedVariable(name));
    }
    parse_endpoint(&value, exact).map(Some)
}

fn read_endpoint_variable(
    lookup: &impl Fn(&str) -> Option<OsString>,
    name: &'static str,
) -> Result<Option<String>, TelemetryConfigError> {
    lookup(name)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .into_string()
                .map_err(|_| TelemetryConfigError::InvalidEncoding(name))
        })
        .transpose()
}

pub(super) fn parse_endpoint(
    value: &str,
    exact: bool,
) -> Result<LocalEndpoint, TelemetryConfigError> {
    if value.is_empty() || value.len() > MAX_ENDPOINT_BYTES {
        return Err(TelemetryConfigError::InvalidEndpoint);
    }
    let authority = value
        .strip_prefix("http://")
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .ok_or(TelemetryConfigError::InvalidEndpoint)?;
    let raw_host = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed
            .split_once(']')
            .ok_or(TelemetryConfigError::InvalidEndpoint)?;
        if !suffix.is_empty() && !valid_port_suffix(suffix) {
            return Err(TelemetryConfigError::InvalidEndpoint);
        }
        host
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if !valid_port_suffix(&format!(":{port}")) {
            return Err(TelemetryConfigError::InvalidEndpoint);
        }
        host
    } else {
        authority
    };
    let raw_ip = raw_host
        .parse::<IpAddr>()
        .map_err(|_| TelemetryConfigError::InvalidEndpoint)?;
    let mut url = Url::parse(value).map_err(|_| TelemetryConfigError::InvalidEndpoint)?;
    let host = url
        .host_str()
        .ok_or(TelemetryConfigError::InvalidEndpoint)?;
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .map_err(|_| TelemetryConfigError::InvalidEndpoint)?;
    if url.scheme() != "http"
        || !ip.is_loopback()
        || ip != raw_ip
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
        || url.path().len() > 256
        || url.path().contains('%')
    {
        return Err(TelemetryConfigError::InvalidEndpoint);
    }
    if !exact {
        let prefix = url.path().trim_end_matches('/');
        url.set_path(&format!("{prefix}/v1/traces"));
    }
    if url.path().len() > 256 {
        return Err(TelemetryConfigError::InvalidEndpoint);
    }
    Ok(LocalEndpoint(url.into()))
}

fn valid_port_suffix(suffix: &str) -> bool {
    suffix
        .strip_prefix(':')
        .is_some_and(|port| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
}
