use super::{DedupeKey, Incident, IncidentId, IncidentStore};
use crate::alert::{Alert, AlertStatus};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Decision {
    Opened { incident: IncidentId },
    Attached { incident: IncidentId },
    Updated { incident: IncidentId },
    Closed { incident: IncidentId },
    DroppedResolve,
}

pub fn decide(store: &mut impl IncidentStore, alert: Alert, now: DateTime<Utc>) -> Decision {
    if let Some(incident) = store.find_open_by_alert(alert.source, &alert.source_alert_id) {
        store.set_alert_status(
            incident,
            alert.source,
            &alert.source_alert_id,
            alert.status,
            now,
        );
        let all_resolved = alert.status == AlertStatus::Resolved
            && store
                .get(incident)
                .is_some_and(|i| i.alerts.iter().all(|a| a.status == AlertStatus::Resolved));
        if all_resolved {
            store.close(incident, super::CloseReason::Resolved, now);
            return Decision::Closed { incident };
        }
        return Decision::Updated { incident };
    }

    if alert.status == AlertStatus::Resolved {
        return Decision::DroppedResolve;
    }

    let key = DedupeKey::of(&alert);
    if let Some(incident) = store.find_open_by_key(&key) {
        store.attach_alert(incident, alert, now);
        return Decision::Attached { incident };
    }

    let incident = IncidentId::new();
    store.insert(Incident::open(incident, key, alert, now));
    Decision::Opened { incident }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{Alert, AlertStatus, test_alert};
    use crate::incident::{InMemoryStore, IncidentState};
    use chrono::Utc;

    fn firing(id: &str) -> Alert {
        let mut alert = test_alert();
        alert.source_alert_id = id.to_owned();
        alert
    }

    fn resolved(id: &str) -> Alert {
        let mut alert = firing(id);
        alert.status = AlertStatus::Resolved;
        alert
    }

    fn with_label(mut alert: Alert, key: &str, value: &str) -> Alert {
        alert.labels.insert(key.to_owned(), value.to_owned());
        alert
    }

    #[test]
    fn same_alert_twice_updates_one_incident() {
        let mut store = InMemoryStore::new();
        let opened = decide(&mut store, firing("a1"), Utc::now());
        let Decision::Opened { incident } = opened else {
            panic!("first alert must open: {opened:?}");
        };
        let second = decide(&mut store, firing("a1"), Utc::now());
        assert_eq!(second, Decision::Updated { incident });
        assert_eq!(
            store.get(incident).expect("present").alerts.len(),
            1,
            "a re-fire updates in place, never appends"
        );
    }

    #[test]
    fn same_labels_different_alert_attaches_by_fingerprint() {
        let mut store = InMemoryStore::new();
        let Decision::Opened { incident } = decide(&mut store, firing("a1"), Utc::now()) else {
            panic!("first alert must open");
        };
        let second = decide(&mut store, firing("a2"), Utc::now());
        assert_eq!(second, Decision::Attached { incident });
        assert_eq!(store.get(incident).expect("present").alerts.len(), 2);
    }

    #[test]
    fn any_label_difference_opens_a_second_incident() {
        let mut store = InMemoryStore::new();
        let first = decide(&mut store, firing("a1"), Utc::now());
        let second = decide(
            &mut store,
            with_label(firing("a2"), "env", "prod"),
            Utc::now(),
        );
        assert!(matches!(first, Decision::Opened { .. }));
        assert!(
            matches!(second, Decision::Opened { .. }),
            "near-duplicates under-dedupe by design: {second:?}"
        );
    }

    #[test]
    fn resolving_the_only_alert_closes_the_incident() {
        let mut store = InMemoryStore::new();
        let Decision::Opened { incident } = decide(&mut store, firing("a1"), Utc::now()) else {
            panic!("first alert must open");
        };
        let resolution = decide(&mut store, resolved("a1"), Utc::now());
        assert_eq!(resolution, Decision::Closed { incident });
        assert!(matches!(
            store.get(incident).expect("present").state,
            IncidentState::Closed { .. }
        ));
    }

    #[test]
    fn partial_resolve_keeps_the_incident_open() {
        let mut store = InMemoryStore::new();
        let Decision::Opened { incident } = decide(&mut store, firing("a1"), Utc::now()) else {
            panic!("first alert must open");
        };
        assert_eq!(
            decide(&mut store, firing("a2"), Utc::now()),
            Decision::Attached { incident }
        );
        let partial = decide(&mut store, resolved("a1"), Utc::now());
        assert_eq!(partial, Decision::Updated { incident });
        assert!(matches!(
            store.get(incident).expect("present").state,
            IncidentState::Open
        ));
    }

    #[test]
    fn resolve_then_refire_opens_a_new_incident() {
        let mut store = InMemoryStore::new();
        let Decision::Opened { incident: first } = decide(&mut store, firing("a1"), Utc::now())
        else {
            panic!("first alert must open");
        };
        assert_eq!(
            decide(&mut store, resolved("a1"), Utc::now()),
            Decision::Closed { incident: first }
        );
        let refire = decide(&mut store, firing("a1"), Utc::now());
        match refire {
            Decision::Opened { incident } => assert_ne!(incident, first),
            other => panic!("a refire after close is a fresh incident: {other:?}"),
        }
    }

    #[test]
    fn orphan_resolve_is_dropped_and_stores_nothing() {
        let mut store = InMemoryStore::new();
        let decision = decide(&mut store, resolved("never-seen"), Utc::now());
        assert_eq!(decision, Decision::DroppedResolve);
        assert_eq!(store.find_open_by_key(&DedupeKey::of(&test_alert())), None);
        assert_eq!(store.find_open_by_alert("grafana", "never-seen"), None);
    }

    #[test]
    fn resolve_never_matches_by_fingerprint() {
        let mut store = InMemoryStore::new();
        let Decision::Opened { incident } = decide(&mut store, firing("a1"), Utc::now()) else {
            panic!("first alert must open");
        };
        let sibling_resolve = decide(&mut store, resolved("a2"), Utc::now());
        assert_eq!(
            sibling_resolve,
            Decision::DroppedResolve,
            "a resolve for an unseen alert must not close its siblings' incident"
        );
        assert!(matches!(
            store.get(incident).expect("present").state,
            IncidentState::Open
        ));
    }

    #[test]
    fn acknowledged_behaves_like_firing_for_dedupe() {
        let mut store = InMemoryStore::new();
        let mut ack = firing("a1");
        ack.status = AlertStatus::Acknowledged;
        let decision = decide(&mut store, ack, Utc::now());
        assert!(
            matches!(decision, Decision::Opened { .. }),
            "ack-before-fire still opens: {decision:?}"
        );
    }

    #[test]
    fn ack_after_fire_updates_and_keeps_open() {
        let mut store = InMemoryStore::new();
        let Decision::Opened { incident } = decide(&mut store, firing("a1"), Utc::now()) else {
            panic!("first alert must open");
        };
        let mut ack = firing("a1");
        ack.status = AlertStatus::Acknowledged;
        assert_eq!(
            decide(&mut store, ack, Utc::now()),
            Decision::Updated { incident }
        );
        assert!(matches!(
            store.get(incident).expect("present").state,
            IncidentState::Open
        ));
    }
}
