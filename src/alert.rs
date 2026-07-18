use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// A normalized alert from an [`AlertSource`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Alert {
    pub source: &'static str,
    pub source_alert_id: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub starts_at: DateTime<Utc>,
    pub status: AlertStatus,
    pub raw_payload: serde_json::Value,
}

/// The lifecycle state of an [`Alert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertStatus {
    Firing,
    Acknowledged,
    Resolved,
}

impl AlertStatus {
    /// The lowercase wire form; `Display` and `Serialize` both go through this.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AlertStatus::Firing => "firing",
            AlertStatus::Acknowledged => "acknowledged",
            AlertStatus::Resolved => "resolved",
        }
    }
}

impl std::fmt::Display for AlertStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for AlertStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
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

/// A minimal firing alert for tests.
#[cfg(test)]
pub(crate) fn test_alert() -> Alert {
    Alert {
        source: "grafana",
        source_alert_id: "test-alert-1".to_owned(),
        labels: BTreeMap::from([("alertname".to_owned(), "CheckoutDbLatency".to_owned())]),
        annotations: BTreeMap::new(),
        starts_at: chrono::DateTime::UNIX_EPOCH,
        status: AlertStatus::Firing,
        raw_payload: serde_json::json!({}),
    }
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
