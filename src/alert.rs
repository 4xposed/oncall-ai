use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// A normalized alert from an [`AlertSource`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Alert {
    pub source: &'static str,
    /// The source's own identifier for the alert.
    pub source_alert_id: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub starts_at: DateTime<Utc>,
    pub status: AlertStatus,
    /// This alert's slice of the original webhook payload.
    pub raw_payload: serde_json::Value,
}

/// The lifecycle state of an [`Alert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertStatus {
    Firing,
    Resolved,
}

impl std::fmt::Display for AlertStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AlertStatus::Firing => "firing",
            AlertStatus::Resolved => "resolved",
        })
    }
}

/// An error from parsing a webhook body.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    message: String,
}

impl From<serde_json::Error> for ParseError {
    fn from(err: serde_json::Error) -> Self {
        Self {
            message: err.to_string(),
        }
    }
}

/// A parser for one alert source's webhook payloads.
pub trait AlertSource: Send + Sync {
    /// Returns the source name used in webhook URLs.
    fn name(&self) -> &'static str;

    /// Parses a raw webhook body into alerts.
    ///
    /// # Errors
    ///
    /// Fails on payloads this source does not understand.
    fn parse(&self, body: &[u8]) -> Result<Vec<Alert>, ParseError>;
}
