use crate::alert::{Alert, AlertSource, AlertStatus, ParseError};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The Grafana alert source.
pub struct Grafana;

#[derive(Deserialize)]
struct GrafanaWebhook {
    alerts: Vec<GrafanaAlert>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrafanaAlert {
    status: GrafanaStatus,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    starts_at: DateTime<Utc>,
    fingerprint: String,
}

/// Statuses Grafana is known to send.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum GrafanaStatus {
    Firing,
    Resolved,
}

impl From<GrafanaStatus> for AlertStatus {
    fn from(status: GrafanaStatus) -> Self {
        match status {
            GrafanaStatus::Firing => AlertStatus::Firing,
            GrafanaStatus::Resolved => AlertStatus::Resolved,
        }
    }
}

impl AlertSource for Grafana {
    fn name(&self) -> &'static str {
        "grafana"
    }

    fn parse(&self, body: &[u8]) -> Result<Vec<Alert>, ParseError> {
        let webhook: GrafanaWebhook = serde_json::from_slice(body)?;
        let mut raw: serde_json::Value = serde_json::from_slice(body)?;
        let raw_alerts = raw
            .get_mut("alerts")
            .and_then(serde_json::Value::as_array_mut)
            .map(std::mem::take)
            .unwrap_or_default();

        Ok(webhook
            .alerts
            .into_iter()
            .zip(raw_alerts)
            .map(|(alert, raw_payload)| Alert {
                source: self.name(),
                source_alert_id: alert.fingerprint,
                labels: alert.labels,
                annotations: alert.annotations,
                starts_at: alert.starts_at,
                status: alert.status.into(),
                raw_payload,
            })
            .collect())
    }
}
