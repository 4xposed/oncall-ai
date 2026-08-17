use super::{Step, TokenUsage, ToolFailure, ToolOutcome, Truncated};

use rig_agent::agent::{
    AgentHook, HookContext, InvalidToolCallAction, InvalidToolCallContext, ModelTurnAction,
    ModelTurnFinished, ToolResultAction, ToolResultEvent,
};
use rig_agent::tool::{ToolErrorKind, ToolResult};
use rig_core::completion::message::ReasoningContent;
use rig_core::completion::{AssistantContent, Usage};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct State {
    steps: Vec<Step>,
    usage: TokenUsage,
}

/// Locked only momentarily from the hooks, never across an await.
#[derive(Debug)]
pub struct Recorder(Arc<Mutex<State>>);

impl Recorder {
    #[must_use]
    pub fn new() -> (Self, Recording) {
        let state = Arc::new(Mutex::new(State::default()));
        (Self(Arc::clone(&state)), Recording(state))
    }

    fn push(&self, step: Step) {
        self.0.lock().expect("recorder lock").steps.push(step);
    }

    fn record_turn(&self, event: ModelTurnFinished<'_>) -> ModelTurnAction {
        add(
            &mut self.0.lock().expect("recorder lock").usage,
            event.usage,
        );
        for step in reasoning(event) {
            self.push(step);
        }
        ModelTurnAction::Continue
    }

    fn record_tool_result(&self, turn: usize, event: &ToolResultEvent<'_>) -> ToolResultAction {
        self.push(tool_step(turn, event));
        ToolResultAction::Keep
    }

    fn record_invalid_tool_call(
        &self,
        turn: usize,
        name: &str,
        raw_args: Option<&str>,
    ) -> Option<InvalidToolCallAction> {
        self.push(Step::ToolCall {
            turn,
            name: name.to_owned(),
            args: raw_args.map_or(serde_json::Value::Null, args),
            result: "the model emitted a tool call that could not be dispatched".to_owned(),
            outcome: ToolOutcome::Failed {
                kind: ToolFailure::InvalidArgs,
            },
            truncated: false,
        });
        None
    }
}

impl AgentHook for Recorder {
    fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> impl std::future::Future<Output = ModelTurnAction> {
        std::future::ready(self.record_turn(event))
    }

    fn on_tool_result(
        &self,
        ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> impl std::future::Future<Output = ToolResultAction> {
        std::future::ready(self.record_tool_result(ctx.turn(), &event))
    }

    fn on_invalid_tool_call(
        &self,
        ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> impl std::future::Future<Output = Option<InvalidToolCallAction>> {
        std::future::ready(self.record_invalid_tool_call(
            ctx.turn(),
            &event.tool_name,
            event.args.as_deref(),
        ))
    }
}

/// The other half of the state, which is what preserves the steps of a run
/// that died mid-loop: they outlive the recorder the agent consumed.
#[derive(Debug)]
pub struct Recording(Arc<Mutex<State>>);

impl Recording {
    #[must_use]
    pub fn drain(self) -> (Vec<Step>, TokenUsage) {
        let state = std::mem::take(&mut *self.0.lock().expect("recorder lock"));
        (state.steps, state.usage)
    }
}

fn add(total: &mut TokenUsage, usage: Usage) {
    total.input_tokens += usage.input_tokens;
    total.cached_input_tokens += usage.cached_input_tokens;
    total.cache_creation_input_tokens += usage.cache_creation_input_tokens;
    total.output_tokens += usage.output_tokens;
    total.total_tokens += usage.total_tokens;
}

fn reasoning(event: ModelTurnFinished<'_>) -> impl Iterator<Item = Step> + '_ {
    event.content.iter().flat_map(move |block| {
        thoughts(block)
            .into_iter()
            .map(move |text| Step::Reasoning {
                turn: event.turn,
                text: text.to_owned(),
            })
    })
}

fn thoughts(block: &AssistantContent) -> Vec<&str> {
    match block {
        AssistantContent::Text(text) => vec![text.text.as_str()],
        AssistantContent::Reasoning(reasoning) => reasoning
            .content
            .iter()
            .filter_map(|content| match content {
                ReasoningContent::Text { text, .. } | ReasoningContent::Summary(text) => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn tool_step(turn: usize, event: &ToolResultEvent<'_>) -> Step {
    let result = event.presentation.render();
    Step::ToolCall {
        turn,
        name: event.tool_name.to_owned(),
        args: args(event.args),
        truncated: event
            .tool_context
            .result::<Truncated>()
            .is_some_and(|truncated| truncated.0),
        outcome: classify(event.raw_result),
        result,
    }
}

/// Arguments rig could not parse are kept verbatim rather than dropped.
fn args(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

fn classify(result: &ToolResult) -> ToolOutcome {
    if let Some(error) = result.error().or_else(|| result.refusal()) {
        return ToolOutcome::Failed {
            kind: failure(error.kind()),
        };
    }
    if result.is_success() {
        return ToolOutcome::Succeeded;
    }
    ToolOutcome::Failed {
        kind: ToolFailure::Other,
    }
}

fn failure(kind: ToolErrorKind) -> ToolFailure {
    match kind {
        ToolErrorKind::InvalidArgs => ToolFailure::InvalidArgs,
        ToolErrorKind::Timeout => ToolFailure::Timeout,
        ToolErrorKind::Cancelled => ToolFailure::Cancelled,
        ToolErrorKind::NotFound => ToolFailure::NotFound,
        ToolErrorKind::PermissionDenied => ToolFailure::PermissionDenied,
        ToolErrorKind::RateLimited => ToolFailure::RateLimited,
        ToolErrorKind::Provider => ToolFailure::Provider,
        ToolErrorKind::Network => ToolFailure::Network,
        // `ToolErrorKind` is `#[non_exhaustive]`: kinds rig adds land here.
        _ => ToolFailure::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::tool::{ToolContext, ToolExecutionError, ToolOutput};
    use rig_core::OneOrMany;
    use rig_core::completion::AssistantContent;

    fn tokens(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_input_tokens: 1,
            cache_creation_input_tokens: 2,
            ..Usage::new()
        }
    }

    fn event<'a>(
        raw: &'a ToolResult,
        presentation: &'a ToolOutput,
        context: &'a ToolContext,
        args: &'a str,
    ) -> ToolResultEvent<'a> {
        ToolResultEvent {
            tool_name: "read_file",
            tool_call_id: None,
            internal_call_id: "call-1",
            args,
            presentation,
            raw_result: raw,
            tool_context: context,
        }
    }

    fn recorded_with(raw: &ToolResult, args: &str, context: &ToolContext) -> Step {
        let presentation = raw.output().clone();
        tool_step(1, &event(raw, &presentation, context, args))
    }

    fn recorded(raw: &ToolResult, args: &str) -> Step {
        recorded_with(raw, args, &ToolContext::new())
    }

    fn outcome_of(step: &Step) -> Option<ToolOutcome> {
        match step {
            Step::ToolCall { outcome, .. } => Some(*outcome),
            Step::Reasoning { .. } => None,
        }
    }

    fn step_turn(step: &Step) -> usize {
        match step {
            Step::ToolCall { turn, .. } | Step::Reasoning { turn, .. } => *turn,
        }
    }

    #[test]
    fn every_rig_failure_kind_maps_to_a_transcript_kind() {
        let cases = [
            (ToolErrorKind::InvalidArgs, ToolFailure::InvalidArgs),
            (ToolErrorKind::Timeout, ToolFailure::Timeout),
            (ToolErrorKind::Cancelled, ToolFailure::Cancelled),
            (ToolErrorKind::NotFound, ToolFailure::NotFound),
            (
                ToolErrorKind::PermissionDenied,
                ToolFailure::PermissionDenied,
            ),
            (ToolErrorKind::RateLimited, ToolFailure::RateLimited),
            (ToolErrorKind::Provider, ToolFailure::Provider),
            (ToolErrorKind::Network, ToolFailure::Network),
            (ToolErrorKind::Other, ToolFailure::Other),
        ];
        for (rig_kind, expected) in cases {
            let raw = ToolResult::failed(ToolExecutionError::new(rig_kind, "boom"));
            assert_eq!(
                outcome_of(&recorded(&raw, "{}")),
                Some(ToolOutcome::Failed { kind: expected }),
                "{rig_kind} must be recorded as {expected:?}"
            );
        }
    }

    #[test]
    fn a_successful_result_is_recorded_as_succeeded() {
        let raw = ToolResult::success(ToolOutput::text("max_connections = 4\n"));
        assert_eq!(
            outcome_of(&recorded(&raw, "{}")),
            Some(ToolOutcome::Succeeded)
        );
    }

    /// rig's `error()` is `None` for a refusal, so reading it alone would
    /// record a refused call as a success.
    #[test]
    fn a_refusal_is_recorded_as_a_failure() {
        let raw = ToolResult::failed(ToolExecutionError::refused("not that file"));
        assert!(raw.error().is_none(), "rig hides refusals from error()");
        assert_eq!(
            outcome_of(&recorded(&raw, "{}")),
            Some(ToolOutcome::Failed {
                kind: ToolFailure::PermissionDenied
            })
        );
    }

    /// A skipped call never ran, so it has no error and is not a success.
    #[test]
    fn a_skipped_call_is_not_recorded_as_a_success() {
        let raw = ToolResult::skipped("a hook skipped this call");
        assert_eq!(
            outcome_of(&recorded(&raw, "{}")),
            Some(ToolOutcome::Failed {
                kind: ToolFailure::Other
            })
        );
    }

    /// The presentation is what the model read; the outcome comes from the raw
    /// result, which no rewrite can touch.
    #[test]
    fn a_step_records_what_the_model_saw_and_the_raw_outcome() {
        let raw = ToolResult::failed(ToolExecutionError::not_found("no such file"));
        let presentation = ToolOutput::text("rewritten by another hook");
        let context = ToolContext::new();
        let step = tool_step(
            4,
            &event(&raw, &presentation, &context, r#"{"path":"a.txt"}"#),
        );
        assert_eq!(
            step,
            Step::ToolCall {
                turn: 4,
                name: "read_file".to_owned(),
                args: serde_json::json!({ "path": "a.txt" }),
                result: "rewritten by another hook".to_owned(),
                outcome: ToolOutcome::Failed {
                    kind: ToolFailure::NotFound
                },
                truncated: false,
            }
        );
    }

    #[test]
    fn tool_args_that_are_not_json_are_kept_verbatim() {
        let raw = ToolResult::success(ToolOutput::text("ok"));
        assert_eq!(
            recorded(&raw, "path=a.txt"),
            Step::ToolCall {
                turn: 1,
                name: "read_file".to_owned(),
                args: serde_json::Value::String("path=a.txt".to_owned()),
                result: "ok".to_owned(),
                outcome: ToolOutcome::Succeeded,
                truncated: false,
            }
        );
    }

    /// The transcript must state that the model saw only part of the file, on
    /// the tool's word rather than on what its output happens to say.
    #[test]
    fn a_truncated_read_is_flagged_from_result_metadata() {
        let raw = ToolResult::success(ToolOutput::text("max_connections = 4\n"));
        let mut context = ToolContext::new();
        context.insert_result(Truncated(true));
        assert_eq!(
            recorded_with(&raw, "{}", &context),
            Step::ToolCall {
                turn: 1,
                name: "read_file".to_owned(),
                args: serde_json::json!({}),
                result: "max_connections = 4\n".to_owned(),
                outcome: ToolOutcome::Succeeded,
                truncated: true,
            }
        );
    }

    /// The failure mode a string-sniff would have: a file whose own contents
    /// end in the notice was not truncated.
    #[test]
    fn output_that_merely_looks_truncated_is_not_flagged() {
        let raw = ToolResult::success(ToolOutput::text(
            "log line\n[truncated: first 8 of 99 bytes shown]",
        ));
        let mut context = ToolContext::new();
        context.insert_result(Truncated(false));
        assert_eq!(
            recorded_with(&raw, "{}", &context),
            Step::ToolCall {
                turn: 1,
                name: "read_file".to_owned(),
                args: serde_json::json!({}),
                result: "log line\n[truncated: first 8 of 99 bytes shown]".to_owned(),
                outcome: ToolOutcome::Succeeded,
                truncated: false,
            }
        );
    }

    #[test]
    fn every_text_block_becomes_a_reasoning_step_for_its_turn() {
        let content = OneOrMany::many([
            AssistantContent::text("first thought"),
            AssistantContent::text("second thought"),
        ])
        .expect("non-empty content");
        let steps: Vec<Step> = reasoning(ModelTurnFinished {
            turn: 3,
            content: &content,
            usage: Usage::new(),
        })
        .collect();
        assert_eq!(
            steps,
            vec![
                Step::Reasoning {
                    turn: 3,
                    text: "first thought".to_owned()
                },
                Step::Reasoning {
                    turn: 3,
                    text: "second thought".to_owned()
                },
            ]
        );
    }

    /// rig's Ollama adapter splits a thinking model's output into `Reasoning`
    /// blocks, and the default investigation model is a thinking model; reading
    /// only `Text` would empty the transcript's reasoning half.
    #[test]
    fn a_thinking_block_becomes_a_reasoning_step() {
        let content = OneOrMany::many([
            AssistantContent::reasoning("the alert names checkout-api"),
            AssistantContent::text("reading its pool config"),
        ])
        .expect("non-empty content");
        let steps: Vec<Step> = reasoning(ModelTurnFinished {
            turn: 2,
            content: &content,
            usage: Usage::new(),
        })
        .collect();
        assert_eq!(
            steps,
            vec![
                Step::Reasoning {
                    turn: 2,
                    text: "the alert names checkout-api".to_owned()
                },
                Step::Reasoning {
                    turn: 2,
                    text: "reading its pool config".to_owned()
                },
            ]
        );
    }

    /// An encrypted payload is not readable reasoning; recording it would put
    /// an opaque blob where an auditor expects prose.
    #[test]
    fn an_opaque_reasoning_payload_is_not_recorded() {
        let content = OneOrMany::one(AssistantContent::Reasoning(
            rig_core::completion::message::Reasoning::encrypted("AAAA"),
        ));
        let steps: Vec<Step> = reasoning(ModelTurnFinished {
            turn: 1,
            content: &content,
            usage: Usage::new(),
        })
        .collect();
        assert!(steps.is_empty(), "got: {steps:?}");
    }

    /// A tool call is recorded when it resolves, not when it is requested.
    #[test]
    fn a_requested_tool_call_is_not_reasoning() {
        let content = OneOrMany::many([
            AssistantContent::text("reading the config"),
            AssistantContent::tool_call("id-1", "read_file", serde_json::json!({ "path": "a" })),
        ])
        .expect("non-empty content");
        let steps: Vec<Step> = reasoning(ModelTurnFinished {
            turn: 1,
            content: &content,
            usage: Usage::new(),
        })
        .collect();
        assert_eq!(
            steps,
            vec![Step::Reasoning {
                turn: 1,
                text: "reading the config".to_owned()
            }]
        );
    }

    #[test]
    fn a_finished_turn_is_recorded_and_accepted() {
        let (recorder, recording) = Recorder::new();
        let content = OneOrMany::one(AssistantContent::text("thinking"));
        for usage in [tokens(10, 5), tokens(20, 7)] {
            let action = recorder.record_turn(ModelTurnFinished {
                turn: 1,
                content: &content,
                usage,
            });
            assert_eq!(action, ModelTurnAction::Continue, "the hook must not steer");
        }
        let (steps, total) = recording.drain();
        assert_eq!(steps.len(), 2);
        assert_eq!(
            total,
            TokenUsage {
                input_tokens: 30,
                cached_input_tokens: 2,
                cache_creation_input_tokens: 4,
                output_tokens: 12,
                total_tokens: 42,
            }
        );
    }

    #[test]
    fn a_tool_result_is_recorded_and_left_as_the_model_saw_it() {
        let (recorder, recording) = Recorder::new();
        let raw = ToolResult::success(ToolOutput::text("max_connections = 4\n"));
        let presentation = raw.output().clone();
        let context = ToolContext::new();
        let action =
            recorder.record_tool_result(2, &event(&raw, &presentation, &context, r#"{"a":1}"#));
        assert_eq!(action, ToolResultAction::Keep, "the hook must not steer");
        let (steps, _) = recording.drain();
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps.first().and_then(outcome_of),
            Some(ToolOutcome::Succeeded)
        );
    }

    #[test]
    fn an_invalid_tool_call_is_recorded_and_left_to_the_runner() {
        let (recorder, recording) = Recorder::new();
        let action = recorder.record_invalid_tool_call(6, "reed_file", Some(r#"{"path":"a.txt"}"#));
        assert!(action.is_none(), "the hook must not steer");
        let (steps, _) = recording.drain();
        assert_eq!(
            steps.first().and_then(outcome_of),
            Some(ToolOutcome::Failed {
                kind: ToolFailure::InvalidArgs
            })
        );
        assert_eq!(
            steps.first().map(step_turn),
            Some(6),
            "the fumbled turn must be visible"
        );
    }

    /// What preserves a partial transcript: the steps outlive the recorder.
    #[test]
    fn draining_yields_what_was_recorded_before_the_recorder_was_dropped() {
        let (recorder, recording) = Recorder::new();
        recorder.push(Step::Reasoning {
            turn: 1,
            text: "thinking".to_owned(),
        });
        drop(recorder);
        let (steps, _) = recording.drain();
        assert_eq!(
            steps,
            vec![Step::Reasoning {
                turn: 1,
                text: "thinking".to_owned()
            }]
        );
    }
}
