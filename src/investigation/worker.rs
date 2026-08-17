use super::read_file::{self, ReadFile};
use super::recorder::Recorder;
use super::{
    Evidence, Hypothesis, INVESTIGATION_PREAMBLE, Investigation, InvestigationRequest, Outcome,
    Step, Transcript, render_prompt,
};

use crate::config::InvestigationConfig;
use crate::model::AnyCompletionModel;

use rig_agent::agent::{
    Agent, AgentBuilder, AgentHook, HookContext, InvalidToolCallAction, InvalidToolCallContext,
    PromptResponse,
};
use rig_agent::completion::{Prompt as _, PromptError};
use rig_core::completion::CompletionModel;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// An error from building the investigation agent.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct BuildError(#[from] read_file::BuildError);

/// Builds an investigation agent from a provider-neutral completion model.
///
/// # Errors
///
/// Fails when `repo_root` cannot be resolved.
pub fn build_agent(
    model: AnyCompletionModel,
    config: &InvestigationConfig,
) -> Result<(Agent<AnyCompletionModel>, PathBuf), BuildError> {
    let read_file = ReadFile::new(config)?;
    let repo_root = read_file.repo_root().to_path_buf();
    let mut builder = AgentBuilder::new(model)
        .preamble(INVESTIGATION_PREAMBLE)
        .tool(read_file)
        .output_schema::<Hypothesis>();
    if let Some(temperature) = config.temperature {
        builder = builder.temperature(temperature);
    }
    Ok((builder.build(), repo_root))
}

pub async fn worker<M>(
    agent: Agent<M>,
    config: InvestigationConfig,
    mut requests_rx: tokio::sync::mpsc::Receiver<InvestigationRequest>,
    done_tx: tokio::sync::mpsc::Sender<Investigation>,
    shutdown: CancellationToken,
) -> usize
where
    M: CompletionModel + 'static,
{
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
        let run = std::pin::pin!(investigate(&agent, &config, request));
        let investigation = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                dropped += 1;
                break;
            }
            investigation = run => investigation,
        };
        if done_tx.send(investigation).await.is_err() {
            tracing::warn!("investigation result dropped; incident worker gone");
        }
    }
    while requests_rx.try_recv().is_ok() {
        dropped += 1;
    }
    tracing::info!(dropped, "investigation queue drained");
    dropped
}

async fn investigate<M>(
    agent: &Agent<M>,
    config: &InvestigationConfig,
    request: InvestigationRequest,
) -> Investigation
where
    M: CompletionModel + 'static,
{
    let (recorder, recording) = Recorder::new();
    let started = tokio::time::Instant::now();
    let run = agent
        .prompt(render_prompt(&request, repository(config)))
        .max_turns(config.max_turns.get())
        .extended_details()
        .add_hook(recorder)
        .add_hook(RetryInvalidToolCalls)
        .max_invalid_tool_call_retries(INVALID_TOOL_CALL_RETRIES);

    let result = tokio::time::timeout(config.timeout(), run).await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let (mut outcome, response, mut cause) = classify(result);
    let (steps, usage) = recording.drain();

    let hypothesis = match response
        .as_ref()
        .map(|response| serde_json::from_str::<Hypothesis>(response.output()))
    {
        Some(Ok(hypothesis)) => Some(hypothesis),
        Some(Err(error)) => {
            outcome = Outcome::Failed;
            cause = Some(format!("output is not a Hypothesis: {error}"));
            None
        }
        None => None,
    };

    let transcript = Transcript {
        incident: request.incident,
        model: config.model.to_string(),
        outcome,
        usage,
        requests: response
            .as_ref()
            .map_or_else(|| turns(&steps), PromptResponse::requests),
        duration_ms,
        steps,
    };
    let unverified_evidence = hypothesis
        .as_ref()
        .map(|hypothesis| unverified(hypothesis, &transcript))
        .unwrap_or_default();

    let investigation = Investigation {
        transcript,
        hypothesis,
        unverified_evidence,
    };
    log_outcome(&investigation, cause.as_deref());
    investigation
}

const INVALID_TOOL_CALL_RETRIES: usize = 2;

struct RetryInvalidToolCalls;

impl AgentHook for RetryInvalidToolCalls {
    fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> impl std::future::Future<Output = Option<InvalidToolCallAction>> {
        std::future::ready(Some(InvalidToolCallAction::retry(format!(
            "there is no tool named `{}`; call one of: {}",
            event.tool_name,
            event.available_tools.join(", ")
        ))))
    }
}

fn classify(
    result: Result<Result<PromptResponse, PromptError>, tokio::time::error::Elapsed>,
) -> (Outcome, Option<PromptResponse>, Option<String>) {
    match result {
        Ok(Ok(response)) => (Outcome::Completed, Some(response), None),
        Ok(Err(error @ PromptError::MaxTurnsError { .. })) => {
            (Outcome::MaxTurnsExhausted, None, Some(error.to_string()))
        }
        Ok(Err(error)) => (Outcome::Failed, None, Some(error.to_string())),
        Err(_elapsed) => (Outcome::TimedOut, None, None),
    }
}

fn turns(steps: &[Step]) -> usize {
    steps
        .iter()
        .map(|step| match step {
            Step::Reasoning { turn, .. } | Step::ToolCall { turn, .. } => *turn,
        })
        .max()
        .unwrap_or(0)
}

fn repository(config: &InvestigationConfig) -> Option<&str> {
    config.repo_root.file_name().and_then(|name| name.to_str())
}

fn unverified(hypothesis: &Hypothesis, transcript: &Transcript) -> Vec<Evidence> {
    hypothesis
        .evidence
        .iter()
        .filter(|evidence| !is_quoted(&evidence.quote, transcript))
        .cloned()
        .collect()
}

fn is_quoted(quote: &str, transcript: &Transcript) -> bool {
    let quote = quote.trim();
    !quote.is_empty()
        && transcript
            .tool_results()
            .any(|result| result.contains(quote))
}

fn log_outcome(investigation: &Investigation, cause: Option<&str>) {
    let transcript = &investigation.transcript;
    let unverified = investigation.unverified_evidence.len();
    match transcript.outcome {
        Outcome::Completed => tracing::info!(
            incident_id = %transcript.incident,
            model = transcript.model,
            requests = transcript.requests,
            steps = transcript.steps.len(),
            total_tokens = transcript.usage.total_tokens,
            duration_ms = transcript.duration_ms,
            confidence = ?investigation.hypothesis.as_ref().map(|h| h.confidence),
            unverified_evidence = unverified,
            "incident investigated"
        ),
        outcome => tracing::error!(
            incident_id = %transcript.incident,
            model = transcript.model,
            outcome = ?outcome,
            cause,
            requests = transcript.requests,
            steps = transcript.steps.len(),
            total_tokens = transcript.usage.total_tokens,
            duration_ms = transcript.duration_ms,
            "investigation abandoned; incident stays uninvestigated"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::test_alert;
    use crate::config::{InvestigationConfig, ModelSpec};
    use crate::incident::IncidentId;
    use crate::investigation::{
        Confidence, Evidence, Hypothesis, Outcome, Step, ToolFailure, ToolOutcome,
    };
    use crate::triage::{Severity, TriageResult};
    use rig_core::client::{CompletionClient as _, Nothing, ProviderClient as _};
    use rig_core::completion::{
        AssistantContent, CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
        Usage,
    };
    use rig_core::providers::{anthropic, ollama};
    use rig_core::streaming::StreamingCompletionResponse;
    use rig_core::test_utils::{MockCompletionModel, MockResponse, MockTurn};
    use std::num::{NonZeroU64, NonZeroUsize};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// rig's synthetic output tool. Pinned here because rig keeps the name
    /// private; a rename would otherwise silently turn every conclusion into an
    /// ordinary tool call.
    const OUTPUT_TOOL: &str = "final_result";

    const DB_RS: &str =
        "// Pool sizing is documented in config/pool.toml.\nconst MAX_POOL_SIZE: usize = 4;\n";
    const POOL_LINE: &str = "const MAX_POOL_SIZE: usize = 4;";

    /// A repo root holding one file with a findable bug.
    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("create tempdir");
            std::fs::write(root.path().join("db.rs"), DB_RS).expect("write fixture file");
            std::fs::write(root.path().join("pool.toml"), "min_connections = 32\n")
                .expect("write fixture file");
            Self { root }
        }

        fn config(&self, max_turns: usize, timeout_secs: u64) -> InvestigationConfig {
            InvestigationConfig {
                model: "ollama:test-model"
                    .parse::<ModelSpec>()
                    .expect("valid spec"),
                temperature: None,
                repo_root: self.root.path().to_path_buf(),
                max_turns: NonZeroUsize::new(max_turns).expect("nonzero"),
                timeout_secs: NonZeroU64::new(timeout_secs).expect("nonzero"),
                max_file_bytes: NonZeroUsize::new(4096).expect("nonzero"),
                queue_capacity: NonZeroUsize::new(8).expect("nonzero"),
            }
        }
    }

    #[test]
    fn agent_builder_accepts_erased_models_from_distinct_providers() {
        let fixture = Fixture::new();
        let ollama_config = fixture.config(8, 5);
        let ollama = ollama::Client::builder()
            .api_key(Nothing)
            .base_url("http://127.0.0.1:11434")
            .build()
            .expect("ollama client builds");
        let ollama_model = AnyCompletionModel::new(ollama.completion_model("test-model"));
        let (_agent, ollama_root) = build_agent(ollama_model, &ollama_config)
            .expect("ollama-shaped erased model builds an agent");
        let _still_available = ollama.completion_model("another-model");

        let anthropic_config = InvestigationConfig {
            model: "anthropic:claude-sonnet-5"
                .parse()
                .expect("valid anthropic model"),
            ..fixture.config(8, 5)
        };
        let anthropic =
            anthropic::Client::from_val("test-key".to_owned()).expect("anthropic client builds");
        let anthropic_model =
            AnyCompletionModel::new(anthropic.completion_model("claude-sonnet-5"));
        let (_agent, anthropic_root) = build_agent(anthropic_model, &anthropic_config)
            .expect("anthropic-shaped erased model builds an agent");
        let _still_available = anthropic.completion_model("another-model");

        assert_eq!(ollama_root, anthropic_root);
    }

    fn request() -> InvestigationRequest {
        InvestigationRequest {
            incident: IncidentId::new(),
            alert: test_alert(),
            triage: TriageResult {
                severity: Severity::P2,
                service: Some("checkout".to_owned()),
                tags: vec!["database".to_owned()],
                summary: "Checkout DB latency is elevated.".to_owned(),
            },
        }
    }

    fn hypothesis(quotes: &[&str]) -> Hypothesis {
        Hypothesis {
            summary: "The pool cap is below the documented minimum.".to_owned(),
            confidence: Confidence::High,
            evidence: quotes
                .iter()
                .map(|quote| Evidence {
                    tool: "read_file".to_owned(),
                    path: "db.rs".to_owned(),
                    quote: (*quote).to_owned(),
                })
                .collect(),
            affected_components: vec!["checkout".to_owned()],
        }
    }

    fn reads(path: &str) -> AssistantContent {
        AssistantContent::tool_call(
            format!("call-{path}"),
            "read_file",
            serde_json::json!({ "path": path }),
        )
    }

    fn concludes(hypothesis: &Hypothesis) -> AssistantContent {
        AssistantContent::tool_call(
            "call-final",
            OUTPUT_TOOL,
            serde_json::to_value(hypothesis).expect("hypothesis serializes"),
        )
    }

    fn turn(content: impl IntoIterator<Item = AssistantContent>) -> MockTurn {
        MockTurn::from_contents(content).expect("a turn has content")
    }

    fn tokens(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            ..Usage::new()
        }
    }

    /// The shape of the transcript: which turn each step belongs to, and what
    /// kind it is.
    fn shape(steps: &[Step]) -> Vec<(usize, &str)> {
        steps
            .iter()
            .map(|step| match step {
                Step::Reasoning { turn, .. } => (*turn, "reasoning"),
                Step::ToolCall { turn, name, .. } => (*turn, name.as_str()),
            })
            .collect()
    }

    fn tool_outcomes(steps: &[Step]) -> Vec<ToolOutcome> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::ToolCall { outcome, .. } => Some(*outcome),
                Step::Reasoning { .. } => None,
            })
            .collect()
    }

    async fn investigate_with<M>(model: M, config: InvestigationConfig) -> Investigation
    where
        M: CompletionModel + 'static,
    {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        let (done_tx, mut done_rx) = mpsc::channel(4);
        requests_tx.send(request()).await.expect("queue open");
        drop(requests_tx);
        let (agent, _repo_root) =
            build_agent(AnyCompletionModel::new(model), &config).expect("agent builds");
        let dropped = worker(
            agent,
            config,
            requests_rx,
            done_tx,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(dropped, 0, "a closed empty queue drops nothing");
        done_rx
            .recv()
            .await
            .expect("every request yields a result, outcome regardless")
    }

    async fn observed_temperature(temperature: Option<f64>) -> Option<f64> {
        let fixture = Fixture::new();
        let model = MockCompletionModel::new([turn([concludes(&hypothesis(&[]))])]);
        let observer = model.clone();
        let mut config = fixture.config(8, 5);
        config.temperature = temperature;

        let _investigated = investigate_with(model, config).await;

        observer
            .requests()
            .first()
            .and_then(|request| request.temperature)
    }

    #[tokio::test]
    async fn stage_temperature_is_optional_and_provider_neutral() {
        assert_eq!(observed_temperature(Some(0.35)).await, Some(0.35));
        assert_eq!(observed_temperature(None).await, None);
    }

    /// The operator is shown the root the tool resolved; a second
    /// canonicalization elsewhere could drift from it.
    #[test]
    fn build_agent_returns_the_root_the_tool_resolved() {
        let fixture = Fixture::new();
        let (_agent, repo_root) = build_agent(
            AnyCompletionModel::new(MockCompletionModel::default()),
            &fixture.config(8, 5),
        )
        .expect("agent builds");
        assert_eq!(
            repo_root,
            fixture
                .root
                .path()
                .canonicalize()
                .expect("root canonicalizes")
        );
    }

    #[tokio::test]
    async fn records_steps_in_order_with_turn_numbers() {
        let fixture = Fixture::new();
        let concluded = hypothesis(&[POOL_LINE]);
        let model = MockCompletionModel::new([
            turn([
                AssistantContent::text("reading the pool config"),
                reads("db.rs"),
            ])
            .with_usage(tokens(10, 5)),
            turn([
                AssistantContent::text("that cap is the cause"),
                concludes(&concluded),
            ])
            .with_usage(tokens(20, 7)),
        ]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        assert_eq!(investigated.transcript.outcome, Outcome::Completed);
        assert_eq!(investigated.hypothesis.as_ref(), Some(&concluded));
        assert_eq!(
            shape(&investigated.transcript.steps),
            [(1, "reasoning"), (1, "read_file"), (2, "reasoning")]
        );
        assert_eq!(investigated.transcript.requests, 2);
        assert_eq!(
            investigated.transcript.usage.total_tokens, 42,
            "per-turn usage must accumulate, not overwrite"
        );
        assert!(investigated.unverified_evidence.is_empty());
    }

    /// A live probe produced three tool calls in one turn, which is why `turn`
    /// is explicit rather than implied by position.
    #[tokio::test]
    async fn parallel_tool_calls_share_a_turn() {
        let fixture = Fixture::new();
        let concluded = hypothesis(&[]);
        let model = MockCompletionModel::new([
            turn([reads("db.rs"), reads("pool.toml")]),
            turn([concludes(&concluded)]),
        ]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        let turns: Vec<usize> = investigated
            .transcript
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::ToolCall { turn, .. } => Some(*turn),
                Step::Reasoning { .. } => None,
            })
            .collect();
        assert_eq!(
            turns,
            [1, 1],
            "both calls were issued in one turn: {:#?}",
            investigated.transcript.steps
        );
    }

    /// Exhaustion is an `Err` with no response to map, so only live capture
    /// can preserve what the run did before it ran out.
    #[tokio::test]
    async fn max_turns_exhaustion_keeps_the_transcript() {
        let fixture = Fixture::new();
        let model = MockCompletionModel::new([
            turn([AssistantContent::text("first look"), reads("db.rs")]),
            turn([AssistantContent::text("second look"), reads("pool.toml")]),
        ]);

        let investigated = investigate_with(model, fixture.config(2, 5)).await;

        assert_eq!(
            investigated.transcript.outcome,
            Outcome::MaxTurnsExhausted,
            "the budget counts the initial call, so two turns exhaust it"
        );
        assert!(investigated.hypothesis.is_none());
        assert!(
            !investigated.transcript.steps.is_empty(),
            "a run that never concluded must still yield its steps"
        );
        assert_eq!(
            shape(&investigated.transcript.steps),
            [
                (1, "reasoning"),
                (1, "read_file"),
                (2, "reasoning"),
                (2, "read_file")
            ],
            "every turn the budget bought must be in the transcript"
        );
    }

    #[tokio::test]
    async fn mid_run_failure_keeps_a_partial_transcript() {
        let fixture = Fixture::new();
        let model = MockCompletionModel::new([
            turn([
                AssistantContent::text("reading the pool config"),
                reads("db.rs"),
            ]),
            MockTurn::error("the provider fell over"),
        ]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        assert_eq!(investigated.transcript.outcome, Outcome::Failed);
        assert!(investigated.hypothesis.is_none());
        assert_eq!(
            shape(&investigated.transcript.steps),
            [(1, "reasoning"), (1, "read_file")],
            "the turn that ran before the failure must survive it"
        );
    }

    #[tokio::test]
    async fn tool_error_is_a_step_and_the_loop_continues() {
        let fixture = Fixture::new();
        let concluded = hypothesis(&[]);
        let model = MockCompletionModel::new([
            turn([reads("no/such/file.rs")]),
            turn([concludes(&concluded)]),
        ]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        assert_eq!(
            tool_outcomes(&investigated.transcript.steps),
            [ToolOutcome::Failed {
                kind: ToolFailure::NotFound
            }],
            "a failed read is a recorded step, not a dead run"
        );
        assert_eq!(
            investigated.transcript.outcome,
            Outcome::Completed,
            "the model gets to recover from a bad path"
        );
        assert_eq!(investigated.hypothesis.as_ref(), Some(&concluded));
    }

    /// rig fails the whole run on an invalid tool call when no hook has an
    /// opinion, so one fumbled name at turn 6 would discard everything the run
    /// already spent.
    #[tokio::test]
    async fn invalid_tool_call_is_retried_rather_than_killing_the_run() {
        let fixture = Fixture::new();
        let concluded = hypothesis(&[]);
        let model = MockCompletionModel::new([
            turn([AssistantContent::tool_call(
                "call-typo",
                "reed_file",
                serde_json::json!({ "path": "db.rs" }),
            )]),
            turn([concludes(&concluded)]),
        ]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        assert_eq!(
            investigated.transcript.outcome,
            Outcome::Completed,
            "the model must get corrective feedback, not a dead run"
        );
        assert_eq!(
            tool_outcomes(&investigated.transcript.steps),
            [ToolOutcome::Failed {
                kind: ToolFailure::InvalidArgs
            }],
            "the fumble is recorded rather than hidden"
        );
        assert_eq!(investigated.hypothesis.as_ref(), Some(&concluded));
    }

    #[tokio::test]
    async fn unverified_evidence_quote_is_flagged() {
        let fixture = Fixture::new();
        let invented = "const MAX_POOL_SIZE: usize = 64;";
        let concluded = hypothesis(&[POOL_LINE, invented]);
        let model =
            MockCompletionModel::new([turn([reads("db.rs")]), turn([concludes(&concluded)])]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        assert_eq!(investigated.transcript.outcome, Outcome::Completed);
        assert_eq!(
            investigated.hypothesis.as_ref(),
            Some(&concluded),
            "the hypothesis is flagged, not discarded or edited"
        );
        let quotes: Vec<&str> = investigated
            .unverified_evidence
            .iter()
            .map(|evidence| evidence.quote.as_str())
            .collect();
        assert_eq!(
            quotes,
            [invented],
            "only the quote no tool result contains is flagged"
        );
    }

    /// Tool output mode is best-effort: the model is asked, not forced, to
    /// call the output tool, so a clean run can still end in prose.
    #[tokio::test]
    async fn unparseable_output_is_a_failed_investigation() {
        let fixture = Fixture::new();
        let model = MockCompletionModel::new([
            turn([AssistantContent::text("I could not work out the cause.")]),
            turn([AssistantContent::text("Still no idea.")]),
        ]);

        let investigated = investigate_with(model, fixture.config(8, 5)).await;

        assert_eq!(
            investigated.transcript.outcome,
            Outcome::Failed,
            "a run that produced no hypothesis did not complete its job"
        );
        assert!(investigated.hypothesis.is_none());
    }

    /// The remaining exit path: the drain must survive a run abandoned by the
    /// wall clock too.
    #[tokio::test(start_paused = true)]
    async fn timeout_keeps_the_transcript() {
        let fixture = Fixture::new();
        let model = HangsAfter::new(
            MockCompletionModel::new([turn([
                AssistantContent::text("reading the pool config"),
                reads("db.rs"),
            ])]),
            1,
        );

        let investigated = investigate_with(model, fixture.config(8, 1)).await;

        assert_eq!(investigated.transcript.outcome, Outcome::TimedOut);
        assert!(investigated.hypothesis.is_none());
        assert_eq!(
            shape(&investigated.transcript.steps),
            [(1, "reasoning"), (1, "read_file")],
            "a run the clock killed must still yield the turns it bought"
        );
    }

    #[tokio::test]
    async fn shutdown_before_any_request_drains_the_queue() {
        let fixture = Fixture::new();
        let config = fixture.config(8, 5);
        let (requests_tx, requests_rx) = mpsc::channel(8);
        let (done_tx, mut done_rx) = mpsc::channel(8);
        for _ in 0..2 {
            requests_tx.send(request()).await.expect("queue open");
        }
        let (agent, _repo_root) = build_agent(
            AnyCompletionModel::new(MockCompletionModel::default()),
            &config,
        )
        .expect("agent builds");
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let dropped = worker(agent, config, requests_rx, done_tx, shutdown).await;

        assert_eq!(dropped, 2, "everything queued counts as dropped");
        assert!(
            done_rx.try_recv().is_err(),
            "nothing is investigated after shutdown"
        );
    }

    /// A scripted model that stops answering after `hang_after` calls, so the
    /// wall-clock timeout has something to fire against.
    #[derive(Clone)]
    struct HangsAfter {
        scripted: MockCompletionModel,
        calls: Arc<AtomicUsize>,
        hang_after: usize,
    }

    impl HangsAfter {
        fn new(scripted: MockCompletionModel, hang_after: usize) -> Self {
            Self {
                scripted,
                calls: Arc::new(AtomicUsize::new(0)),
                hang_after,
            }
        }
    }

    impl CompletionModel for HangsAfter {
        type Response = MockResponse;
        type StreamingResponse = MockResponse;
        type Client = ();

        fn make((): &Self::Client, _: impl Into<String>) -> Self {
            Self::new(MockCompletionModel::default(), 0)
        }

        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) >= self.hang_after {
                std::future::pending::<()>().await;
            }
            self.scripted.completion(request).await
        }

        async fn stream(
            &self,
            request: CompletionRequest,
        ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
            self.scripted.stream(request).await
        }
    }
}
