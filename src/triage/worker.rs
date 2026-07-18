use super::{ErrorClass, TriageError, TriageResult, Triager};
use tokio_util::sync::CancellationToken;

const OUTPUT_ERROR_ATTEMPTS: u32 = 3;

pub trait Triage {
    fn triage(
        &self,
        alert: &crate::alert::Alert,
    ) -> impl Future<Output = Result<TriageResult, TriageError>> + Send;
}

impl Triage for Triager {
    fn triage(
        &self,
        alert: &crate::alert::Alert,
    ) -> impl Future<Output = Result<TriageResult, TriageError>> + Send {
        Triager::triage(self, alert)
    }
}

pub async fn worker(
    mut requests_rx: tokio::sync::mpsc::Receiver<crate::incident::TriageRequest>,
    triager: impl Triage,
    backoff: crate::retry::Backoff,
    done_tx: tokio::sync::mpsc::Sender<crate::incident::Triaged>,
    shutdown: CancellationToken,
) -> usize {
    let mut dropped: usize = 0;
    loop {
        let request = tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            received = requests_rx.recv() => match received {
                Some(request) => request,
                None => break,
            },
        };
        let mut output_attempts = 0u32;
        let triage_call = std::pin::pin!(crate::retry::with_backoff(
            backoff,
            |error: &TriageError| {
                if error.class() != ErrorClass::Output {
                    return true;
                }
                output_attempts += 1;
                output_attempts < OUTPUT_ERROR_ATTEMPTS
            },
            || triager.triage(&request.alert),
        ));
        let result = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                dropped += 1;
                break;
            }
            result = triage_call => result,
        };
        log_outcome(&request, result.as_ref());
        if let Ok(result) = result {
            let triaged = crate::incident::Triaged {
                incident: request.incident,
                result,
            };
            if done_tx.send(triaged).await.is_err() {
                tracing::warn!(
                    incident_id = %request.incident,
                    "triage result dropped; incident worker gone"
                );
            }
        }
    }
    while requests_rx.try_recv().is_ok() {
        dropped += 1;
    }
    tracing::info!(dropped, "triage queue drained");
    dropped
}

fn log_outcome(
    request: &crate::incident::TriageRequest,
    result: Result<&TriageResult, &TriageError>,
) {
    match result {
        Ok(triage) => tracing::info!(
            incident_id = %request.incident,
            source = request.alert.source,
            source_alert_id = request.alert.source_alert_id,
            severity = %triage.severity,
            service = triage.service.as_deref(),
            tags = ?triage.tags,
            summary = triage.summary,
            "alert triaged"
        ),
        Err(error) => tracing::error!(
            incident_id = %request.incident,
            source = request.alert.source,
            source_alert_id = request.alert.source_alert_id,
            %error,
            "triage abandoned after capped retries"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{Alert, test_alert};
    use crate::triage::Severity;
    use rig_core::completion::CompletionError;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn tiny_backoff() -> crate::retry::Backoff {
        crate::retry::Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        }
    }

    fn request(alert: crate::alert::Alert) -> crate::incident::TriageRequest {
        crate::incident::TriageRequest {
            incident: crate::incident::IncidentId::new(),
            alert,
        }
    }

    #[derive(Clone)]
    struct ScriptedTriage {
        calls: Arc<AtomicUsize>,
        errors: Arc<Mutex<VecDeque<TriageError>>>,
        hang: bool,
    }

    impl ScriptedTriage {
        fn new(hang: bool) -> Self {
            ScriptedTriage {
                calls: Arc::new(AtomicUsize::new(0)),
                errors: Arc::new(Mutex::new(VecDeque::new())),
                hang,
            }
        }

        fn with_errors(errors: impl IntoIterator<Item = TriageError>) -> Self {
            ScriptedTriage {
                calls: Arc::new(AtomicUsize::new(0)),
                errors: Arc::new(Mutex::new(errors.into_iter().collect())),
                hang: false,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Triage for ScriptedTriage {
        fn triage(
            &self,
            _alert: &Alert,
        ) -> impl Future<Output = Result<TriageResult, TriageError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let error = self.errors.lock().expect("script lock").pop_front();
            let hang = self.hang;
            async move {
                if hang {
                    std::future::pending().await
                } else if let Some(error) = error {
                    Err(error)
                } else {
                    Ok(TriageResult {
                        severity: Severity::P4,
                        service: None,
                        tags: Vec::new(),
                        summary: "stubbed".to_owned(),
                    })
                }
            }
        }
    }

    fn model_error() -> TriageError {
        TriageError::Completion(CompletionError::ProviderError("scripted".to_owned()))
    }

    #[tokio::test]
    async fn triages_every_queued_alert_and_drops_nothing_on_close() {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(8);
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel(8);
        let mut sent = Vec::new();
        for _ in 0..3 {
            let request = request(test_alert());
            sent.push(request.incident);
            requests_tx.send(request).await.expect("queue open");
        }
        drop(requests_tx);

        let triage = ScriptedTriage::new(false);
        let dropped = worker(
            requests_rx,
            triage.clone(),
            tiny_backoff(),
            done_tx,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(dropped, 0, "a closed empty queue drops nothing");
        assert_eq!(triage.calls(), 3, "every queued alert is triaged once");
        for incident in sent {
            let triaged = done_rx.recv().await.expect("one result per request");
            assert_eq!(triaged.incident, incident, "results keep send order");
            assert_eq!(triaged.result.summary, "stubbed");
        }
        assert!(
            done_rx.try_recv().is_err(),
            "exactly three results, nothing more"
        );
    }

    #[tokio::test]
    async fn output_errors_give_up_after_the_cap() {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(8);
        let (done_tx, _done_rx) = tokio::sync::mpsc::channel(8);
        requests_tx
            .send(request(test_alert()))
            .await
            .expect("queue open");
        drop(requests_tx);

        let triage = ScriptedTriage::with_errors([
            TriageError::MissingText,
            TriageError::MissingText,
            TriageError::MissingText,
            TriageError::MissingText,
        ]);
        let dropped = worker(
            requests_rx,
            triage.clone(),
            tiny_backoff(),
            done_tx,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(dropped, 0, "an abandoned alert is not a shutdown drop");
        assert_eq!(triage.calls(), 3, "output errors stop at the cap");
    }

    #[tokio::test]
    async fn model_errors_retry_and_do_not_consume_the_output_cap() {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(8);
        let (done_tx, _done_rx) = tokio::sync::mpsc::channel(8);
        requests_tx
            .send(request(test_alert()))
            .await
            .expect("queue open");
        drop(requests_tx);

        let triage = ScriptedTriage::with_errors([
            TriageError::MissingText,
            model_error(),
            TriageError::MissingText,
        ]);
        let dropped = worker(
            requests_rx,
            triage.clone(),
            tiny_backoff(),
            done_tx,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(dropped, 0);
        assert_eq!(
            triage.calls(),
            4,
            "two output and one model error, then success"
        );
    }

    #[tokio::test]
    async fn shutdown_drops_the_in_flight_alert_and_the_queued_rest() {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(8);
        let (done_tx, _done_rx) = tokio::sync::mpsc::channel(8);
        for _ in 0..3 {
            requests_tx
                .send(request(test_alert()))
                .await
                .expect("queue open");
        }

        let triage = ScriptedTriage::new(true);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(worker(
            requests_rx,
            triage.clone(),
            tiny_backoff(),
            done_tx,
            shutdown.clone(),
        ));

        for _ in 0..100 {
            if triage.calls() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(triage.calls(), 1, "first alert must be in flight");

        shutdown.cancel();
        let dropped = handle.await.expect("worker task completes");
        assert_eq!(
            dropped, 3,
            "one abandoned in flight plus two never dequeued"
        );
        assert_eq!(triage.calls(), 1, "queued alerts are dropped, not triaged");
    }

    #[tokio::test]
    async fn shutdown_before_any_alert_drains_the_queue() {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(8);
        let (done_tx, _done_rx) = tokio::sync::mpsc::channel(8);
        for _ in 0..2 {
            requests_tx
                .send(request(test_alert()))
                .await
                .expect("queue open");
        }

        let triage = ScriptedTriage::new(false);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let dropped = worker(
            requests_rx,
            triage.clone(),
            tiny_backoff(),
            done_tx,
            shutdown,
        )
        .await;

        assert_eq!(dropped, 2, "everything queued is counted as dropped");
        assert_eq!(triage.calls(), 0, "nothing is triaged after shutdown");
    }

    #[tokio::test]
    async fn failed_triage_sends_no_result() {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(8);
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel(8);
        requests_tx
            .send(request(test_alert()))
            .await
            .expect("queue open");
        drop(requests_tx);

        let triage = ScriptedTriage::with_errors([
            TriageError::MissingText,
            TriageError::MissingText,
            TriageError::MissingText,
            TriageError::MissingText,
        ]);
        worker(
            requests_rx,
            triage,
            tiny_backoff(),
            done_tx,
            CancellationToken::new(),
        )
        .await;
        assert!(
            done_rx.try_recv().is_err(),
            "an abandoned triage must not fabricate a result"
        );
    }
}
