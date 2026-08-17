use super::{TRIAGE_PREAMBLE, TriageResult, render_prompt};
use crate::config::TriageConfig;
use crate::model::AnyCompletionModel;
use rig_core::completion::{AssistantContent, CompletionError, CompletionModel as _};
use rig_core::http_client;
use std::time::Duration;

const OUTPUT_SNIPPET_BYTES: usize = 200;

pub struct Triager {
    model: AnyCompletionModel,
    timeout: Duration,
    temperature: Option<f64>,
}

impl Triager {
    #[must_use]
    pub fn new(model: AnyCompletionModel, config: &TriageConfig) -> Self {
        Self {
            model,
            timeout: Duration::from_secs(config.timeout_secs.get()),
            temperature: config.temperature,
        }
    }

    /// # Errors
    ///
    /// Fails on timeout, a completion error, or output that is not a
    /// [`TriageResult`]; [`TriageError::class`] tells the classes apart.
    pub async fn triage(&self, alert: &crate::alert::Alert) -> Result<TriageResult, TriageError> {
        let call = self
            .model
            .completion_request(render_prompt(alert))
            .preamble(TRIAGE_PREAMBLE.to_owned())
            .output_schema(schemars::schema_for!(TriageResult))
            .temperature_opt(self.temperature)
            .send();
        let response = match tokio::time::timeout(self.timeout, call).await {
            Ok(outcome) => outcome?,
            Err(_elapsed) => return Err(TriageError::Timeout(self.timeout)),
        };
        let text = response
            .choice
            .iter()
            .find_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .ok_or(TriageError::MissingText)?;
        serde_json::from_str(text).map_err(|source| TriageError::UnparseableOutput {
            snippet: crate::text::truncate_utf8(text, OUTPUT_SNIPPET_BYTES).to_owned(),
            source,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TriageError {
    #[error("triage call timed out after {0:?}")]
    Timeout(Duration),
    #[error(transparent)]
    Completion(#[from] CompletionError),
    #[error("model response has no text content")]
    MissingText,
    #[error("model output is not valid TriageResult JSON: {source}; output began: {snippet}")]
    UnparseableOutput {
        snippet: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Transport,
    Model,
    Output,
}

impl TriageError {
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            TriageError::Timeout(_) => ErrorClass::Transport,
            TriageError::Completion(error) => classify_completion(error),
            TriageError::MissingText | TriageError::UnparseableOutput { .. } => ErrorClass::Output,
        }
    }
}

fn classify_completion(error: &CompletionError) -> ErrorClass {
    match error {
        CompletionError::HttpError(http_error) => match http_error {
            http_client::Error::InvalidStatusCode(_)
            | http_client::Error::InvalidStatusCodeWithMessage(_, _) => ErrorClass::Model,
            _ => ErrorClass::Transport,
        },
        _ => ErrorClass::Model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::test_alert;
    use crate::config::{ModelSpec, TriageConfig};
    use crate::triage::{Severity, TRIAGE_PREAMBLE, render_prompt};
    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use rig_core::client::{CompletionClient as _, Nothing};
    use rig_core::completion::CompletionError;
    use rig_core::providers::ollama;
    use std::net::SocketAddr;
    use std::num::{NonZeroU64, NonZeroUsize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const VALID_CONTENT_RESPONSE: &str = r#"{"model":"test-model","created_at":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":"{\"severity\":\"P2\",\"service\":\"checkout\",\"tags\":[\"database\"],\"summary\":\"Checkout DB latency is elevated.\"}"},"done":true}"#;

    const MALFORMED_CONTENT_RESPONSE: &str = r#"{"model":"test-model","created_at":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":"not json at all"},"done":true}"#;

    const TOOL_CALL_ONLY_RESPONSE: &str = r#"{"model":"test-model","created_at":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":"","tool_calls":[{"type":"function","function":{"name":"submit","arguments":{"severity":"P2"}}}]},"done":true}"#;

    /// Assistant message with no content of any kind: rig rejects this
    /// pre-parse with `ResponseError("No content provided")`.
    const EMPTY_MESSAGE_RESPONSE: &str = r#"{"model":"test-model","created_at":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":""},"done":true}"#;

    fn chat_response_with_content(content: &str) -> String {
        serde_json::json!({
            "model": "test-model",
            "created_at": "2026-01-01T00:00:00Z",
            "message": { "role": "assistant", "content": content },
            "done": true,
        })
        .to_string()
    }

    #[derive(Clone)]
    enum FakeResponse {
        Chat(String),
        Status(u16),
        /// Sleeps longer than any test timeout.
        Hang,
    }

    #[derive(Clone)]
    struct FakeState {
        /// Response script: request N gets entry N; the last entry repeats
        /// once the script is exhausted.
        responses: Vec<FakeResponse>,
        requests: Arc<AtomicUsize>,
        seen: Arc<Mutex<Option<serde_json::Value>>>,
    }

    struct FakeOllama {
        addr: SocketAddr,
        seen: Arc<Mutex<Option<serde_json::Value>>>,
    }

    /// Serves one canned response for `POST /api/chat` on an ephemeral port,
    /// capturing the request body for assertions.
    async fn fake_ollama(response: FakeResponse) -> FakeOllama {
        fake_ollama_script(vec![response]).await
    }

    /// Serves a scripted response sequence for `POST /api/chat`: request N
    /// gets `responses[N]`, and the last entry repeats forever after.
    async fn fake_ollama_script(responses: Vec<FakeResponse>) -> FakeOllama {
        let seen = Arc::new(Mutex::new(None));
        let state = FakeState {
            responses,
            requests: Arc::new(AtomicUsize::new(0)),
            seen: Arc::clone(&seen),
        };
        let router = Router::new()
            .route("/api/chat", post(chat))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake ollama binds");
        let addr = listener.local_addr().expect("fake ollama has an address");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("fake ollama serves");
        });
        FakeOllama { addr, seen }
    }

    async fn chat(State(state): State<FakeState>, body: String) -> Response {
        *state.seen.lock().expect("request capture lock") = serde_json::from_str(&body).ok();
        let index = state.requests.fetch_add(1, Ordering::SeqCst);
        let response = state
            .responses
            .get(index)
            .or_else(|| state.responses.last())
            .expect("fake ollama script is non-empty")
            .clone();
        match response {
            FakeResponse::Chat(chat_body) => (StatusCode::OK, chat_body).into_response(),
            FakeResponse::Status(code) => (
                StatusCode::from_u16(code).expect("valid test status"),
                r#"{"error":"boom"}"#.to_owned(),
            )
                .into_response(),
            FakeResponse::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
                StatusCode::OK.into_response()
            }
        }
    }

    fn test_config() -> TriageConfig {
        TriageConfig {
            model: "ollama:test-model"
                .parse::<ModelSpec>()
                .expect("valid spec"),
            temperature: Some(0.0),
            queue_capacity: NonZeroUsize::new(8).expect("nonzero"),
            timeout_secs: NonZeroU64::new(5).expect("nonzero"),
            backoff_initial_ms: NonZeroU64::new(1).expect("nonzero"),
            backoff_max_ms: 1,
        }
    }

    fn test_triager_with_temperature(addr: SocketAddr, temperature: Option<f64>) -> Triager {
        let client = ollama::Client::builder()
            .api_key(Nothing)
            .base_url(format!("http://{addr}"))
            .build()
            .expect("test Ollama client builds");
        let model = AnyCompletionModel::new(client.completion_model("test-model"));
        let mut config = test_config();
        config.temperature = temperature;
        Triager::new(model, &config)
    }

    fn test_triager(addr: SocketAddr) -> Triager {
        test_triager_with_temperature(addr, Some(0.0))
    }

    #[tokio::test]
    async fn valid_model_output_parses() {
        let fake = fake_ollama(FakeResponse::Chat(VALID_CONTENT_RESPONSE.to_owned())).await;
        let result = test_triager(fake.addr)
            .triage(&test_alert())
            .await
            .expect("triage succeeds");
        assert_eq!(result.severity, Severity::P2);
        assert_eq!(result.service.as_deref(), Some("checkout"));
        assert_eq!(result.tags, ["database"]);
        assert_eq!(result.summary, "Checkout DB latency is elevated.");
    }

    #[tokio::test]
    async fn request_carries_schema_preamble_and_prompt() {
        let fake = fake_ollama(FakeResponse::Chat(VALID_CONTENT_RESPONSE.to_owned())).await;
        let alert = test_alert();
        test_triager(fake.addr)
            .triage(&alert)
            .await
            .expect("triage succeeds");

        let request = fake
            .seen
            .lock()
            .expect("request capture lock")
            .clone()
            .expect("fake ollama saw a request");
        assert_eq!(
            request.pointer("/stream"),
            Some(&serde_json::Value::Bool(false)),
            "call must be non-streaming"
        );
        assert_eq!(
            request
                .pointer("/model")
                .and_then(serde_json::Value::as_str),
            Some("test-model"),
            "the provider prefix must be stripped before the model reaches Ollama"
        );
        assert!(
            request
                .pointer("/format/properties/severity")
                .is_some_and(serde_json::Value::is_object),
            "TriageResult schema must reach Ollama's format field, got: {request}"
        );
        assert_eq!(
            request
                .pointer("/options/temperature")
                .and_then(serde_json::Value::as_f64),
            Some(0.0),
            "triage must request deterministic sampling, got: {request}"
        );
        assert!(
            request.pointer("/options/seed").is_none(),
            "triage must not inject Ollama-specific seed parameters: {request}"
        );
        assert_eq!(
            request
                .pointer("/messages/0/role")
                .and_then(serde_json::Value::as_str),
            Some("system")
        );
        assert_eq!(
            request
                .pointer("/messages/0/content")
                .and_then(serde_json::Value::as_str),
            Some(TRIAGE_PREAMBLE)
        );
        assert_eq!(
            request
                .pointer("/messages/1/content")
                .and_then(serde_json::Value::as_str),
            Some(render_prompt(&alert).as_str())
        );
    }

    #[tokio::test]
    async fn absent_temperature_leaves_provider_sampling_unset() {
        let fake = fake_ollama(FakeResponse::Chat(VALID_CONTENT_RESPONSE.to_owned())).await;

        test_triager_with_temperature(fake.addr, None)
            .triage(&test_alert())
            .await
            .expect("triage succeeds");

        let request = fake
            .seen
            .lock()
            .expect("request capture lock")
            .clone()
            .expect("fake Ollama saw a request");
        assert!(
            request.pointer("/options/temperature").is_none(),
            "absent stage temperature must defer to the provider: {request}"
        );
    }

    #[tokio::test]
    async fn malformed_model_output_is_output_error() {
        let fake = fake_ollama(FakeResponse::Chat(MALFORMED_CONTENT_RESPONSE.to_owned())).await;
        let error = test_triager(fake.addr)
            .triage(&test_alert())
            .await
            .expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Output);
        assert!(
            error.to_string().contains("not json at all"),
            "error must carry the model output, got: {error}"
        );
    }

    #[tokio::test]
    async fn unparseable_output_error_truncates_the_snippet() {
        let garbage = "z".repeat(300);
        let fake = fake_ollama(FakeResponse::Chat(chat_response_with_content(&garbage))).await;
        let error = test_triager(fake.addr)
            .triage(&test_alert())
            .await
            .expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Output);
        let message = error.to_string();
        assert!(
            message.contains(&"z".repeat(200)),
            "error must carry the first 200 bytes, got: {message}"
        );
        assert!(
            !message.contains(&"z".repeat(201)),
            "snippet must stop at 200 bytes, got: {message}"
        );
    }

    #[tokio::test]
    async fn tool_call_only_response_is_output_error() {
        let fake = fake_ollama(FakeResponse::Chat(TOOL_CALL_ONLY_RESPONSE.to_owned())).await;
        let error = test_triager(fake.addr)
            .triage(&test_alert())
            .await
            .expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Output);
    }

    #[tokio::test]
    async fn empty_message_is_model_error() {
        let fake = fake_ollama(FakeResponse::Chat(EMPTY_MESSAGE_RESPONSE.to_owned())).await;
        let error = test_triager(fake.addr)
            .triage(&test_alert())
            .await
            .expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Model);
    }

    #[tokio::test]
    async fn http_error_is_model_error() {
        let fake = fake_ollama(FakeResponse::Status(500)).await;
        let error = test_triager(fake.addr)
            .triage(&test_alert())
            .await
            .expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Model);
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_transport_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        drop(listener);

        let error = test_triager(addr)
            .triage(&test_alert())
            .await
            .expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Transport);
    }

    #[tokio::test]
    async fn timeout_is_transport_error() {
        let fake = fake_ollama(FakeResponse::Hang).await;
        let mut triager = test_triager(fake.addr);
        triager.timeout = Duration::from_millis(100);
        let error = triager.triage(&test_alert()).await.expect_err("must fail");
        assert_eq!(error.class(), ErrorClass::Transport);
        assert!(
            error.to_string().contains("100ms"),
            "error must name the configured timeout, got: {error}"
        );
    }

    #[test]
    fn provider_side_completion_errors_classify_as_model() {
        let json_error =
            serde_json::from_str::<serde_json::Value>("not json").expect_err("invalid json");
        assert_eq!(
            TriageError::Completion(CompletionError::JsonError(json_error)).class(),
            ErrorClass::Model
        );
        assert_eq!(
            TriageError::Completion(CompletionError::ProviderError(
                "model not pulled".to_owned()
            ))
            .class(),
            ErrorClass::Model
        );
        assert_eq!(
            TriageError::Completion(CompletionError::ResponseError(
                "no assistant message".to_owned()
            ))
            .class(),
            ErrorClass::Model
        );
    }
}
