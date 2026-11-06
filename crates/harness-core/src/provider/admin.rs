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

#[derive(Deserialize)]
struct LoadResponse {
    instance_id: String,
    load_config: InstanceConfig,
}

/// May `ensure_ready` load a model? Loading disrupts anything else using the server (SPEC §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPolicy {
    /// `auto_load = true`, or the user approved this load.
    Allowed,
    /// Fail with `ProviderError::LoadNeedsConsent` so the caller can ask, then retry with `Allowed`.
    AskFirst,
}

/// An instance known to have at least the requested context. Chat should address `instance_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyModel {
    pub instance_id: String,
    pub context_length: u32,
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

    pub(super) async fn ensure_ready(
        &self,
        model: &str,
        context: u32,
        policy: LoadPolicy,
    ) -> Result<ReadyModel, ProviderError> {
        let models = self.list_models().await?;
        let Some(info) = models.iter().find(|m| m.key == model) else {
            return Err(ProviderError::ModelNotFound {
                message: model.to_owned(),
            });
        };
        let loaded = info
            .loaded_instances
            .iter()
            .find(|i| i.config.context_length >= context);
        if let Some(instance) = loaded {
            return Ok(ReadyModel {
                instance_id: instance.id.clone(),
                context_length: instance.config.context_length,
            });
        }
        match policy {
            LoadPolicy::Allowed => self.load(model, context).await,
            LoadPolicy::AskFirst => Err(ProviderError::LoadNeedsConsent {
                model: model.to_owned(),
                context,
            }),
        }
    }

    async fn load(&self, model: &str, context: u32) -> Result<ReadyModel, ProviderError> {
        let url = self.endpoint.url("api/v1/models/load");
        let request = self.endpoint.http.post(url).json(&serde_json::json!({
            "model": model,
            "context_length": context,
            "echo_load_config": true,
        }));
        let response = self.endpoint.send(request).await?;
        let body = response.text().await.map_err(ProviderError::Transport)?;
        let loaded: LoadResponse =
            serde_json::from_str(&body).map_err(|e| ProviderError::Malformed {
                detail: e.to_string(),
            })?;
        let applied = loaded.load_config.context_length;
        if applied < context {
            return Err(ProviderError::Server {
                message: format!(
                    "`{model}` loaded with {applied} context instead of the requested {context}"
                ),
            });
        }
        Ok(ReadyModel {
            instance_id: loaded.instance_id,
            context_length: applied,
        })
    }
}
