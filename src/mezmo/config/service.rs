use std::collections::HashMap;

use hyper::Body;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use url::Url;
use vector_core::{
    config::proxy::ProxyConfig,
    tls::{TlsConfig, TlsSettings},
};

use crate::{built_info, http::HttpClient};

use super::{MezmoPartitionConfig, PipelineId, Revision, RevisionId};

#[async_trait::async_trait]
pub(crate) trait ConfigService: Send + Sync {
    async fn get_pipelines_by_partition(&self) -> Result<(Vec<PipelineId>, String), String>;

    /// Given a list of current revisions, it returns the new revision configuration (if any).
    async fn get_new_revisions(
        &self,
        current_revisions: Vec<(PipelineId, Option<RevisionId>)>,
    ) -> Result<HashMap<PipelineId, Revision>, String>;
}

pub(crate) struct DefaultConfigService {
    http_client: HttpClient,
    latest_revisions_url: Url,
    pipelines_by_partition_url: Url,
    headers: IndexMap<String, String>,
}

impl DefaultConfigService {
    pub(crate) fn new(partition_config: &MezmoPartitionConfig) -> Self {
        let tls_settings = TlsSettings::from_options(&Some(TlsConfig::default())).unwrap();
        let http_client = HttpClient::<Body>::new(tls_settings, &ProxyConfig::default())
            .expect("Invalid TLS settings");

        let mut pipelines_by_partition_url = Url::parse(
            &partition_config
                .pipelines_by_partition_url
                .replace("{partition_id}", &partition_config.partition_id),
        )
        .expect("a valid pipeline by partition url");
        pipelines_by_partition_url.set_query(Some(
            format!("vector_version={}", built_info::PKG_VERSION).as_str(),
        ));
        let mut latest_revisions_url = Url::parse(&partition_config.latest_revisions_url)
            .expect("a valid pipeline by partition url");
        latest_revisions_url.set_query(Some(
            format!("vector_version={}", built_info::PKG_VERSION).as_str(),
        ));

        Self {
            http_client,
            latest_revisions_url,
            pipelines_by_partition_url,
            headers: partition_config.request.clone().headers,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PipelinesByPartitionResponse {
    pipeline_ids: Vec<PipelineId>,
    common_config_toml: String,
}

#[derive(Serialize, Deserialize)]
struct LatestRevisionsRequest {
    revisions: Vec<LatestRevisionRequestItem>,
}

#[derive(Serialize, Deserialize)]
struct LatestRevisionRequestItem {
    pipeline_id: PipelineId,
    revision_id: Option<RevisionId>,
}

#[async_trait::async_trait]
impl ConfigService for DefaultConfigService {
    /// Gets all the pipelines composing the partition
    async fn get_pipelines_by_partition(&self) -> Result<(Vec<PipelineId>, String), String> {
        let body = http_request(
            &self.http_client,
            &self.pipelines_by_partition_url,
            &self.headers,
            None,
        )
        .await?;

        let r: PipelinesByPartitionResponse =
            serde_json::from_slice(&body).map_err(|e| e.to_string())?;

        Ok((r.pipeline_ids, r.common_config_toml))
    }

    /// Given a list of current revisions, it returns the new revision configuration (if any).
    async fn get_new_revisions(
        &self,
        current_revisions: Vec<(PipelineId, Option<RevisionId>)>,
    ) -> Result<HashMap<PipelineId, Revision>, String> {
        let revisions = current_revisions
            .into_iter()
            .map(|(pipeline_id, revision_id)| LatestRevisionRequestItem {
                pipeline_id,
                revision_id,
            })
            .collect();
        let body =
            serde_json::to_vec(&LatestRevisionsRequest { revisions }).map_err(|e| e.to_string())?;

        let response_body = http_request(
            &self.http_client,
            &self.latest_revisions_url,
            &self.headers,
            Some(body.into()),
        )
        .await?;

        let revisions: HashMap<PipelineId, Revision> =
            serde_json::from_slice(&response_body).map_err(|e| e.to_string())?;

        Ok(adapt_revisions(revisions))
    }
}

fn adapt_revisions(mut revisions: HashMap<PipelineId, Revision>) -> HashMap<PipelineId, Revision> {
    let mut to_remove = Vec::new();
    for (k, r) in revisions.iter() {
        // Non-durable pipelines still exist in the DB
        // Leave them out manually.
        if r.config.contains("sources = { }") {
            to_remove.push(k.clone());
            continue;
        }
    }

    for k in to_remove {
        revisions.remove(&k);
    }

    revisions
}

/// Makes an HTTP request to the provided endpoint, returning the String body.
async fn http_request(
    http_client: &HttpClient,
    url: &Url,
    headers: &IndexMap<String, String>,
    body: Option<Body>,
) -> Result<bytes::Bytes, String> {
    let mut builder = http::request::Builder::new().uri(url.as_str());

    if body.is_some() {
        builder = builder.method("POST");
        builder = builder.header("Content-Type", "application/json");
    }

    // Augment with headers. These may be required e.g. for authentication to private endpoints.
    for (header, value) in headers.iter() {
        builder = builder.header(header.as_str(), value.as_str());
    }

    let request = builder
        .body(body.unwrap_or_else(Body::empty))
        .map_err(|_| "Couldn't create HTTP request".to_string())?;

    info!(
        message = "Attempting to retrieve configuration.",
        url = ?url.as_str()
    );

    let response = http_client.send(request).await.map_err(|err| {
        let message = "HTTP error";
        error!(
            message = ?message,
            error = ?err,
            url = ?url.as_str());

        format!("{message}. Error: {err:?}")
    })?;

    info!(message = "Response received.", url = ?url.as_str(), status_code = ?response.status());

    let status = response.status();
    let body = hyper::body::to_bytes(response.into_body())
        .await
        .map_err(|err| {
            let message = "Error interpreting response.";
            let cause = err.into_cause();
            error!(
                    message = ?message,
                    error = ?cause);

            format!("{message} Error: {cause:?}")
        })?;

    if !status.is_success() {
        let text = String::from_utf8(body.into_iter().collect()).unwrap_or_default();
        return Err(format!(
            "Request resulted in {} error: {}",
            status.as_u16(),
            text
        ));
    }

    Ok(body)
}
