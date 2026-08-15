use super::{Decision, IncidentId, IncidentState, IncidentStore, decide};
use crate::alert::Alert;
use crate::investigation::{Investigation, InvestigationRequest};
use crate::triage::{TriageRequest, TriageResult, Triaged};
use chrono::{TimeDelta, Utc};
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;

pub const TRIAGED_CAPACITY: usize = 32;
pub const INVESTIGATED_CAPACITY: usize = 32;

#[derive(Debug)]
pub struct Channels {
    pub alerts_rx: tokio::sync::mpsc::Receiver<Alert>,
    pub triaged_rx: tokio::sync::mpsc::Receiver<Triaged>,
    pub investigated_rx: tokio::sync::mpsc::Receiver<Investigation>,
    pub triage_tx: tokio::sync::mpsc::Sender<TriageRequest>,
    pub investigation_tx: tokio::sync::mpsc::Sender<InvestigationRequest>,
}

pub async fn worker(
    channels: Channels,
    mut store: impl IncidentStore,
    idle_ttl: TimeDelta,
    shutdown: CancellationToken,
) -> usize {
    let Channels {
        mut alerts_rx,
        mut triaged_rx,
        mut investigated_rx,
        triage_tx,
        investigation_tx,
    } = channels;
    let mut dropped: usize = 0;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            triaged = triaged_rx.recv() => match triaged {
                Some(triaged) => record_triage(&mut store, &investigation_tx, triaged),
                None => break,
            },
            investigated = investigated_rx.recv() => match investigated {
                Some(investigated) => record_investigation(&mut store, investigated),
                None => break,
            },
            alert = alerts_rx.recv() => match alert {
                Some(alert) => handle_alert(&mut store, &triage_tx, alert, idle_ttl),
                None => break,
            },
        }
    }
    while let Ok(triaged) = triaged_rx.try_recv() {
        record_triage(&mut store, &investigation_tx, triaged);
    }
    while let Ok(investigated) = investigated_rx.try_recv() {
        record_investigation(&mut store, investigated);
    }
    while alerts_rx.try_recv().is_ok() {
        dropped += 1;
    }
    tracing::info!(dropped, "incident inbox drained");
    dropped
}

fn handle_alert(
    store: &mut impl IncidentStore,
    triage_tx: &tokio::sync::mpsc::Sender<TriageRequest>,
    alert: Alert,
    idle_ttl: TimeDelta,
) {
    let now = Utc::now();
    for incident in store.expire_idle_before(now - idle_ttl, now) {
        tracing::info!(
            incident_id = %incident,
            decision = "closed",
            reason = super::CloseReason::IdleTtl.as_str(),
            "incident closed"
        );
    }

    let (source, source_alert_id) = (alert.source, alert.source_alert_id.clone());
    let label_count = alert.labels.len();
    let decision = decide(store, alert, now);
    log_decision(&decision, source, &source_alert_id, label_count);

    if let Decision::Opened { incident } = decision {
        request_triage(store, triage_tx, incident);
    }
}

fn log_decision(decision: &Decision, source: &str, source_alert_id: &str, label_count: usize) {
    match decision {
        Decision::Opened { incident } => tracing::info!(
            incident_id = %incident, decision = "opened", source, source_alert_id, label_count,
            "incident opened"
        ),
        Decision::Attached { incident } => tracing::info!(
            incident_id = %incident, decision = "attached", rule = "fingerprint",
            source, source_alert_id, "alert attached to incident"
        ),
        Decision::Updated { incident } => tracing::info!(
            incident_id = %incident, decision = "updated", rule = "exact",
            source, source_alert_id, "alert updated in incident"
        ),
        Decision::Closed { incident } => tracing::info!(
            incident_id = %incident, decision = "closed", rule = "exact",
            reason = super::CloseReason::Resolved.as_str(),
            source, source_alert_id, "incident closed"
        ),
        Decision::DroppedResolve => tracing::info!(
            decision = "dropped_resolve",
            source,
            source_alert_id,
            "resolve matched no open incident, dropped"
        ),
    }
}

fn request_triage(
    store: &impl IncidentStore,
    triage_tx: &tokio::sync::mpsc::Sender<TriageRequest>,
    incident: IncidentId,
) {
    let Some(alert) = store.get(incident).and_then(|i| i.alerts.first()).cloned() else {
        tracing::error!(incident_id = %incident, "opened incident vanished before triage (bug)");
        return;
    };
    match triage_tx.try_send(TriageRequest { incident, alert }) {
        Ok(()) => {}
        Err(error @ TrySendError::Full(_)) => tracing::warn!(
            incident_id = %incident,
            %error,
            triage_skipped = true,
            reason = "queue_full",
            "triage request shed; incident stays untriaged"
        ),
        Err(TrySendError::Closed(_)) => tracing::warn!(
            incident_id = %incident,
            triage_skipped = true,
            reason = "channel_closed",
            "triage channel closed; incident stays untriaged"
        ),
    }
}

fn record_triage(
    store: &mut impl IncidentStore,
    investigation_tx: &tokio::sync::mpsc::Sender<InvestigationRequest>,
    triaged: Triaged,
) {
    store.set_triage(triaged.incident, triaged.result.clone());
    tracing::info!(
        incident_id = %triaged.incident,
        severity = %triaged.result.severity,
        service = triaged.result.service.as_deref(),
        "incident triaged"
    );
    request_investigation(store, investigation_tx, triaged.incident, triaged.result);
}

/// One investigation per incident fired by its triage result.
fn request_investigation(
    store: &impl IncidentStore,
    investigation_tx: &tokio::sync::mpsc::Sender<InvestigationRequest>,
    incident: IncidentId,
    triage: TriageResult,
) {
    let Some(triaged) = store.get(incident) else {
        tracing::error!(incident_id = %incident, "triaged incident vanished (bug)");
        return;
    };
    if !matches!(triaged.state, IncidentState::Open) {
        tracing::info!(
            incident_id = %incident,
            investigation_skipped = true,
            reason = "incident_closed",
            "incident closed before triage returned, not investigated"
        );
        return;
    }
    let Some(alert) = triaged.alerts.first().cloned() else {
        tracing::error!(incident_id = %incident, "triaged incident has no alerts (bug)");
        return;
    };
    match investigation_tx.try_send(InvestigationRequest {
        incident,
        alert,
        triage,
    }) {
        Ok(()) => {}
        Err(error @ TrySendError::Full(_)) => tracing::warn!(
            incident_id = %incident,
            %error,
            investigation_skipped = true,
            reason = "queue_full",
            "investigation request shed; incident stays uninvestigated"
        ),
        Err(TrySendError::Closed(_)) => tracing::warn!(
            incident_id = %incident,
            investigation_skipped = true,
            reason = "channel_closed",
            "investigation channel closed; incident stays uninvestigated"
        ),
    }
}

fn record_investigation(store: &mut impl IncidentStore, investigation: Investigation) {
    let incident = investigation.transcript.incident;
    tracing::info!(
        incident_id = %incident,
        outcome = ?investigation.transcript.outcome,
        hypothesis = investigation.hypothesis.is_some(),
        unverified_evidence = investigation.unverified_evidence.len(),
        "investigation recorded on incident"
    );
    store.set_investigation(incident, investigation);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertStatus, test_alert};
    use crate::incident::InMemoryStore;
    use crate::triage::{Severity, TriageResult};
    use tokio_util::sync::CancellationToken;

    struct Rig {
        alerts_tx: tokio::sync::mpsc::Sender<crate::alert::Alert>,
        triaged_tx: tokio::sync::mpsc::Sender<Triaged>,
        investigated_tx: tokio::sync::mpsc::Sender<Investigation>,
        triage_rx: tokio::sync::mpsc::Receiver<TriageRequest>,
        investigation_rx: tokio::sync::mpsc::Receiver<InvestigationRequest>,
        shutdown: CancellationToken,
        handle: tokio::task::JoinHandle<usize>,
    }

    fn spawn_worker(
        triage_capacity: usize,
        investigation_capacity: usize,
        idle_ttl: TimeDelta,
    ) -> Rig {
        let (alerts_tx, alerts_rx) = tokio::sync::mpsc::channel(8);
        let (triaged_tx, triaged_rx) = tokio::sync::mpsc::channel(8);
        let (investigated_tx, investigated_rx) = tokio::sync::mpsc::channel(8);
        let (triage_tx, triage_rx) = tokio::sync::mpsc::channel(triage_capacity);
        let (investigation_tx, investigation_rx) =
            tokio::sync::mpsc::channel(investigation_capacity);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(worker(
            Channels {
                alerts_rx,
                triaged_rx,
                investigated_rx,
                triage_tx,
                investigation_tx,
            },
            InMemoryStore::new(),
            idle_ttl,
            shutdown.clone(),
        ));
        Rig {
            alerts_tx,
            triaged_tx,
            investigated_tx,
            triage_rx,
            investigation_rx,
            shutdown,
            handle,
        }
    }

    fn firing(id: &str) -> crate::alert::Alert {
        let mut alert = test_alert();
        alert.source_alert_id = id.to_owned();
        alert
    }

    fn triage_result() -> TriageResult {
        TriageResult {
            severity: Severity::P1,
            service: Some("checkout".to_owned()),
            tags: Vec::new(),
            summary: "stub".to_owned(),
        }
    }

    #[tokio::test]
    async fn only_new_incidents_request_triage() {
        let mut rig = spawn_worker(8, 8, TimeDelta::hours(24));
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        rig.alerts_tx.send(firing("a2")).await.expect("queue open");

        let request = rig.triage_rx.recv().await.expect("one request");
        assert_eq!(request.alert.source_alert_id, "a1");

        rig.shutdown.cancel();
        let dropped = rig.handle.await.expect("worker exits");
        assert_eq!(dropped, 0, "all alerts were processed before shutdown");
        assert!(
            rig.triage_rx.try_recv().is_err(),
            "duplicate and attached alerts must not re-request triage"
        );
    }

    #[tokio::test]
    async fn triaged_results_are_recorded_without_new_requests() {
        let mut rig = spawn_worker(8, 8, TimeDelta::hours(24));
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        let request = rig.triage_rx.recv().await.expect("request");

        rig.triaged_tx
            .send(Triaged {
                incident: request.incident,
                result: triage_result(),
            })
            .await
            .expect("worker alive");

        // Biased select drains the Triaged result before this alert, so the
        // in-loop record_triage arm ran once the request below arrives.
        let mut second = firing("b1");
        second.labels.insert("shard".to_owned(), "b".to_owned());
        rig.alerts_tx.send(second).await.expect("queue open");
        let request = rig.triage_rx.recv().await.expect("second request");
        assert_eq!(request.alert.source_alert_id, "b1");

        rig.shutdown.cancel();
        let dropped = rig.handle.await.expect("worker exits");
        assert_eq!(dropped, 0, "all alerts were processed before shutdown");
        rig.triage_rx
            .try_recv()
            .expect_err("a recorded triage asks for nothing more");
    }

    #[tokio::test]
    async fn full_triage_queue_never_blocks_intake() {
        let mut rig = spawn_worker(1, 8, TimeDelta::hours(24));
        // Three distinct fingerprints: three opens, but the request channel
        // holds one — the worker must shed, not block.
        for n in 0..3 {
            let mut alert = firing(&format!("a{n}"));
            alert.labels.insert("n".to_owned(), n.to_string());
            rig.alerts_tx.send(alert).await.expect("queue open");
        }
        let first = rig.triage_rx.recv().await.expect("first request");
        assert_eq!(first.alert.source_alert_id, "a0");

        rig.shutdown.cancel();
        let dropped = rig.handle.await.expect("worker exits despite full channel");
        assert_eq!(dropped, 0, "all three alerts were processed");
    }

    #[tokio::test]
    async fn shutdown_drains_and_counts_unprocessed_alerts() {
        let (alerts_tx, alerts_rx) = tokio::sync::mpsc::channel(8);
        let (triaged_tx, triaged_rx) = tokio::sync::mpsc::channel::<Triaged>(8);
        let (investigated_tx, investigated_rx) = tokio::sync::mpsc::channel::<Investigation>(8);
        let (triage_tx, _triage_rx) = tokio::sync::mpsc::channel(8);
        let (investigation_tx, _investigation_rx) = tokio::sync::mpsc::channel(8);

        let alert = test_alert();
        let incident = IncidentId::new();
        let mut store = InMemoryStore::new();
        store.insert(crate::incident::Incident::open(
            incident,
            crate::incident::DedupeKey::of(&alert),
            alert,
            chrono::Utc::now(),
        ));
        for _ in 0..2 {
            alerts_tx.send(test_alert()).await.expect("queue open");
        }
        triaged_tx
            .send(Triaged {
                incident,
                result: triage_result(),
            })
            .await
            .expect("queue open");
        investigated_tx
            .send(crate::investigation::test_investigation(incident))
            .await
            .expect("queue open");

        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let dropped = worker(
            Channels {
                alerts_rx,
                triaged_rx,
                investigated_rx,
                triage_tx,
                investigation_tx,
            },
            store,
            chrono::TimeDelta::hours(24),
            shutdown,
        )
        .await;
        assert_eq!(dropped, 2, "queued alerts count as dropped on shutdown");
    }

    #[tokio::test]
    async fn a_triage_result_requests_an_investigation() {
        let mut rig = spawn_worker(8, 8, TimeDelta::hours(24));
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        let triage = rig.triage_rx.recv().await.expect("triage request");

        rig.triaged_tx
            .send(Triaged {
                incident: triage.incident,
                result: triage_result(),
            })
            .await
            .expect("worker alive");

        let request = rig
            .investigation_rx
            .recv()
            .await
            .expect("investigation request");
        assert_eq!(request.incident, triage.incident);
        assert_eq!(request.alert.source_alert_id, "a1");
        assert_eq!(request.triage.severity, Severity::P1);

        rig.shutdown.cancel();
        let dropped = rig.handle.await.expect("worker exits");
        assert_eq!(dropped, 0, "all alerts were processed before shutdown");
    }

    /// Triage outlives the incident it was asked about often enough that the
    /// tokens of an investigation nobody will read must not be spent.
    #[tokio::test]
    async fn an_incident_closed_before_triage_returns_is_not_investigated() {
        let mut rig = spawn_worker(8, 8, TimeDelta::hours(24));
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        let closed = rig.triage_rx.recv().await.expect("first triage request");

        let mut resolve = firing("a1");
        resolve.status = AlertStatus::Resolved;
        rig.alerts_tx.send(resolve).await.expect("queue open");
        // Alerts are one FIFO queue, so the second incident's triage request
        // proves the resolve above was already processed.
        let mut second = firing("b1");
        second.labels.insert("shard".to_owned(), "b".to_owned());
        rig.alerts_tx.send(second).await.expect("queue open");
        let open = rig.triage_rx.recv().await.expect("second triage request");

        for incident in [closed.incident, open.incident] {
            rig.triaged_tx
                .send(Triaged {
                    incident,
                    result: triage_result(),
                })
                .await
                .expect("worker alive");
        }

        let request = rig
            .investigation_rx
            .recv()
            .await
            .expect("investigation request");
        assert_eq!(
            request.incident, open.incident,
            "the closed incident must not be investigated"
        );

        rig.shutdown.cancel();
        rig.handle.await.expect("worker exits");
        assert!(
            rig.investigation_rx.try_recv().is_err(),
            "only the still-open incident is investigated"
        );
    }

    /// The result arm records and stops: recording must not fan out into
    /// more work.
    #[tokio::test]
    async fn an_investigation_result_is_recorded_without_new_requests() {
        let mut rig = spawn_worker(8, 8, TimeDelta::hours(24));
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        let triage = rig.triage_rx.recv().await.expect("triage request");
        rig.triaged_tx
            .send(Triaged {
                incident: triage.incident,
                result: triage_result(),
            })
            .await
            .expect("worker alive");
        let request = rig
            .investigation_rx
            .recv()
            .await
            .expect("investigation request");

        rig.investigated_tx
            .send(crate::investigation::test_investigation(request.incident))
            .await
            .expect("worker alive");

        // Biased select drains the result before this alert, so the recording
        // arm ran once the triage request below arrives.
        let mut second = firing("b1");
        second.labels.insert("shard".to_owned(), "b".to_owned());
        rig.alerts_tx.send(second).await.expect("queue open");
        rig.triage_rx.recv().await.expect("second triage request");

        rig.shutdown.cancel();
        let dropped = rig.handle.await.expect("worker exits");
        assert_eq!(dropped, 0, "all alerts were processed before shutdown");
        assert!(
            rig.investigation_rx.try_recv().is_err(),
            "a recorded investigation asks for nothing more"
        );
    }

    #[tokio::test]
    async fn full_investigation_queue_never_blocks_the_worker() {
        let mut rig = spawn_worker(8, 1, TimeDelta::hours(24));
        // Three incidents triaged, one slot to send into: the worker must
        // shed, not block on the outbound channel.
        let mut incidents = Vec::new();
        for n in 0..3 {
            let mut alert = firing(&format!("a{n}"));
            alert.labels.insert("n".to_owned(), n.to_string());
            rig.alerts_tx.send(alert).await.expect("queue open");
            incidents.push(rig.triage_rx.recv().await.expect("triage request").incident);
        }
        for incident in incidents {
            rig.triaged_tx
                .send(Triaged {
                    incident,
                    result: triage_result(),
                })
                .await
                .expect("worker alive");
        }

        let request = rig
            .investigation_rx
            .recv()
            .await
            .expect("the one request that fit");
        assert_eq!(request.alert.source_alert_id, "a0");

        rig.shutdown.cancel();
        let dropped = rig
            .handle
            .await
            .expect("worker exits despite a full investigation channel");
        assert_eq!(dropped, 0, "all three alerts were processed");
    }

    #[tokio::test]
    async fn resolves_flow_through_without_triage_requests() {
        let mut rig = spawn_worker(8, 8, TimeDelta::hours(24));
        let mut resolve = firing("never-seen");
        resolve.status = AlertStatus::Resolved;
        rig.alerts_tx.send(resolve).await.expect("queue open");
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");

        let request = rig
            .triage_rx
            .recv()
            .await
            .expect("request for the firing alert");
        assert_eq!(
            request.alert.source_alert_id, "a1",
            "the orphan resolve must not reach triage"
        );
        rig.shutdown.cancel();
        rig.handle.await.expect("worker exits");
    }

    #[tokio::test]
    async fn zero_ttl_expires_before_dedupe_so_refires_reopen() {
        let mut rig = spawn_worker(8, 8, TimeDelta::zero());
        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        let first = rig.triage_rx.recv().await.expect("first request");

        rig.alerts_tx.send(firing("a1")).await.expect("queue open");
        let second = rig.triage_rx.recv().await.expect("second request");
        assert_ne!(
            first.incident, second.incident,
            "the sweep must expire the first incident before dedupe sees the refire"
        );

        rig.shutdown.cancel();
        let dropped = rig.handle.await.expect("worker exits");
        assert_eq!(dropped, 0, "both alerts were processed before shutdown");
    }
}
