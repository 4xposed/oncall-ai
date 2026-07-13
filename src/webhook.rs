use crate::alert::AlertSource;
use crate::config::WebhookConfig;
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

type Registry = Arc<HashMap<&'static str, Box<dyn AlertSource>>>;

#[derive(Clone)]
struct AppState {
    registry: Registry,
    body_log_limit_bytes: usize,
}

/// Builds the registry. New adapters are one line here.
fn registry() -> Registry {
    Arc::new(
        [
            Box::new(Grafana) as Box<dyn AlertSource>,
            Box::new(Pagerduty),
        ]
        .into_iter()
        .map(|adapter| (adapter.name(), adapter))
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

/// Builds the webhook router.
///
/// `POST /webhook/{source}` dispatches to that alert source and
/// `GET /health` reports liveness.
pub fn router(config: &WebhookConfig) -> Router {
    Router::new()
        .route("/webhook/{source}", post(webhook))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(config.body_limit_bytes))
        .with_state(AppState {
            registry: registry(),
            body_log_limit_bytes: config.body_log_limit_bytes,
        })
}

/// Returns the longest prefix of at most `max` bytes that ends on a
/// character boundary.
fn truncate_at_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let end = (0..=max)
        .rev()
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(0);
    text.get(..end).unwrap_or_default()
}

/// Logs the raw body, then parses it with the matching adapter.
///
/// The raw line is the audit trail for unparseable payloads and the capture
/// source for new fixtures; bodies over `body_log_limit_bytes` are echoed
/// truncated, with `truncated = true` marking the cut.
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
            let body = truncate_at_char_boundary(text, state.body_log_limit_bytes);
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
            tracing::info!(source, alerts = alerts.len(), "webhook parsed");
            StatusCode::ACCEPTED.into_response()
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
    use tower::ServiceExt;

    use crate::config::WebhookConfig;

    fn router_with_limit(body_limit_bytes: usize) -> axum::Router {
        super::router(&WebhookConfig {
            bind: "127.0.0.1:0".parse().expect("valid addr"),
            body_limit_bytes,
            body_log_limit_bytes: 65_536,
        })
    }

    #[test]
    fn truncation_is_char_boundary_safe() {
        // "é" is two bytes; a cap of 3 lands mid-character and must back up.
        assert_eq!(super::truncate_at_char_boundary("aéé", 3), "aé");
        assert_eq!(super::truncate_at_char_boundary("aéé", 5), "aéé");
        assert_eq!(super::truncate_at_char_boundary("abc", 2), "ab");
        assert_eq!(super::truncate_at_char_boundary("é", 1), "");
    }

    fn test_router() -> axum::Router {
        router_with_limit(4096)
    }

    async fn body_text(body: Body) -> String {
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .expect("read response body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 response body")
    }

    #[tokio::test]
    async fn parseable_grafana_webhook_returns_202() {
        let response = test_router()
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
        let response = test_router()
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
        let response = test_router()
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
    async fn unknown_source_is_404() {
        let response = test_router()
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
    async fn malformed_payload_is_400_with_error_body() {
        let response = test_router()
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
        let response = router_with_limit(64)
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
        let response = test_router()
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
        let response = test_router()
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
        let response = test_router()
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
