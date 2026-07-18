use crate::alert::Alert;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

mod dedupe;
mod store;
mod worker;
pub use dedupe::{Decision, decide};
pub use store::{InMemoryStore, IncidentStore};
pub use worker::{TRIAGED_CAPACITY, TriageRequest, Triaged, worker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IncidentId(uuid::Uuid);

impl IncidentId {
    #[must_use]
    pub fn new() -> Self {
        IncidentId(uuid::Uuid::now_v7())
    }
}

impl Default for IncidentId {
    fn default() -> Self {
        IncidentId::new()
    }
}

impl std::fmt::Display for IncidentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupeKey {
    source: &'static str,
    labels: BTreeMap<String, String>,
}

impl DedupeKey {
    #[must_use]
    pub fn of(alert: &Alert) -> Self {
        DedupeKey {
            source: alert.source,
            labels: alert.labels.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncidentState {
    Open,
    Closed {
        reason: CloseReason,
        closed_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Resolved,
    IdleTtl,
}

impl CloseReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            CloseReason::Resolved => "resolved",
            CloseReason::IdleTtl => "idle_ttl",
        }
    }
}

#[derive(Debug)]
pub struct Incident {
    pub id: IncidentId,
    pub state: IncidentState,
    pub key: DedupeKey,
    pub alerts: Vec<Alert>,
    pub triage: Option<crate::triage::TriageResult>,
    pub opened_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
}

impl Incident {
    /// A fresh open incident holding its first alert.
    #[must_use]
    pub fn open(id: IncidentId, key: DedupeKey, alert: Alert, now: DateTime<Utc>) -> Self {
        Incident {
            id,
            state: IncidentState::Open,
            key,
            alerts: vec![alert],
            triage: None,
            opened_at: now,
            last_activity_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::test_alert;

    #[test]
    fn incident_ids_are_v7_and_unique() {
        let (a, b) = (IncidentId::new(), IncidentId::new());
        assert_eq!(a.0.get_version_num(), 7);
        assert_ne!(a, b);
    }

    #[test]
    fn dedupe_key_ignores_everything_but_source_and_labels() {
        let mut other = test_alert();
        other.source_alert_id = "different-id".to_owned();
        other
            .annotations
            .insert("summary".to_owned(), "different".to_owned());
        other.status = crate::alert::AlertStatus::Resolved;
        assert_eq!(DedupeKey::of(&test_alert()), DedupeKey::of(&other));
    }

    #[test]
    fn dedupe_key_separates_on_any_label_difference() {
        let mut other = test_alert();
        other.labels.insert("env".to_owned(), "prod".to_owned());
        assert_ne!(DedupeKey::of(&test_alert()), DedupeKey::of(&other));
    }

    #[test]
    fn open_starts_with_one_alert_and_no_triage() {
        let now = chrono::Utc::now();
        let alert = test_alert();
        let incident = Incident::open(IncidentId::new(), DedupeKey::of(&alert), alert, now);
        assert_eq!(incident.state, IncidentState::Open);
        assert_eq!(incident.alerts.len(), 1);
        assert!(incident.triage.is_none());
        assert_eq!(incident.opened_at, now);
        assert_eq!(incident.last_activity_at, now);
    }
}
