use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use rig_core::client::Nothing;
use rig_core::providers::{azure, chatgpt, copilot, llamafile, ollama};

use super::AnyCompletionClient;
use super::settings::{
    AzureConfig, ChatGptAuth, ChatGptConfig, CopilotAuth, CopilotConfig, EnvironmentSource,
    KeyedConfig, LlamafileConfig, OllamaConfig, ProviderConfig, SettingsError,
    resolve_optional_secret, resolve_optional_value, resolve_required_secret,
    resolve_required_value, validate_http_url,
};
use super::spec::ProviderId;

macro_rules! provider_registry {
    ($callback:ident) => {
        $callback! {
            ("anthropic", keyed, anthropic, ["ANTHROPIC_API_KEY"], ["ANTHROPIC_BASE_URL"]),
            ("azure", azure, azure, [], []),
            ("chatgpt", chatgpt, chatgpt, [], []),
            ("cohere", keyed, cohere, ["COHERE_API_KEY"], []),
            ("copilot", copilot, copilot, [], []),
            ("deepseek", keyed, deepseek, ["DEEPSEEK_API_KEY"], []),
            ("doubleword", keyed, doubleword, ["DOUBLEWORD_API_KEY"], ["DOUBLEWORD_BASE_URL"]),
            ("gemini", keyed, gemini, ["GEMINI_API_KEY"], []),
            ("groq", keyed, groq, ["GROQ_API_KEY"], []),
            ("huggingface", keyed, huggingface, ["HUGGINGFACE_API_KEY"], []),
            ("hyperbolic", keyed, hyperbolic, ["HYPERBOLIC_API_KEY"], []),
            ("llamafile", llamafile, llamafile, [], ["LLAMAFILE_API_BASE_URL"]),
            ("minimax", keyed, minimax, ["MINIMAX_API_KEY"], ["MINIMAX_API_BASE"]),
            ("mira", keyed, mira, ["MIRA_API_KEY"], []),
            ("mistral", keyed, mistral, ["MISTRAL_API_KEY"], []),
            ("moonshot", keyed, moonshot, ["MOONSHOT_API_KEY"], ["MOONSHOT_API_BASE"]),
            ("ollama", ollama, ollama, ["OLLAMA_API_KEY"], ["OLLAMA_API_BASE_URL"]),
            ("openai", keyed, openai, ["OPENAI_API_KEY"], ["OPENAI_BASE_URL"]),
            ("openrouter", keyed, openrouter, ["OPENROUTER_API_KEY"], []),
            ("perplexity", keyed, perplexity, ["PERPLEXITY_API_KEY"], []),
            ("together", keyed, together, ["TOGETHER_API_KEY"], []),
            ("xai", keyed, xai, ["XAI_API_KEY"], []),
            ("xiaomimimo", keyed, xiaomimimo, ["XIAOMI_MIMO_API_KEY"], ["XIAOMI_MIMO_API_BASE"]),
            ("zai", keyed, zai, ["ZAI_API_KEY"], ["ZAI_API_BASE"]),
        }
    };
}

macro_rules! define_provider_names {
    ($(($name:literal, $profile:ident, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*]),)*) => {
        const PROVIDER_NAMES: &[&str] = &[$($name),*];
    };
}

provider_registry!(define_provider_names);

macro_rules! parse_profile {
    (keyed, $value:expr) => {
        serde_json::from_value::<KeyedConfig>($value).map(ProviderConfig::Keyed)
    };
    (ollama, $value:expr) => {
        serde_json::from_value::<OllamaConfig>($value).map(ProviderConfig::Ollama)
    };
    (llamafile, $value:expr) => {
        serde_json::from_value::<LlamafileConfig>($value).map(ProviderConfig::Llamafile)
    };
    (azure, $value:expr) => {
        serde_json::from_value::<AzureConfig>($value).map(ProviderConfig::Azure)
    };
    (chatgpt, $value:expr) => {
        serde_json::from_value::<ChatGptConfig>($value).map(ProviderConfig::ChatGpt)
    };
    (copilot, $value:expr) => {
        serde_json::from_value::<CopilotConfig>($value).map(ProviderConfig::Copilot)
    };
}

macro_rules! define_provider_parser {
    ($(($name:literal, $profile:ident, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*]),)*) => {
        pub(super) fn parse_provider_config(
            name: &str,
            value: serde_json::Value,
        ) -> Result<ProviderConfig, String> {
            match name {
                $($name => parse_profile!($profile, value),)*
                unknown => return Err(super::spec::ModelSpecError::UnknownProvider {
                    provider: unknown.to_owned(),
                }
                .to_string()),
            }
            .map_err(|source| format!("invalid provider block {name:?}: {source}"))
        }
    };
}

provider_registry!(define_provider_parser);

/// A local provider configuration or Rig client-construction error.
#[derive(Debug)]
pub struct ProviderError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProviderError {
    pub(super) fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    fn with_source(message: impl Into<String>, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    fn client(provider: &str, source: rig_core::http_client::Error) -> Self {
        Self::with_source(format!("failed to build provider {provider}"), source)
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

impl From<SettingsError> for ProviderError {
    fn from(source: SettingsError) -> Self {
        Self::with_source(source.to_string(), source)
    }
}

fn checked_url(key: &str, value: Option<String>) -> Result<Option<String>, ProviderError> {
    if let Some(value) = value {
        validate_http_url(key, &value)?;
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

fn profile_mismatch(provider: &str) -> ProviderError {
    ProviderError::message(format!(
        "provider {provider} resolved to the wrong configuration profile"
    ))
}

macro_rules! selected_config {
    (keyed, $provider:expr, $config:expr) => {
        match $config {
            Some(ProviderConfig::Keyed(config)) => config.clone(),
            None => KeyedConfig::default(),
            Some(_) => return Err(profile_mismatch($provider)),
        }
    };
    (ollama, $provider:expr, $config:expr) => {
        match $config {
            Some(ProviderConfig::Ollama(config)) => config.clone(),
            None => OllamaConfig::default(),
            Some(_) => return Err(profile_mismatch($provider)),
        }
    };
    (llamafile, $provider:expr, $config:expr) => {
        match $config {
            Some(ProviderConfig::Llamafile(config)) => config.clone(),
            None => LlamafileConfig::default(),
            Some(_) => return Err(profile_mismatch($provider)),
        }
    };
    (azure, $provider:expr, $config:expr) => {
        match $config {
            Some(ProviderConfig::Azure(config)) => Cow::Borrowed(config),
            None => Cow::Owned(AzureConfig::default()),
            Some(_) => return Err(profile_mismatch($provider)),
        }
    };
    (chatgpt, $provider:expr, $config:expr) => {
        match $config {
            Some(ProviderConfig::ChatGpt(config)) => config.clone(),
            None => ChatGptConfig::default(),
            Some(_) => return Err(profile_mismatch($provider)),
        }
    };
    (copilot, $provider:expr, $config:expr) => {
        match $config {
            Some(ProviderConfig::Copilot(config)) => config.clone(),
            None => CopilotConfig::default(),
            Some(_) => return Err(profile_mismatch($provider)),
        }
    };
}

macro_rules! build_profile {
    (keyed, $name:expr, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*], $config:expr, $environment:expr) => {{
        let config = selected_config!(keyed, $name, $config);
        let api_key = resolve_required_secret(
            $name,
            "api_key",
            config.api_key.as_ref(),
            &[$($key_env),*],
            $environment,
        )?;
        let base_url = checked_url(
            &format!("providers.{}.base_url", $name),
            resolve_optional_value(config.base_url.as_deref(), &[$($base_env),*], $environment)?,
        )?;
        let builder = rig_core::providers::$module::Client::builder().api_key(api_key.into_inner());
        let builder = if let Some(base_url) = base_url {
            builder.base_url(base_url)
        } else {
            builder
        };
        let client = builder.build().map_err(|source| ProviderError::client($name, source))?;
        Ok(AnyCompletionClient::new(client))
    }};
    (ollama, $name:expr, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*], $config:expr, $environment:expr) => {{
        let config = selected_config!(ollama, $name, $config);
        let api_key = resolve_optional_secret(
            $name,
            "api_key",
            config.api_key.as_ref(),
            &[$($key_env),*],
            $environment,
        )?;
        let base_url = checked_url(
            "providers.ollama.base_url",
            resolve_optional_value(config.base_url.as_deref(), &[$($base_env),*], $environment)?,
        )?;
        let builder = ollama::Client::builder().api_key(
            api_key.map_or_else(String::new, |secret| secret.into_inner()),
        );
        let builder = if let Some(base_url) = base_url {
            builder.base_url(base_url)
        } else {
            builder
        };
        let client = builder.build().map_err(|source| ProviderError::client($name, source))?;
        Ok(AnyCompletionClient::new(client))
    }};
    (llamafile, $name:expr, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*], $config:expr, $environment:expr) => {{
        let config = selected_config!(llamafile, $name, $config);
        let base_url = resolve_required_value(
            $name,
            "base_url",
            config.base_url.as_deref(),
            &[$($base_env),*],
            $environment,
        )?;
        validate_http_url("providers.llamafile.base_url", &base_url)?;
        let client = llamafile::Client::builder()
            .api_key(Nothing)
            .base_url(base_url)
            .build()
            .map_err(|source| ProviderError::client($name, source))?;
        Ok(AnyCompletionClient::new(client))
    }};
    (azure, $name:expr, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*], $config:expr, $environment:expr) => {{
        let config = selected_config!(azure, $name, $config);
        build_azure($name, config.as_ref(), $environment)
    }};
    (chatgpt, $name:expr, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*], $config:expr, $environment:expr) => {{
        build_chatgpt($name, selected_config!(chatgpt, $name, $config), $environment)
    }};
    (copilot, $name:expr, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*], $config:expr, $environment:expr) => {{
        build_copilot($name, selected_config!(copilot, $name, $config), $environment)
    }};
}

macro_rules! define_client_builder {
    ($(($name:literal, $profile:ident, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*]),)*) => {
        pub(super) fn build_client(
            provider: &ProviderId,
            config: Option<&ProviderConfig>,
            environment: &impl EnvironmentSource,
        ) -> Result<AnyCompletionClient, ProviderError> {
            match provider.as_str() {
                $($name => build_profile!(
                    $profile,
                    $name,
                    $module,
                    [$($key_env),*],
                    [$($base_env),*],
                    config,
                    environment
                ),)*
                unknown => Err(ProviderError::message(format!(
                    "unknown completion provider {unknown:?}"
                ))),
            }
        }
    };
}

provider_registry!(define_client_builder);

fn build_azure(
    provider: &str,
    config: &AzureConfig,
    environment: &impl EnvironmentSource,
) -> Result<AnyCompletionClient, ProviderError> {
    let (api_key, token) = match (config.api_key.as_ref(), config.token.as_ref()) {
        (Some(_), Some(_)) => (None, None),
        (Some(api_key), None) => (
            Some(resolve_required_secret(
                provider,
                "api_key",
                Some(api_key),
                &[],
                environment,
            )?),
            None,
        ),
        (None, Some(token)) => (
            None,
            Some(resolve_required_secret(
                provider,
                "token",
                Some(token),
                &[],
                environment,
            )?),
        ),
        (None, None) => (
            resolve_optional_secret(provider, "api_key", None, &["AZURE_API_KEY"], environment)?,
            resolve_optional_secret(provider, "token", None, &["AZURE_TOKEN"], environment)?,
        ),
    };
    let auth = match (api_key, token) {
        (Some(api_key), None) => azure::AzureOpenAIAuth::ApiKey(api_key.into_inner()),
        (None, Some(token)) => azure::AzureOpenAIAuth::Token(token.into_inner()),
        _ => {
            return Err(SettingsError::ConflictingValues {
                provider: provider.to_owned(),
                left: "api_key".to_owned(),
                right: "token".to_owned(),
            }
            .into());
        }
    };
    let endpoint = resolve_required_value(
        provider,
        "endpoint",
        config.endpoint.as_deref(),
        &["AZURE_ENDPOINT"],
        environment,
    )?;
    validate_http_url("providers.azure.endpoint", &endpoint)?;
    let api_version = resolve_required_value(
        provider,
        "api_version",
        config.api_version.as_deref(),
        &["AZURE_API_VERSION"],
        environment,
    )?;
    let client = azure::Client::builder()
        .api_key(auth)
        .azure_endpoint(endpoint)
        .api_version(&api_version)
        .build()
        .map_err(|source| ProviderError::client(provider, source))?;
    Ok(AnyCompletionClient::new(client))
}

fn build_chatgpt(
    provider: &str,
    config: ChatGptConfig,
    environment: &impl EnvironmentSource,
) -> Result<AnyCompletionClient, ProviderError> {
    let base_url = checked_url(
        "providers.chatgpt.base_url",
        resolve_optional_value(
            config.base_url.as_deref(),
            &["CHATGPT_API_BASE", "OPENAI_CHATGPT_API_BASE"],
            environment,
        )?,
    )?;
    let auth = match config.auth {
        Some(ChatGptAuth::Oauth {}) => chatgpt::ChatGPTAuth::OAuth,
        Some(ChatGptAuth::AccessToken { secret, account_id }) => {
            chatgpt::ChatGPTAuth::AccessToken {
                access_token: resolve_required_secret(
                    provider,
                    "auth.secret",
                    Some(&secret),
                    &[],
                    environment,
                )?
                .into_inner(),
                account_id: resolve_optional_value(
                    account_id.as_deref(),
                    &["CHATGPT_ACCOUNT_ID"],
                    environment,
                )?,
            }
        }
        None => match resolve_optional_secret(
            provider,
            "auth.secret",
            None,
            &["CHATGPT_ACCESS_TOKEN"],
            environment,
        )? {
            Some(secret) => chatgpt::ChatGPTAuth::AccessToken {
                access_token: secret.into_inner(),
                account_id: resolve_optional_value(None, &["CHATGPT_ACCOUNT_ID"], environment)?,
            },
            None => chatgpt::ChatGPTAuth::OAuth,
        },
    };
    let builder = chatgpt::Client::builder()
        .api_key(auth)
        .allow_device_flow(config.allow_device_flow);
    let builder = if let Some(base_url) = base_url {
        builder.base_url(base_url)
    } else {
        builder
    };
    let client = builder
        .build()
        .map_err(|source| ProviderError::client(provider, source))?;
    Ok(AnyCompletionClient::new(client))
}

fn first_secret(
    provider: &str,
    key: &str,
    names: &[&str],
    environment: &impl EnvironmentSource,
) -> Result<Option<super::settings::Secret>, ProviderError> {
    resolve_optional_secret(provider, key, None, names, environment).map_err(Into::into)
}

fn build_copilot(
    provider: &str,
    config: CopilotConfig,
    environment: &impl EnvironmentSource,
) -> Result<AnyCompletionClient, ProviderError> {
    let base_url = checked_url(
        "providers.copilot.base_url",
        resolve_optional_value(
            config.base_url.as_deref(),
            &["GITHUB_COPILOT_API_BASE", "COPILOT_BASE_URL"],
            environment,
        )?,
    )?;
    let auth = match config.auth {
        Some(CopilotAuth::Oauth {}) => copilot::CopilotAuth::OAuth,
        Some(CopilotAuth::ApiKey { secret }) => copilot::CopilotAuth::ApiKey(
            resolve_required_secret(provider, "auth.secret", Some(&secret), &[], environment)?
                .into_inner(),
        ),
        Some(CopilotAuth::GitHubAccessToken { secret }) => copilot::CopilotAuth::GitHubAccessToken(
            resolve_required_secret(provider, "auth.secret", Some(&secret), &[], environment)?
                .into_inner(),
        ),
        None => {
            if let Some(secret) = first_secret(
                provider,
                "auth.api_key",
                &["GITHUB_COPILOT_API_KEY", "COPILOT_API_KEY"],
                environment,
            )? {
                copilot::CopilotAuth::ApiKey(secret.into_inner())
            } else if let Some(secret) = first_secret(
                provider,
                "auth.github_access_token",
                &["COPILOT_GITHUB_ACCESS_TOKEN", "GITHUB_TOKEN"],
                environment,
            )? {
                copilot::CopilotAuth::GitHubAccessToken(secret.into_inner())
            } else {
                copilot::CopilotAuth::OAuth
            }
        }
    };
    let builder = copilot::Client::builder()
        .api_key(auth)
        .allow_device_flow(config.allow_device_flow);
    let builder = if let Some(base_url) = base_url {
        builder.base_url(base_url)
    } else {
        builder
    };
    let client = builder
        .build()
        .map_err(|source| ProviderError::client(provider, source))?;
    Ok(AnyCompletionClient::new(client))
}

/// Canonical completion-provider names accepted by [`crate::model::ModelSpec`].
#[must_use]
pub fn provider_names() -> &'static [&'static str] {
    PROVIDER_NAMES
}

#[must_use]
pub(super) fn is_provider(name: &str) -> bool {
    PROVIDER_NAMES.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use rig_core::client::CompletionClient;

    macro_rules! assert_registered_clients {
        ($(($name:literal, $profile:ident, $module:ident, [$($key_env:literal),*], [$($base_env:literal),*]),)*) => {
            #[test]
            fn every_registered_client_is_completion_capable_and_erasable() {
                fn assert_client<C>()
                where
                    C: CompletionClient + Send + Sync + 'static,
                    C::CompletionModel: Send + Sync + 'static,
                {
                }

                $(let _ = assert_client::<rig_core::providers::$module::Client>;)*
            }
        };
    }

    provider_registry!(assert_registered_clients);
}
