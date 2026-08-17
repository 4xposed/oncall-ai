use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer};

use super::registry;

/// Provider blocks keyed by canonical provider name.
#[derive(Clone, Debug, Default)]
pub struct ProviderConfigs(BTreeMap<String, ProviderConfig>);

impl ProviderConfigs {
    #[must_use]
    pub(super) fn get(&self, provider: &str) -> Option<&ProviderConfig> {
        self.0.get(provider)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&str, &ProviderConfig)> {
        self.0
            .iter()
            .map(|(provider, config)| (provider.as_str(), config))
    }
}

impl<'de> Deserialize<'de> for ProviderConfigs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
        let configs = raw
            .into_iter()
            .map(|(name, value)| {
                registry::parse_provider_config(&name, value)
                    .map(|config| (name, config))
                    .map_err(serde::de::Error::custom)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self(configs))
    }
}

#[derive(Clone, Debug)]
pub(super) enum ProviderConfig {
    Keyed(KeyedConfig),
    Ollama(OllamaConfig),
    Llamafile(LlamafileConfig),
    Azure(AzureConfig),
    ChatGpt(ChatGptConfig),
    Copilot(CopilotConfig),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KeyedConfig {
    pub(super) api_key: Option<SecretSource>,
    pub(super) base_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OllamaConfig {
    pub(super) api_key: Option<SecretSource>,
    pub(super) base_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LlamafileConfig {
    pub(super) base_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AzureConfig {
    pub(super) api_key: Option<SecretSource>,
    pub(super) token: Option<SecretSource>,
    pub(super) endpoint: Option<String>,
    pub(super) api_version: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChatGptConfig {
    pub(super) base_url: Option<String>,
    pub(super) auth: Option<ChatGptAuth>,
    #[serde(default)]
    pub(super) allow_device_flow: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ChatGptAuth {
    Oauth {},
    AccessToken {
        secret: SecretSource,
        #[serde(default)]
        account_id: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CopilotConfig {
    pub(super) base_url: Option<String>,
    pub(super) auth: Option<CopilotAuth>,
    #[serde(default)]
    pub(super) allow_device_flow: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum CopilotAuth {
    Oauth {},
    ApiKey { secret: SecretSource },
    GitHubAccessToken { secret: SecretSource },
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum SecretSource {
    Env(EnvSecret),
    Value(InlineSecret),
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnvSecret {
    env: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InlineSecret {
    value: String,
}

impl fmt::Debug for SecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretSource(<redacted>)")
    }
}

pub(super) struct Secret(String);

impl Secret {
    #[cfg(test)]
    pub(super) fn expose(&self) -> &str {
        &self.0
    }

    pub(super) fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Read-only environment access used by provider resolution.
pub trait EnvironmentSource {
    /// Returns a Unicode environment value without changing process state.
    ///
    /// # Errors
    ///
    /// Returns [`EnvironmentError`] when the value is not valid Unicode.
    fn var(&self, name: &str) -> Result<Option<String>, EnvironmentError>;
}

/// The process environment used by application startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironment;

impl EnvironmentSource for ProcessEnvironment {
    fn var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
        let Some(value) = std::env::var_os(name) else {
            return Ok(None);
        };
        match value.into_string() {
            Ok(value) => Ok(Some(value)),
            Err(_) => Err(EnvironmentError {
                name: name.to_owned(),
            }),
        }
    }
}

/// A non-Unicode provider environment value.
#[derive(Debug, thiserror::Error)]
#[error("environment variable {name:?} is not valid Unicode")]
pub struct EnvironmentError {
    name: String,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SettingsError {
    #[error(transparent)]
    Environment(#[from] EnvironmentError),
    #[error("provider {provider} is missing required secret {key}; checked {environment}")]
    MissingSecret {
        provider: String,
        key: String,
        environment: String,
    },
    #[error("provider {provider} has an empty secret for {key}")]
    EmptySecret { provider: String, key: String },
    #[error("provider {provider} has an empty environment variable name for {key}")]
    EmptyEnvironmentName { provider: String, key: String },
    #[error("provider {provider} is missing required setting {key}; checked {environment}")]
    MissingValue {
        provider: String,
        key: String,
        environment: String,
    },
    #[error("provider {provider} has an empty required setting {key}")]
    EmptyValue { provider: String, key: String },
    #[error("provider {provider} must configure exactly one of {left} or {right}")]
    ConflictingValues {
        provider: String,
        left: String,
        right: String,
    },
    #[error("{key} must be an absolute http(s) URL; got {value:?}")]
    InvalidUrl { key: String, value: String },
}

pub(super) fn resolve_required_secret(
    provider: &str,
    key: &str,
    explicit: Option<&SecretSource>,
    conventional_environment: &[&str],
    environment: &impl EnvironmentSource,
) -> Result<Secret, SettingsError> {
    if let Some(source) = explicit {
        return resolve_explicit_secret(provider, key, source, environment);
    }
    for name in conventional_environment {
        if let Some(value) = environment.var(name)? {
            return nonempty_secret(provider, key, value);
        }
    }
    Err(SettingsError::MissingSecret {
        provider: provider.to_owned(),
        key: key.to_owned(),
        environment: conventional_environment.join(", "),
    })
}

pub(super) fn resolve_optional_secret(
    provider: &str,
    key: &str,
    explicit: Option<&SecretSource>,
    conventional_environment: &[&str],
    environment: &impl EnvironmentSource,
) -> Result<Option<Secret>, SettingsError> {
    if let Some(source) = explicit {
        return resolve_explicit_secret(provider, key, source, environment).map(Some);
    }
    for name in conventional_environment {
        if let Some(value) = environment.var(name)? {
            return nonempty_secret(provider, key, value).map(Some);
        }
    }
    Ok(None)
}

fn resolve_explicit_secret(
    provider: &str,
    key: &str,
    source: &SecretSource,
    environment: &impl EnvironmentSource,
) -> Result<Secret, SettingsError> {
    match source {
        SecretSource::Env(source) => {
            if source.env.is_empty() {
                return Err(SettingsError::EmptyEnvironmentName {
                    provider: provider.to_owned(),
                    key: key.to_owned(),
                });
            }
            let value =
                environment
                    .var(&source.env)?
                    .ok_or_else(|| SettingsError::MissingSecret {
                        provider: provider.to_owned(),
                        key: key.to_owned(),
                        environment: source.env.clone(),
                    })?;
            nonempty_secret(provider, key, value)
        }
        SecretSource::Value(source) => {
            tracing::warn!(
                provider,
                config_key = key,
                "inline provider secret configured"
            );
            nonempty_secret(provider, key, source.value.clone())
        }
    }
}

fn nonempty_secret(provider: &str, key: &str, value: String) -> Result<Secret, SettingsError> {
    if value.is_empty() {
        Err(SettingsError::EmptySecret {
            provider: provider.to_owned(),
            key: key.to_owned(),
        })
    } else {
        Ok(Secret(value))
    }
}

pub(super) fn resolve_optional_value(
    explicit: Option<&str>,
    conventional_environment: &[&str],
    environment: &impl EnvironmentSource,
) -> Result<Option<String>, SettingsError> {
    if let Some(value) = explicit {
        return Ok(Some(value.to_owned()));
    }
    for name in conventional_environment {
        if let Some(value) = environment.var(name)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

pub(super) fn resolve_required_value(
    provider: &str,
    key: &str,
    explicit: Option<&str>,
    conventional_environment: &[&str],
    environment: &impl EnvironmentSource,
) -> Result<String, SettingsError> {
    let value = resolve_optional_value(explicit, conventional_environment, environment)?
        .ok_or_else(|| SettingsError::MissingValue {
            provider: provider.to_owned(),
            key: key.to_owned(),
            environment: conventional_environment.join(", "),
        })?;
    if value.trim().is_empty() {
        Err(SettingsError::EmptyValue {
            provider: provider.to_owned(),
            key: key.to_owned(),
        })
    } else {
        Ok(value)
    }
}

pub(super) fn validate_http_url(key: &str, value: &str) -> Result<(), SettingsError> {
    let valid = value.parse::<http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && uri.host().is_some()
    });
    if valid {
        Ok(())
    } else {
        Err(SettingsError::InvalidUrl {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct TestEnvironment(BTreeMap<String, String>);

    impl TestEnvironment {
        fn with(name: &str, value: &str) -> Self {
            Self(BTreeMap::from([(name.to_owned(), value.to_owned())]))
        }
    }

    impl EnvironmentSource for TestEnvironment {
        fn var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[test]
    fn inline_secret_debug_is_redacted() {
        let source: SecretSource = serde_json::from_value(serde_json::json!({
            "value": "top-secret"
        }))
        .expect("inline secret parses");

        let debug = format!("{source:?}");

        assert_eq!(debug, "SecretSource(<redacted>)");
        assert!(!debug.contains("top-secret"));
    }

    #[test]
    fn explicit_secret_wins_over_conventional_environment() {
        let source: SecretSource = serde_json::from_value(serde_json::json!({
            "value": "from-config"
        }))
        .expect("inline secret parses");
        let env = TestEnvironment::with("OPENAI_API_KEY", "from-env");

        let secret = resolve_required_secret(
            "openai",
            "api_key",
            Some(&source),
            &["OPENAI_API_KEY"],
            &env,
        )
        .expect("secret resolves");

        assert_eq!(secret.expose(), "from-config");
    }

    #[test]
    fn named_secret_environment_is_resolved_before_conventional_environment() {
        let source: SecretSource = serde_json::from_value(serde_json::json!({
            "env": "PROJECT_OPENAI_KEY"
        }))
        .expect("environment secret parses");
        let env = TestEnvironment(BTreeMap::from([
            ("PROJECT_OPENAI_KEY".to_owned(), "project-key".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "conventional-key".to_owned()),
        ]));

        let secret = resolve_required_secret(
            "openai",
            "api_key",
            Some(&source),
            &["OPENAI_API_KEY"],
            &env,
        )
        .expect("secret resolves");

        assert_eq!(secret.expose(), "project-key");
    }

    #[test]
    fn empty_secret_is_rejected_without_revealing_it() {
        let source: SecretSource = serde_json::from_value(serde_json::json!({
            "value": ""
        }))
        .expect("inline secret shape parses");

        let error = resolve_required_secret(
            "openai",
            "api_key",
            Some(&source),
            &["OPENAI_API_KEY"],
            &TestEnvironment::default(),
        )
        .expect_err("empty secret must fail");

        assert_eq!(
            error.to_string(),
            "provider openai has an empty secret for api_key"
        );
    }

    #[test]
    fn provider_profile_rejects_unknown_fields() {
        let error = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
            "openai": {
                "api_key": {"value": "secret"},
                "endpoint": "https://wrong.example"
            }
        }))
        .expect_err("keyed profile must reject endpoint");

        assert!(error.to_string().contains("endpoint"));
    }

    #[test]
    fn provider_name_selects_its_strict_profile() {
        let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
            "openai": {"api_key": {"value": "secret"}},
            "ollama": {"base_url": "http://127.0.0.1:11434"},
            "llamafile": {"base_url": "http://127.0.0.1:8080"},
            "azure": {
                "api_key": {"value": "secret"},
                "endpoint": "https://example.openai.azure.com",
                "api_version": "2026-01-01"
            },
            "chatgpt": {"auth": {"type": "oauth"}},
            "copilot": {"auth": {"type": "oauth"}}
        }))
        .expect("all six profiles parse");

        assert!(matches!(
            providers.get("openai"),
            Some(ProviderConfig::Keyed(_))
        ));
        assert!(matches!(
            providers.get("ollama"),
            Some(ProviderConfig::Ollama(_))
        ));
        assert!(matches!(
            providers.get("llamafile"),
            Some(ProviderConfig::Llamafile(_))
        ));
        assert!(matches!(
            providers.get("azure"),
            Some(ProviderConfig::Azure(_))
        ));
        assert!(matches!(
            providers.get("chatgpt"),
            Some(ProviderConfig::ChatGpt(_))
        ));
        assert!(matches!(
            providers.get("copilot"),
            Some(ProviderConfig::Copilot(_))
        ));
    }

    #[test]
    fn auth_profiles_reject_fields_from_other_variants() {
        let error = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
            "chatgpt": {
                "auth": {
                    "type": "oauth",
                    "secret": {"value": "must-not-be-accepted"}
                }
            }
        }))
        .expect_err("oauth must not carry a secret");

        assert!(error.to_string().contains("secret"));
    }

    #[test]
    fn invalid_provider_url_is_rejected_locally() {
        let error = validate_http_url("providers.openai.base_url", "localhost:8080")
            .expect_err("relative URL must fail");

        assert!(error.to_string().contains("providers.openai.base_url"));
    }
}
