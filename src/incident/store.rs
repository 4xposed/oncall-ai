use super::{CloseReason, DedupeKey, Incident, IncidentId, IncidentState};
use crate::alert::{Alert, AlertStatus};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

pub trait IncidentStore {
    fn insert(&mut self, incident: Incident);
    fn get(&self, id: IncidentId) -> Option<&Incident>;
    fn find_open_by_alert(&self, source: &str, alert_id: &str) -> Option<IncidentId>;
    fn find_open_by_key(&self, key: &DedupeKey) -> Option<IncidentId>;
    fn attach_alert(&mut self, id: IncidentId, alert: Alert, now: DateTime<Utc>);
    fn set_alert_status(
        &mut self,
        id: IncidentId,
        source: &str,
        alert_id: &str,
        status: AlertStatus,
        now: DateTime<Utc>,
    );
    fn set_triage(&mut self, id: IncidentId, triage: crate::triage::TriageResult);
    fn close(&mut self, id: IncidentId, reason: CloseReason, now: DateTime<Utc>);
    fn expire_idle_before(&mut self, cutoff: DateTime<Utc>, now: DateTime<Utc>) -> Vec<IncidentId>;
}

#[derive(Default)]
pub struct InMemoryStore {
    incidents: HashMap<IncidentId, Incident>,
    open_by_alert: HashMap<(String, String), IncidentId>,
    open_by_key: HashMap<DedupeKey, IncidentId>,
}

impl InMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        InMemoryStore::default()
    }

    fn get_mut(&mut self, id: IncidentId) -> Option<&mut Incident> {
        let incident = self.incidents.get_mut(&id);
        if incident.is_none() {
            tracing::error!(incident_id = %id, "incident store id miss (bug)");
        }
        incident
    }
}

impl IncidentStore for InMemoryStore {
    fn insert(&mut self, incident: Incident) {
        for alert in &incident.alerts {
            self.open_by_alert.insert(
                (alert.source.to_owned(), alert.source_alert_id.clone()),
                incident.id,
            );
        }
        self.open_by_key.insert(incident.key.clone(), incident.id);
        self.incidents.insert(incident.id, incident);
    }

    fn get(&self, id: IncidentId) -> Option<&Incident> {
        self.incidents.get(&id)
    }

    fn find_open_by_alert(&self, source: &str, alert_id: &str) -> Option<IncidentId> {
        self.open_by_alert
            .get(&(source.to_owned(), alert_id.to_owned()))
            .copied()
    }

    fn find_open_by_key(&self, key: &DedupeKey) -> Option<IncidentId> {
        self.open_by_key.get(key).copied()
    }

    fn attach_alert(&mut self, id: IncidentId, alert: Alert, now: DateTime<Utc>) {
        if let Some(incident) = self.get_mut(id) {
            let key = (alert.source.to_owned(), alert.source_alert_id.clone());
            incident.alerts.push(alert);
            incident.last_activity_at = now;
            self.open_by_alert.insert(key, id);
        }
    }

    fn set_alert_status(
        &mut self,
        id: IncidentId,
        source: &str,
        alert_id: &str,
        status: AlertStatus,
        now: DateTime<Utc>,
    ) {
        if let Some(incident) = self.get_mut(id) {
            if let Some(alert) = incident
                .alerts
                .iter_mut()
                .find(|a| a.source == source && a.source_alert_id == alert_id)
            {
                alert.status = status;
                incident.last_activity_at = now;
            } else {
                tracing::error!(
                    incident_id = %id,
                    source,
                    alert_id,
                    "alert not found in incident (bug)"
                );
            }
        }
    }

    fn set_triage(&mut self, id: IncidentId, triage: crate::triage::TriageResult) {
        if let Some(incident) = self.get_mut(id) {
            incident.triage = Some(triage);
        }
    }

    fn close(&mut self, id: IncidentId, reason: CloseReason, now: DateTime<Utc>) {
        if let Some(incident) = self.get_mut(id) {
            incident.state = IncidentState::Closed {
                reason,
                closed_at: now,
            };
        }
        // Borrow of self.incidents ended; prune the open indexes.
        if let Some(incident) = self.incidents.get(&id) {
            for alert in &incident.alerts {
                self.open_by_alert
                    .remove(&(alert.source.to_owned(), alert.source_alert_id.clone()));
            }
            self.open_by_key.remove(&incident.key);
        }
    }

    fn expire_idle_before(&mut self, cutoff: DateTime<Utc>, now: DateTime<Utc>) -> Vec<IncidentId> {
        let mut expired: Vec<IncidentId> = self
            .incidents
            .values()
            .filter(|i| matches!(i.state, IncidentState::Open) && i.last_activity_at < cutoff)
            .map(|i| i.id)
            .collect();
        expired.sort_unstable();
        for &id in &expired {
            self.close(id, CloseReason::IdleTtl, now);
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertStatus, test_alert};
    use chrono::{TimeDelta, Utc};

    fn open_test_incident(store: &mut InMemoryStore) -> IncidentId {
        let now = Utc::now();
        let alert = test_alert();
        let id = IncidentId::new();
        store.insert(Incident::open(id, DedupeKey::of(&alert), alert, now));
        id
    }

    #[test]
    fn insert_indexes_both_lookups() {
        let mut store = InMemoryStore::new();
        let id = open_test_incident(&mut store);
        let alert = test_alert();
        assert_eq!(
            store.find_open_by_alert(alert.source, &alert.source_alert_id),
            Some(id)
        );
        assert_eq!(store.find_open_by_key(&DedupeKey::of(&alert)), Some(id));
        assert_eq!(store.get(id).expect("inserted").id, id);
    }

    #[test]
    fn attach_indexes_the_new_alert_and_bumps_activity() {
        let mut store = InMemoryStore::new();
        let id = open_test_incident(&mut store);
        let mut second = test_alert();
        second.source_alert_id = "test-alert-2".to_owned();
        let later = Utc::now() + TimeDelta::seconds(10);
        store.attach_alert(id, second, later);

        let incident = store.get(id).expect("present");
        assert_eq!(incident.alerts.len(), 2);
        assert_eq!(incident.last_activity_at, later);
        assert_eq!(
            store.find_open_by_alert("grafana", "test-alert-2"),
            Some(id)
        );
    }

    #[test]
    fn set_alert_status_updates_in_place_and_bumps_activity() {
        let mut store = InMemoryStore::new();
        let id = open_test_incident(&mut store);
        let later = Utc::now() + TimeDelta::seconds(10);
        store.set_alert_status(id, "grafana", "test-alert-1", AlertStatus::Resolved, later);

        let incident = store.get(id).expect("present");
        assert_eq!(incident.alerts.len(), 1, "an update never appends");
        assert_eq!(incident.alerts[0].status, AlertStatus::Resolved);
        assert_eq!(incident.last_activity_at, later);
    }

    #[test]
    fn close_removes_every_open_index_entry() {
        let mut store = InMemoryStore::new();
        let id = open_test_incident(&mut store);
        let mut second = test_alert();
        second.source_alert_id = "test-alert-2".to_owned();
        store.attach_alert(id, second, Utc::now());

        store.close(id, CloseReason::Resolved, Utc::now());

        assert_eq!(store.find_open_by_alert("grafana", "test-alert-1"), None);
        assert_eq!(store.find_open_by_alert("grafana", "test-alert-2"), None);
        assert_eq!(store.find_open_by_key(&DedupeKey::of(&test_alert())), None);
        assert!(matches!(
            store.get(id).expect("closed incidents stay readable").state,
            IncidentState::Closed {
                reason: CloseReason::Resolved,
                ..
            }
        ));
    }

    #[test]
    fn set_triage_records_the_result() {
        let mut store = InMemoryStore::new();
        let id = open_test_incident(&mut store);
        store.set_triage(
            id,
            crate::triage::TriageResult {
                severity: crate::triage::Severity::P1,
                service: Some("checkout".to_owned()),
                tags: Vec::new(),
                summary: "stub".to_owned(),
            },
        );
        assert!(store.get(id).expect("present").triage.is_some());
    }

    #[test]
    fn expire_closes_only_incidents_idle_past_the_cutoff() {
        let mut store = InMemoryStore::new();
        let stale = open_test_incident(&mut store);
        let mut fresh_alert = test_alert();
        fresh_alert.source_alert_id = "fresh".to_owned();
        fresh_alert
            .labels
            .insert("env".to_owned(), "prod".to_owned());
        let fresh = IncidentId::new();
        let now = Utc::now();
        store.insert(Incident::open(
            fresh,
            DedupeKey::of(&fresh_alert),
            fresh_alert,
            now + TimeDelta::hours(1),
        ));

        let sweep_at = now + TimeDelta::hours(2);
        let expired = store.expire_idle_before(now + TimeDelta::seconds(1), sweep_at);

        assert_eq!(expired, vec![stale]);
        assert!(matches!(
            store.get(stale).expect("present").state,
            IncidentState::Closed {
                reason: CloseReason::IdleTtl,
                closed_at,
            } if closed_at == sweep_at
        ));
        assert_eq!(store.find_open_by_alert("grafana", "test-alert-1"), None);
        assert!(matches!(
            store.get(fresh).expect("present").state,
            IncidentState::Open
        ));
    }

    #[test]
    fn expire_returns_ids_in_creation_order() {
        let mut store = InMemoryStore::new();
        let stale_since = Utc::now() - TimeDelta::hours(2);
        let first_alert = test_alert();
        let first = IncidentId::new();
        store.insert(Incident::open(
            first,
            DedupeKey::of(&first_alert),
            first_alert,
            stale_since,
        ));
        let mut second_alert = test_alert();
        second_alert.source_alert_id = "test-alert-2".to_owned();
        second_alert
            .labels
            .insert("env".to_owned(), "prod".to_owned());
        let second = IncidentId::new();
        store.insert(Incident::open(
            second,
            DedupeKey::of(&second_alert),
            second_alert,
            stale_since,
        ));

        let now = Utc::now();
        let expired = store.expire_idle_before(now - TimeDelta::hours(1), now);

        assert_eq!(expired, vec![first, second]);
    }

    #[test]
    fn mutators_on_missing_id_are_safe_no_ops() {
        let mut store = InMemoryStore::new();
        let id = IncidentId::new();
        let now = Utc::now();

        store.attach_alert(id, test_alert(), now);
        store.set_alert_status(id, "grafana", "test-alert-1", AlertStatus::Resolved, now);
        store.set_triage(
            id,
            crate::triage::TriageResult {
                severity: crate::triage::Severity::P1,
                service: None,
                tags: Vec::new(),
                summary: "stub".to_owned(),
            },
        );
        store.close(id, CloseReason::Resolved, now);

        assert!(store.get(id).is_none());
        assert_eq!(store.find_open_by_alert("grafana", "test-alert-1"), None);
        assert_eq!(store.find_open_by_key(&DedupeKey::of(&test_alert())), None);
    }
}
