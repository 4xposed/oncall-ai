use crate::alert::{Alert, AlertSource, AlertStatus, ParseError};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The PagerDuty alert.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pagerduty;

#[derive(Deserialize)]
struct PagerdutyWebhook {
    event: serde_json::Value,
}

/// The event's type alone, decoded first to decide whether the event maps to an alert at all.
#[derive(Deserialize)]
struct PagerdutyEventKind {
    event_type: String,
}

/// The event's incident payload.
#[derive(Deserialize)]
struct PagerdutyEventData {
    data: PagerdutyIncident,
}

/// PagerDuty's incident object.
#[derive(Deserialize)]
struct PagerdutyIncident {
    id: String,
    created_at: DateTime<Utc>,
    title: String,
    service: PagerdutyRef,
    urgency: String,
    priority: Option<PagerdutyRef>,
}

#[derive(Deserialize)]
struct PagerdutyRef {
    summary: String,
}

fn status_for(event_type: &str) -> Option<AlertStatus> {
    match event_type {
        "incident.triggered" | "incident.reopened" | "incident.unacknowledged" => {
            Some(AlertStatus::Firing)
        }
        "incident.acknowledged" => Some(AlertStatus::Acknowledged),
        "incident.resolved" => Some(AlertStatus::Resolved),
        _ => None,
    }
}

impl AlertSource for Pagerduty {
    fn name(&self) -> &'static str {
        "pagerduty"
    }

    fn parse(&self, body: &[u8]) -> Result<Vec<Alert>, ParseError> {
        let PagerdutyWebhook { event } = serde_json::from_slice(body)?;
        let kind = PagerdutyEventKind::deserialize(&event)?;
        let Some(status) = status_for(&kind.event_type) else {
            tracing::debug!(
                event_type = kind.event_type,
                "event type carries no lifecycle signal, skipped"
            );
            return Ok(Vec::new());
        };
        let PagerdutyEventData { data: pd_incident } = PagerdutyEventData::deserialize(&event)?;

        let mut labels = BTreeMap::from([
            ("service".to_string(), pd_incident.service.summary),
            ("urgency".to_string(), pd_incident.urgency),
        ]);
        if let Some(priority) = pd_incident.priority {
            labels.insert("priority".to_string(), priority.summary);
        }

        Ok(vec![Alert {
            source: self.name(),
            source_alert_id: pd_incident.id,
            labels,
            annotations: BTreeMap::from([("summary".to_string(), pd_incident.title)]),
            starts_at: pd_incident.created_at,
            status,
            raw_payload: event,
        }])
    }
}
