use super::{EXPORT_TIMEOUT, TelemetryConfigError};
use async_trait::async_trait;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use reqwest::redirect::Policy;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};

pub(super) struct BoundedHttpClient {
    client: reqwest::Client,
    runtime: Runtime,
}

impl std::fmt::Debug for BoundedHttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BoundedHttpClient")
    }
}

impl BoundedHttpClient {
    pub(super) fn new() -> Result<Self, TelemetryConfigError> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| TelemetryConfigError::ExporterUnavailable)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_millis(250))
            .timeout(EXPORT_TIMEOUT)
            .build()
            .map_err(|_| TelemetryConfigError::ExporterUnavailable)?;
        Ok(Self { client, runtime })
    }
}

#[async_trait]
impl HttpClient for BoundedHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        self.runtime
            .block_on(async {
                tokio::time::timeout(EXPORT_TIMEOUT, self.client.send_bytes(request)).await
            })
            .map_err(|error| Box::new(error) as HttpError)?
    }
}
