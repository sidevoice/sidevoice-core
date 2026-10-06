//! The OTLP/HTTP transport. The SDK exports from its own threads; the request
//! itself runs on the core's Tokio runtime with the core's rustls `reqwest`, so
//! HTTPS collectors work and no second HTTP stack is started.

#[derive(Clone, Debug)]
pub(super) struct Collector {
    pub(super) client: reqwest::Client,
    pub(super) runtime: tokio::runtime::Handle,
}

#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for Collector {
    async fn send_bytes(
        &self,
        request: opentelemetry_http::Request<opentelemetry_http::Bytes>,
    ) -> Result<
        opentelemetry_http::Response<opentelemetry_http::Bytes>,
        opentelemetry_http::HttpError,
    > {
        let request = reqwest::Request::try_from(request)?;
        let client = self.client.clone();
        let (status, body) = self
            .runtime
            .spawn(async move {
                let response = client.execute(request).await?.error_for_status()?;
                let status = response.status();
                Ok::<_, reqwest::Error>((status, response.bytes().await?))
            })
            .await??;
        Ok(opentelemetry_http::Response::builder()
            .status(status)
            .body(body)?)
    }
}
