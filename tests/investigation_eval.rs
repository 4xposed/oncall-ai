use std::num::NonZeroU64;
use std::path::PathBuf;

use oncall_ai::alert::AlertSource as _;
use oncall_ai::config::InvestigationConfig;
use oncall_ai::grafana::Grafana;
use oncall_ai::incident::IncidentId;
use oncall_ai::investigation::{
    Investigation, InvestigationRequest, Outcome, Step, build_agent, worker,
};
use oncall_ai::model::AnyCompletionModel;
use oncall_ai::triage::{Severity, TriageResult};
use rig_agent::agent::Agent;
use rig_core::client::{CompletionClient as _, Nothing, ProviderClient as _};
use rig_core::completion::CompletionModel;
use rig_core::providers::{anthropic, ollama};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The file with the known bug.
const GUILTY_PATH: &str = "internal/db/db.go";

fn eval_config() -> InvestigationConfig {
    let home = tempfile::tempdir().expect("create tempdir");
    let (config, _source) = oncall_ai::config::load(home.path()).expect("defaults load");
    InvestigationConfig {
        repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/repo"),
        timeout_secs: NonZeroU64::new(900).expect("nonzero"),
        ..config.investigation
    }
}

fn request() -> InvestigationRequest {
    let alerts = Grafana
        .parse(include_bytes!("../fixtures/grafana/firing_single.json"))
        .expect("fixture parses");
    InvestigationRequest {
        incident: IncidentId::new(),
        alert: alerts.into_iter().next().expect("one alert"),
        triage: TriageResult {
            severity: Severity::P1,
            service: Some("checkout".to_owned()),
            tags: vec![
                "error-5xx".to_owned(),
                "error-rate".to_owned(),
                "production".to_owned(),
            ],
            summary: "Checkout service is experiencing a 5xx error rate above 2%, \
                      impacting user transactions."
                .to_owned(),
        },
    }
}

async fn investigate<M>(agent: Agent<M>, config: InvestigationConfig) -> Investigation
where
    M: CompletionModel + 'static,
{
    let (requests_tx, requests_rx) = mpsc::channel(1);
    let (done_tx, mut done_rx) = mpsc::channel(1);
    requests_tx.send(request()).await.expect("queue open");
    drop(requests_tx);
    let dropped = worker(
        agent,
        config,
        requests_rx,
        done_tx,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(dropped, 0, "the queued request must be investigated");
    done_rx.recv().await.expect("every request yields a result")
}

fn read_paths(investigated: &Investigation) -> Vec<&str> {
    investigated
        .transcript
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::ToolCall { name, args, .. } if name == "read_file" => {
                args.get("path").and_then(serde_json::Value::as_str)
            }
            _ => None,
        })
        .collect()
}

fn report(investigated: &Investigation) {
    let transcript = &investigated.transcript;
    eprintln!(
        "model={} outcome={:?} requests={} steps={} total_tokens={} duration_ms={}",
        transcript.model,
        transcript.outcome,
        transcript.requests,
        transcript.steps.len(),
        transcript.usage.total_tokens,
        transcript.duration_ms,
    );
    for step in &transcript.steps {
        match step {
            Step::Reasoning { turn, text } => eprintln!("  [{turn}] reasoning: {text}"),
            Step::ToolCall {
                turn,
                name,
                args,
                outcome,
                truncated,
                ..
            } => eprintln!("  [{turn}] {name}({args}) -> {outcome:?} truncated={truncated}"),
        }
    }
    match &investigated.hypothesis {
        Some(hypothesis) => eprintln!("hypothesis: {hypothesis:#?}"),
        None => eprintln!("hypothesis: none"),
    }
    eprintln!(
        "unverified evidence: {:#?}",
        investigated.unverified_evidence
    );
}

fn assert_found_the_pool_cap(investigated: &Investigation) {
    report(investigated);
    let transcript = &investigated.transcript;
    assert!(
        !transcript.steps.is_empty(),
        "a run must leave a transcript behind"
    );
    assert_eq!(
        transcript.outcome,
        Outcome::Completed,
        "the run did not complete"
    );
    let reads = read_paths(investigated);
    assert!(
        !reads.is_empty(),
        "the model must read the repository before concluding"
    );
    let hypothesis = investigated
        .hypothesis
        .as_ref()
        .expect("a completed run must yield a hypothesis");
    assert!(
        hypothesis
            .evidence
            .iter()
            .any(|evidence| evidence.path.contains("db.go")),
        "evidence must cite {GUILTY_PATH}, which sets the pool cap; \
         files read: {reads:?}; evidence: {:#?}",
        hypothesis.evidence
    );
    assert!(
        investigated.unverified_evidence.is_empty(),
        "every evidence quote must appear verbatim in a tool result; \
         unverified: {:#?}",
        investigated.unverified_evidence
    );
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn ollama_finds_the_pool_cap() {
    let config = eval_config();
    let client = ollama::Client::builder()
        .api_key(Nothing)
        .base_url("http://localhost:11434")
        .build()
        .expect("ollama client builds");
    let model = AnyCompletionModel::new(client.completion_model(config.model.model().as_str()));
    let (agent, repo_root) = build_agent(model, &config).expect("agent builds");
    eprintln!("repo_root: {}", repo_root.display());
    let investigated = investigate(agent, config).await;
    assert_found_the_pool_cap(&investigated);
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and spends real money"]
async fn anthropic_finds_the_pool_cap() {
    let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!("skipped: ANTHROPIC_API_KEY is not set");
        return;
    };
    let config = InvestigationConfig {
        model: "anthropic:claude-sonnet-5"
            .parse()
            .expect("valid model spec"),
        ..eval_config()
    };
    let client = anthropic::Client::from_val(api_key).expect("anthropic client builds");
    let model = AnyCompletionModel::new(client.completion_model(config.model.model().as_str()));
    let (agent, repo_root) = build_agent(model, &config).expect("agent builds");
    eprintln!("repo_root: {}", repo_root.display());
    let investigated = investigate(agent, config).await;
    assert_found_the_pool_cap(&investigated);
}
