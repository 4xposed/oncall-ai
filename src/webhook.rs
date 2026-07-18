use crate::alert::{Alert, AlertSource};
use crate::config::{SourceConfig, WebhookConfig};
use crate::grafana::Grafana;
use crate::pagerduty::Pagerduty;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::collections::HashMap;
use std::sync::Arc;

type Registry = Arc<HashMap<&'static str, &'static dyn AlertSource>>;

#[derive(Clone)]
struct AppState {
    registry: Registry,
    body_log_limit_bytes: usize,
    alerts_tx: tokio::sync::mpsc::Sender<Alert>,
}

/// Builds the registry from the enabled sources
fn registry(config: &WebhookConfig) -> Registry {
    let sources: [(&'static dyn AlertSource, &SourceConfig); 2] =
        [(&Grafana, &config.grafana), (&Pagerduty, &config.pagerduty)];
    Arc::new(
        sources
            .into_iter()
            .filter(|(_, source)| source.enabled)
            .map(|(adapter, _)| (adapter.name(), adapter))
            .collect(),
    )
}

/// Serves `router` until `shutdown` resolves, then drains in-flight requests.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Builds the webhook router
/// `POST /webhook/{source}` parses and enqueues alerts.
/// `GET /health` reports liveness.
pub fn router(config: &WebhookConfig, alerts_tx: tokio::sync::mpsc::Sender<Alert>) -> Router {
    let registry = registry(config);
    if registry.is_empty() {
        tracing::warn!("no webhook sources enabled; every webhook will 404");
    }
    Router::new()
        .route("/webhook/{source}", post(webhook))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(config.body_limit_bytes))
        .with_state(AppState {
            registry,
            body_log_limit_bytes: config.body_log_limit_bytes,
            alerts_tx,
        })
}

async fn webhook(
    State(state): State<AppState>,
    Path(source): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(rejection) => {
            tracing::warn!(source, error = %rejection, "webhook body rejected");
            return rejection.into_response();
        }
    };
    match std::str::from_utf8(&bytes) {
        Ok(text) => {
            let body = crate::log::truncate_utf8(text, state.body_log_limit_bytes);
            tracing::info!(
                source,
                bytes = bytes.len(),
                body,
                truncated = body.len() < text.len(),
                "webhook received"
            );
        }
        Err(_) => {
            tracing::info!(
                source,
                bytes = bytes.len(),
                "webhook received (non-utf8 body)"
            );
        }
    }

    let Some(adapter) = state.registry.get(source.as_str()) else {
        tracing::warn!(source, "unknown webhook source");
        return StatusCode::NOT_FOUND.into_response();
    };
    match adapter.parse(&bytes) {
        Ok(alerts) => {
            for alert in &alerts {
                tracing::info!(
                    source = alert.source,
                    source_alert_id = alert.source_alert_id,
                    status = %alert.status,
                    starts_at = %alert.starts_at,
                    labels = ?alert.labels,
                    "alert"
                );
            }
            let total = alerts.len();
            match state.alerts_tx.try_reserve_many(total) {
                Ok(permits) => {
                    for (permit, alert) in permits.zip(alerts) {
                        permit.send(alert);
                    }
                    tracing::info!(source, alerts = total, "webhook parsed");
                    StatusCode::ACCEPTED.into_response()
                }
                Err(error) => {
                    tracing::warn!(source, total, %error, "intake queue rejected delivery");
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        }
        Err(error) => {
            tracing::warn!(source, %error, "webhook parse failed");
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tokio::sync::mpsc::Receiver;
    use tower::ServiceExt;

    use crate::alert::Alert;
    use crate::config::{SourceConfig, WebhookConfig};

    fn test_config(body_limit_bytes: usize) -> WebhookConfig {
        WebhookConfig {
            bind: "127.0.0.1:0".parse().expect("valid addr"),
            body_limit_bytes,
            body_log_limit_bytes: 65_536,
            grafana: SourceConfig { enabled: true },
            pagerduty: SourceConfig { enabled: true },
        }
    }

    fn router_with(
        config: &WebhookConfig,
        queue_capacity: usize,
    ) -> (axum::Router, Receiver<Alert>) {
        let (alerts_tx, alerts_rx) = tokio::sync::mpsc::channel(queue_capacity);
        let router = super::router(config, alerts_tx);
        (router, alerts_rx)
    }

    fn test_router() -> (axum::Router, Receiver<Alert>) {
        router_with(&test_config(4096), 64)
    }

    async fn body_text(body: Body) -> String {
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .expect("read response body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 response body")
    }

    #[tokio::test]
    async fn parseable_grafana_webhook_returns_202() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/webhook/grafana")
                    .body(Body::from(
                        include_bytes!("../fixtures/grafana/firing_single.json") as &[u8],
                    ))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn parseable_pagerduty_webhook_returns_202() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/webhook/pagerduty")
                    .body(Body::from(
                        include_bytes!("../fixtures/pagerduty/triggered.json") as &[u8],
                    ))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn skipped_pagerduty_event_returns_202() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/webhook/pagerduty")
                    .body(Body::from(
                        include_bytes!("../fixtures/pagerduty/priority_updated.json") as &[u8],
                    ))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn full_queue_returns_503() {
        let (router, _alerts_rx) = router_with(&test_config(4096), 1);
        let post = || {
            Request::post("/webhook/grafana")
                .body(Body::from(
                    include_bytes!("../fixtures/grafana/firing_single.json") as &[u8],
                ))
                .expect("build request")
        };
        let first = router
            .clone()
            .oneshot(post())
            .await
            .expect("router response");
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let second = router.oneshot(post()).await.expect("router response");
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn batch_larger_than_queue_capacity_returns_503() {
        let (router, _alerts_rx) = router_with(&test_config(4096), 1);
        let response = router
            .oneshot(
                Request::post("/webhook/grafana")
                    .body(Body::from(
                        include_bytes!("../fixtures/grafana/firing_batch.json") as &[u8],
                    ))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn enqueued_alerts_reach_the_receiver() {
        let (router, mut alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/webhook/grafana")
                    .body(Body::from(
                        include_bytes!("../fixtures/grafana/firing_batch.json") as &[u8],
                    ))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let first = alerts_rx.recv().await.expect("first alert enqueued");
        assert_eq!(first.source, "grafana");
        assert_eq!(first.source_alert_id, "c4f3a2b1d8e90f67");
        let second = alerts_rx.recv().await.expect("second alert enqueued");
        assert_eq!(second.source, "grafana");
        assert_eq!(second.source_alert_id, "9e2a4c6b8d0f1e35");
        assert!(
            alerts_rx.try_recv().is_err(),
            "the batch has exactly two alerts, nothing more may be enqueued"
        );
    }

    #[tokio::test]
    async fn unknown_source_is_404() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/webhook/nonexistent")
                    .body(Body::from(r#"{"any":"json"}"#))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn disabled_source_is_404() {
        let mut config = test_config(4096);
        config.pagerduty = SourceConfig { enabled: false };
        let (router, _alerts_rx) = router_with(&config, 64);
        let response = router
            .oneshot(
                Request::post("/webhook/pagerduty")
                    .body(Body::from(
                        include_bytes!("../fixtures/pagerduty/triggered.json") as &[u8],
                    ))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn disabled_source_is_left_out_of_the_registry() {
        let mut config = test_config(4096);
        config.pagerduty = SourceConfig { enabled: false };
        let registry = super::registry(&config);
        assert!(registry.contains_key("grafana"));
        assert!(!registry.contains_key("pagerduty"));
    }

    #[tokio::test]
    async fn malformed_payload_is_400_with_error_body() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/webhook/grafana")
                    .body(Body::from(r#"{"any":"json"}"#))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response.into_body()).await;
        assert!(
            body.contains("alerts"),
            "error body must name what failed to parse: {body}"
        );
    }

    #[tokio::test]
    async fn oversized_body_is_413() {
        let (router, _alerts_rx) = router_with(&test_config(64), 64);
        let response = router
            .oneshot(
                Request::post("/webhook/grafana")
                    .body(Body::from(vec![b'x'; 65]))
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn health_returns_200_ok() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response.into_body()).await, "ok");
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::post("/nope")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_method_on_webhook_is_405() {
        let (router, _alerts_rx) = test_router();
        let response = router
            .oneshot(
                Request::get("/webhook/grafana")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
