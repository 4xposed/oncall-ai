use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::registry;

/// A provider name validated against the compile-time completion registry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderId(Box<str>);

impl ProviderId {
    /// Parses a canonical completion-provider name.
    ///
    /// # Errors
    ///
    /// Returns [`ModelSpecError::UnknownProvider`] when the name is absent from the provider registry.
    pub fn parse(value: &str) -> Result<Self, ModelSpecError> {
        if registry::is_provider(value) {
            Ok(Self(value.into()))
        } else {
            Err(ModelSpecError::UnknownProvider {
                provider: value.to_owned(),
            })
        }
    }

    /// Returns the canonical provider name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A non-empty provider model identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModelId(Box<str>);

impl ModelId {
    /// Parses a provider model identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ModelSpecError::EmptyModel`] for an empty value.
    pub fn parse(value: &str) -> Result<Self, ModelSpecError> {
        if value.is_empty() {
            Err(ModelSpecError::EmptyModel)
        } else {
            Ok(Self(value.into()))
        }
    }

    /// Returns the provider model identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A completion model selected as `<provider>:<model>`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModelSpec {
    provider: ProviderId,
    model: ModelId,
}

impl ModelSpec {
    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    #[must_use]
    pub fn model(&self) -> &ModelId {
        &self.model
    }
}

/// An error from parsing a [`ModelSpec`].
#[derive(Debug, thiserror::Error)]
pub enum ModelSpecError {
    #[error("model must be \"<provider>:<model>\"; got {got:?}")]
    BadFormat { got: String },
    #[error(
        "unknown completion provider {provider:?}; supported providers: {}",
        registry::provider_names().join(", ")
    )]
    UnknownProvider { provider: String },
    #[error("model identifier is empty")]
    EmptyModel,
}

impl FromStr for ModelSpec {
    type Err = ModelSpecError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (provider, model) = value
            .split_once(':')
            .ok_or_else(|| ModelSpecError::BadFormat {
                got: value.to_owned(),
            })?;
        Ok(Self {
            provider: ProviderId::parse(provider)?,
            model: ModelId::parse(model)?,
        })
    }
}

impl fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.model)
    }
}

impl Serialize for ModelSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ModelSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
