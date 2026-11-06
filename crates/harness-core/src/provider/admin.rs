//! Model management over LM Studio's native API (SPEC §5.2).

use super::{Endpoint, ProviderError};
use serde::Deserialize;

/// Non-LLM entries such as embedding models can omit fields, hence the defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub key: String,
    #[serde(default)]
    pub max_context_length: u32,
    #[serde(default)]
    pub loaded_instances: Vec<LoadedInstance>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoadedInstance {
    pub id: String,
    pub config: InstanceConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceConfig {
    pub context_length: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub trained_for_tool_use: bool,
}

#[derive(Deserialize)]
struct ModelList {
    models: Vec<ModelInfo>,
}

pub(super) struct AdminClient {
    endpoint: Endpoint,
}

impl AdminClient {
    pub(super) fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub(super) async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = self.endpoint.url("api/v1/models");
        let response = self.endpoint.send(self.endpoint.http.get(url)).await?;
        let body = response.text().await.map_err(ProviderError::Transport)?;
        let list: ModelList =
            serde_json::from_str(&body).map_err(|e| ProviderError::Malformed {
                detail: e.to_string(),
            })?;
        Ok(list.models)
    }
}
