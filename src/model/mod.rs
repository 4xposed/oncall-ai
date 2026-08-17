//! Provider-neutral completion models and provider construction.

use std::collections::HashMap;

mod erased;
mod registry;
mod settings;
mod spec;

pub use erased::{AnyCompletionClient, AnyCompletionModel, ErasedStreamingResponse};
pub use registry::{ProviderError, provider_names};
pub use settings::{EnvironmentError, EnvironmentSource, ProcessEnvironment, ProviderConfigs};
pub use spec::{ModelId, ModelSpec, ModelSpecError, ProviderId};

/// Provider clients and models constructed eagerly from application settings.
#[derive(Debug)]
pub struct ModelRuntime {
    models: HashMap<ModelSpec, AnyCompletionModel>,
    #[cfg(test)]
    client_count: usize,
}

impl ModelRuntime {
    /// Builds every configured provider, then every referenced provider and model.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when a configured or referenced provider's
    /// settings, secrets, URL, authentication choice, or Rig client builder is invalid.
    pub fn build(
        configs: &ProviderConfigs,
        specs: &[ModelSpec],
        environment: &impl EnvironmentSource,
    ) -> Result<Self, ProviderError> {
        let mut clients = HashMap::new();
        let mut models = HashMap::new();

        for (name, config) in configs.iter() {
            let provider = ProviderId::parse(name)
                .map_err(|error| ProviderError::message(error.to_string()))?;
            let client = registry::build_client(&provider, Some(config), environment)?;
            clients.insert(provider, client);
        }

        for spec in specs {
            if !clients.contains_key(spec.provider()) {
                let client = registry::build_client(
                    spec.provider(),
                    configs.get(spec.provider().as_str()),
                    environment,
                )?;
                clients.insert(spec.provider().clone(), client);
            }

            if !models.contains_key(spec) {
                let client = clients.get(spec.provider()).ok_or_else(|| {
                    ProviderError::message(format!(
                        "provider {} was not constructed",
                        spec.provider()
                    ))
                })?;
                models.insert(spec.clone(), client.completion_model(spec.model()));
            }
        }

        Ok(Self {
            models,
            #[cfg(test)]
            client_count: clients.len(),
        })
    }

    /// Returns a cheaply cloned handle to a model validated during construction.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if `spec` was not included when the runtime was built.
    pub fn model(&self, spec: &ModelSpec) -> Result<AnyCompletionModel, ProviderError> {
        self.models.get(spec).cloned().ok_or_else(|| {
            ProviderError::message(format!("model {spec} was not constructed at startup"))
        })
    }

    #[cfg(test)]
    fn client_count(&self) -> usize {
        self.client_count
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct TestEnvironment(BTreeMap<String, String>);

    impl EnvironmentSource for TestEnvironment {
        fn var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[test]
    fn stages_sharing_a_provider_reuse_one_client() {
        let configs = serde_json::from_value(serde_json::json!({
            "ollama": {"base_url": "http://localhost:11434"}
        }))
        .expect("provider config parses");
        let specs = vec![
            "ollama:qwen3:8b".parse().expect("triage spec parses"),
            "ollama:qwen3.5:27b"
                .parse()
                .expect("investigation spec parses"),
        ];

        let runtime = ModelRuntime::build(&configs, &specs, &TestEnvironment(BTreeMap::new()))
            .expect("runtime builds");

        assert_eq!(runtime.client_count(), 1);
    }
}
