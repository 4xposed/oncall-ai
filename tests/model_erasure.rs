use futures::StreamExt as _;
use rig_core::OneOrMany;
use rig_core::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
    GetTokenUsage, Usage,
};
use rig_core::streaming::{StreamedAssistantContent, StreamingCompletionResponse, StreamingResult};
use rig_core::test_utils::{MockCompletionModel, MockResponse, MockStreamEvent, MockTurn};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use oncall_ai::model::{AnyCompletionModel, ErasedStreamingResponse};

#[tokio::test]
async fn completion_erasure_preserves_normalized_response_fields() {
    let usage = Usage {
        input_tokens: 3,
        output_tokens: 5,
        total_tokens: 8,
        ..Usage::default()
    };
    let turn = MockTurn::from_contents([
        AssistantContent::text("answer"),
        AssistantContent::reasoning("because"),
        AssistantContent::tool_call("call-1", "read_file", serde_json::json!({"path":"db.rs"})),
    ])
    .expect("non-empty content")
    .with_usage(usage)
    .with_message_id("message-1");
    let model = AnyCompletionModel::new(MockCompletionModel::new([turn]));

    let response = model
        .completion(model.completion_request("alert").build())
        .await
        .expect("completion succeeds");

    assert_eq!(response.choice.len(), 3);
    assert_eq!(response.usage, usage);
    assert_eq!(response.message_id.as_deref(), Some("message-1"));
    assert_eq!(response.raw_response, ());
}

#[tokio::test]
async fn completion_erasure_preserves_completion_errors() {
    let model = AnyCompletionModel::new(MockCompletionModel::new([MockTurn::error(
        "provider failed",
    )]));

    let error = model
        .completion(model.completion_request("alert").build())
        .await
        .expect_err("completion must fail");

    assert!(matches!(
        error,
        CompletionError::ProviderError(message) if message == "provider failed"
    ));
}

#[test]
fn different_raw_response_types_share_one_model_type() {
    let models = [
        AnyCompletionModel::new(MockCompletionModel::default()),
        AnyCompletionModel::new(NativeModel),
    ];

    assert!(!models[0].composes_native_output_with_tools());
    assert!(models[1].composes_native_output_with_tools());
}

#[tokio::test]
async fn stream_erasure_preserves_public_events() {
    let model = AnyCompletionModel::new(MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("answer"),
        MockStreamEvent::reasoning("because"),
        MockStreamEvent::tool_call("call-1", "read_file", serde_json::json!({"path":"db.rs"})),
        MockStreamEvent::tool_call_name_delta("call-2", "internal-2", "read"),
        MockStreamEvent::reasoning_delta(None::<String>, "still thinking"),
        MockStreamEvent::unknown(serde_json::json!({"type":"hosted_tool"})),
        MockStreamEvent::FinalResponse(MockResponse::with_total_tokens(13)),
    ]]));
    let mut stream = model
        .stream(model.completion_request("alert").build())
        .await
        .expect("stream starts");
    let mut kinds = Vec::new();

    while let Some(event) = stream.next().await {
        kinds.push(match event.expect("stream event succeeds") {
            StreamedAssistantContent::Text(_) => "text",
            StreamedAssistantContent::Reasoning(_) => "reasoning",
            StreamedAssistantContent::ToolCall { .. } => "tool_call",
            StreamedAssistantContent::Unknown(_) => "unknown",
            StreamedAssistantContent::Final(_) => "final",
            StreamedAssistantContent::ToolCallDelta { .. }
            | StreamedAssistantContent::ReasoningDelta { .. } => "delta",
        });
    }

    assert_eq!(
        kinds,
        [
            "text",
            "reasoning",
            "tool_call",
            "delta",
            "delta",
            "unknown",
            "final"
        ]
    );
}

#[tokio::test]
async fn stream_erasure_preserves_final_usage_and_message_id() {
    let model = AnyCompletionModel::new(MockCompletionModel::from_stream_turns([[
        MockStreamEvent::MessageId("message-2".to_owned()),
        MockStreamEvent::FinalResponse(MockResponse::with_total_tokens(21)),
    ]]));
    let mut stream = model
        .stream(model.completion_request("alert").build())
        .await
        .expect("stream starts");

    while stream.next().await.is_some() {}

    assert_eq!(stream.usage().total_tokens, 21);
    assert_eq!(stream.message_id.as_deref(), Some("message-2"));
}

#[tokio::test]
async fn stream_erasure_preserves_provider_errors() {
    let model = AnyCompletionModel::new(MockCompletionModel::from_stream_turns([[
        MockStreamEvent::error("stream failed"),
    ]]));
    let mut stream = model
        .stream(model.completion_request("alert").build())
        .await
        .expect("stream starts");

    let error = stream
        .next()
        .await
        .expect("error event exists")
        .expect_err("event must fail");

    assert!(matches!(
        error,
        CompletionError::ProviderError(message) if message == "stream failed"
    ));
}

#[tokio::test]
async fn dropping_erased_stream_drops_the_provider_stream() {
    let dropped = Arc::new(AtomicBool::new(false));
    let model = AnyCompletionModel::new(DroppingStreamModel {
        dropped: Arc::clone(&dropped),
    });
    let stream = model
        .stream(model.completion_request("alert").build())
        .await
        .expect("stream starts");

    assert!(!dropped.load(Ordering::SeqCst));
    drop(stream);
    assert!(dropped.load(Ordering::SeqCst));
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct NativeResponse;

impl GetTokenUsage for NativeResponse {
    fn token_usage(&self) -> Usage {
        Usage::default()
    }
}

#[derive(Clone)]
struct NativeModel;

impl CompletionModel for NativeModel {
    type Response = NativeResponse;
    type StreamingResponse = NativeResponse;
    type Client = ();

    fn make((): &Self::Client, _: impl Into<String>) -> Self {
        Self
    }

    fn completion(
        &self,
        _request: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse<Self::Response>, CompletionError>> {
        std::future::ready(Ok(CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text("native")),
            usage: Usage::default(),
            raw_response: NativeResponse,
            message_id: None,
        }))
    }

    fn stream(
        &self,
        _request: CompletionRequest,
    ) -> impl Future<
        Output = Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError>,
    > {
        let stream: StreamingResult<NativeResponse> = Box::pin(futures::stream::empty());
        std::future::ready(Ok(StreamingCompletionResponse::stream(stream)))
    }

    fn composes_native_output_with_tools(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct DroppingStreamModel {
    dropped: Arc<AtomicBool>,
}

impl CompletionModel for DroppingStreamModel {
    type Response = NativeResponse;
    type StreamingResponse = NativeResponse;
    type Client = ();

    fn make((): &Self::Client, _: impl Into<String>) -> Self {
        Self {
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn completion(
        &self,
        _request: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse<Self::Response>, CompletionError>> {
        std::future::ready(Ok(CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text("unused")),
            usage: Usage::default(),
            raw_response: NativeResponse,
            message_id: None,
        }))
    }

    fn stream(
        &self,
        _request: CompletionRequest,
    ) -> impl Future<
        Output = Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError>,
    > {
        let stream: StreamingResult<NativeResponse> = Box::pin(PendingDropStream {
            dropped: Arc::clone(&self.dropped),
        });
        std::future::ready(Ok(StreamingCompletionResponse::stream(stream)))
    }
}

struct PendingDropStream {
    dropped: Arc<AtomicBool>,
}

impl futures::Stream for PendingDropStream {
    type Item = Result<rig_core::streaming::RawStreamingChoice<NativeResponse>, CompletionError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for PendingDropStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn _assert_erased_stream_response_is_usage(response: &ErasedStreamingResponse) -> Usage {
    response.token_usage()
}
