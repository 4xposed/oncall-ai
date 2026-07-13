use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// A normalized alert from an [`AlertSource`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    Acknowledged,
    Resolved,
}

impl std::fmt::Display for AlertStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AlertStatus::Firing => "firing",
            AlertStatus::Acknowledged => "acknowledged",
            AlertStatus::Resolved => "resolved",
        })
    }
}

/// An error from parsing a webhook body.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ParseError(#[from] serde_json::Error);

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

#[cfg(test)]
mod tests {
    use super::AlertStatus;

    #[test]
    fn status_display_and_serde_are_stable() {
        let statuses = [
            AlertStatus::Firing,
            AlertStatus::Acknowledged,
            AlertStatus::Resolved,
        ];
        let rendered: Vec<(String, String)> = statuses
            .iter()
            .map(|status| {
                (
                    status.to_string(),
                    serde_json::to_string(status).expect("status serializes"),
                )
            })
            .collect();
        insta::assert_yaml_snapshot!(rendered);
    }
}
