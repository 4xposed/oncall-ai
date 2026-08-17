use super::ModelId;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt as _;
use rig_core::client::CompletionClient;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, GetTokenUsage, Usage,
};
use rig_core::streaming::{
    RawStreamingChoice, RawStreamingToolCall, StreamedAssistantContent,
    StreamingCompletionResponse, StreamingResult,
};
use serde::{Deserialize, Serialize};

type CompletionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletionResponse<()>, CompletionError>> + Send + 'a>>;
type StreamFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = Result<
                    StreamingCompletionResponse<ErasedStreamingResponse>,
                    CompletionError,
                >,
            > + Send
            + 'a,
    >,
>;

trait ErasedCompletionModel: Send + Sync {
    fn completion(&self, request: CompletionRequest) -> CompletionFuture<'_>;
    fn stream(&self, request: CompletionRequest) -> StreamFuture<'_>;
    fn composes_native_output_with_tools(&self) -> bool;
}

impl<M> ErasedCompletionModel for M
where
    M: CompletionModel + Send + Sync + 'static,
{
    fn completion(&self, request: CompletionRequest) -> CompletionFuture<'_> {
        Box::pin(async move {
            let response = <M as CompletionModel>::completion(self, request).await?;
            Ok(CompletionResponse {
                choice: response.choice,
                usage: response.usage,
                raw_response: (),
                message_id: response.message_id,
            })
        })
    }

    fn stream(&self, request: CompletionRequest) -> StreamFuture<'_> {
        Box::pin(async move {
            let response = <M as CompletionModel>::stream(self, request).await?;
            Ok(erase_stream(response))
        })
    }

    fn composes_native_output_with_tools(&self) -> bool {
        <M as CompletionModel>::composes_native_output_with_tools(self)
    }
}

/// A completion model whose provider-specific success response has been erased.
#[derive(Clone)]
pub struct AnyCompletionModel {
    inner: Arc<dyn ErasedCompletionModel>,
}

impl AnyCompletionModel {
    /// Erases a concrete Rig completion model.
    #[must_use]
    pub fn new<M>(model: M) -> Self
    where
        M: CompletionModel + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(model),
        }
    }
}

impl fmt::Debug for AnyCompletionModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AnyCompletionModel")
    }
}

impl CompletionModel for AnyCompletionModel {
    type Response = ();
    type StreamingResponse = ErasedStreamingResponse;
    type Client = AnyCompletionClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        client.make_model(&model.into())
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        self.inner.completion(request).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        self.inner.stream(request).await
    }

    fn composes_native_output_with_tools(&self) -> bool {
        self.inner.composes_native_output_with_tools()
    }
}

type ModelFactory = dyn Fn(&str) -> AnyCompletionModel + Send + Sync;

/// A provider client reduced to its completion-model factory.
#[derive(Clone)]
pub struct AnyCompletionClient {
    model_factory: Arc<ModelFactory>,
}

impl AnyCompletionClient {
    /// Erases a concrete completion-capable Rig client.
    #[must_use]
    pub fn new<C>(client: C) -> Self
    where
        C: CompletionClient + Send + Sync + 'static,
        C::CompletionModel: Send + Sync + 'static,
    {
        Self {
            model_factory: Arc::new(move |model| {
                AnyCompletionModel::new(client.completion_model(model.to_owned()))
            }),
        }
    }

    /// Creates an erased model using a validated model identifier.
    #[must_use]
    pub fn completion_model(&self, model: &ModelId) -> AnyCompletionModel {
        self.make_model(model.as_str())
    }

    fn make_model(&self, model: &str) -> AnyCompletionModel {
        (self.model_factory)(model)
    }
}

impl fmt::Debug for AnyCompletionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AnyCompletionClient")
    }
}

impl CompletionClient for AnyCompletionClient {
    type CompletionModel = AnyCompletionModel;
}

/// The provider-neutral final response carried by an erased public stream.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErasedStreamingResponse {
    usage: Usage,
}

impl GetTokenUsage for ErasedStreamingResponse {
    fn token_usage(&self) -> Usage {
        self.usage
    }
}

fn erase_stream<R>(
    response: StreamingCompletionResponse<R>,
) -> StreamingCompletionResponse<ErasedStreamingResponse>
where
    R: Clone + Unpin + GetTokenUsage + Send + 'static,
{
    let stream = futures::stream::unfold(
        (response, false),
        |(mut response, message_id_emitted)| async move {
            if message_id_emitted {
                return None;
            }
            loop {
                match response.next().await {
                    Some(Ok(event)) => {
                        if let Some(event) = erase_stream_event(event) {
                            return Some((Ok(event), (response, false)));
                        }
                    }
                    Some(Err(error)) => return Some((Err(error), (response, false))),
                    None => {
                        let message_id = response.message_id.take()?;
                        return Some((
                            Ok(RawStreamingChoice::MessageId(message_id)),
                            (response, true),
                        ));
                    }
                }
            }
        },
    );
    let stream: StreamingResult<ErasedStreamingResponse> = Box::pin(stream);
    StreamingCompletionResponse::stream(stream)
}

fn erase_stream_event<R>(
    event: StreamedAssistantContent<R>,
) -> Option<RawStreamingChoice<ErasedStreamingResponse>>
where
    R: GetTokenUsage,
{
    match event {
        StreamedAssistantContent::Text(text) => Some(RawStreamingChoice::Message(text.text)),
        StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        } => Some(RawStreamingChoice::ToolCall(RawStreamingToolCall {
            id: tool_call.id,
            internal_call_id,
            call_id: tool_call.call_id,
            name: tool_call.function.name,
            arguments: tool_call.function.arguments,
            signature: tool_call.signature,
            additional_params: tool_call.additional_params,
        })),
        StreamedAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        } => Some(RawStreamingChoice::ToolCallDelta {
            id,
            internal_call_id,
            content,
        }),
        StreamedAssistantContent::Reasoning(reasoning) => {
            let content = reasoning.content.into_iter().next()?;
            Some(RawStreamingChoice::Reasoning {
                id: reasoning.id,
                content,
            })
        }
        StreamedAssistantContent::ReasoningDelta { id, reasoning } => {
            Some(RawStreamingChoice::ReasoningDelta { id, reasoning })
        }
        StreamedAssistantContent::Final(response) => {
            Some(RawStreamingChoice::FinalResponse(ErasedStreamingResponse {
                usage: response.token_usage(),
            }))
        }
        StreamedAssistantContent::Unknown(value) => Some(RawStreamingChoice::Unknown(value)),
    }
}
