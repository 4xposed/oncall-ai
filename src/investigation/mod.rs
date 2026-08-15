pub mod read_file;
mod recorder;
mod worker;

use crate::incident::IncidentId;
use std::fmt::Write as _;
pub use worker::{BuildError, build_agent, worker};

pub const INVESTIGATION_PREAMBLE: &str = include_str!("preamble.txt");

#[derive(Debug)]
pub struct InvestigationRequest {
    pub incident: IncidentId,
    pub alert: crate::alert::Alert,
    pub triage: crate::triage::TriageResult,
}

#[must_use]
pub fn render_prompt(request: &InvestigationRequest, repository: Option<&str>) -> String {
    let mut prompt = String::new();
    if let Some(repository) = repository {
        prompt.push_str("repository: ");
        prompt.push_str(repository);
        prompt.push('\n');
    }
    prompt.push_str(&crate::triage::render_prompt(&request.alert));
    let triage = &request.triage;
    writeln!(
        prompt,
        "triage:\n  severity: {}\n  service: {}",
        triage.severity,
        triage.service.as_deref().unwrap_or("unidentified")
    )
    .expect("write to String is infallible");
    if !triage.tags.is_empty() {
        writeln!(prompt, "  tags: {}", triage.tags.join(", "))
            .expect("write to String is infallible");
    }
    writeln!(prompt, "  summary: {}", triage.summary).expect("write to String is infallible");
    prompt
}

#[derive(Debug, PartialEq)]
pub struct Investigation {
    pub transcript: Transcript,
    pub hypothesis: Option<Hypothesis>,
    pub unverified_evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Transcript {
    pub incident: IncidentId,
    pub model: String,
    pub outcome: Outcome,
    pub usage: TokenUsage,
    pub requests: usize,
    pub duration_ms: u64,
    pub steps: Vec<Step>,
}

impl Transcript {
    pub fn tool_results(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|step| match step {
            Step::ToolCall { result, .. } => Some(result.as_str()),
            Step::Reasoning { .. } => None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Outcome {
    Completed,
    MaxTurnsExhausted,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    Reasoning {
        turn: usize,
        text: String,
    },
    ToolCall {
        turn: usize,
        name: String,
        args: serde_json::Value,
        result: String,
        outcome: ToolOutcome,
        truncated: bool,
    },
}

/// Classified, because `result` cannot distinguish a tool that timed out from
/// a file containing the words "the tool timed out".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ToolOutcome {
    Succeeded,
    Failed { kind: ToolFailure },
}

/// Mirrors rig's `ToolErrorKind` so its churn stays out of the contract; that
/// type is `#[non_exhaustive]`, so kinds it adds later map onto `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ToolFailure {
    InvalidArgs,
    Timeout,
    Cancelled,
    NotFound,
    PermissionDenied,
    RateLimited,
    Provider,
    Network,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Truncated(pub bool);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(
    description = "What the investigation concluded, and the evidence it rests on. Cite only files you actually read."
)]
pub struct Hypothesis {
    pub summary: String,
    pub confidence: Confidence,
    pub evidence: Vec<Evidence>,
    pub affected_components: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(description = "How well the evidence supports the summary.")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(
    description = "One citation: the tool that produced it, the path it came from, and a line quoted verbatim from that output."
)]
pub struct Evidence {
    pub tool: String,
    pub path: String,
    pub quote: String,
}

#[cfg(test)]
pub(crate) fn test_investigation(incident: IncidentId) -> Investigation {
    Investigation {
        transcript: Transcript {
            incident,
            model: "ollama:test-model".to_owned(),
            outcome: Outcome::Completed,
            usage: TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 5,
                total_tokens: 15,
            },
            requests: 2,
            duration_ms: 1,
            steps: Vec::new(),
        },
        hypothesis: Some(Hypothesis {
            summary: "stub".to_owned(),
            confidence: Confidence::Low,
            evidence: Vec::new(),
            affected_components: Vec::new(),
        }),
        unverified_evidence: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Transcript {
        Transcript {
            incident: IncidentId::new(),
            model: "ollama:qwen3:8b".to_owned(),
            outcome: Outcome::Completed,
            usage: TokenUsage {
                input_tokens: 1_024,
                cached_input_tokens: 512,
                cache_creation_input_tokens: 128,
                output_tokens: 256,
                total_tokens: 1_920,
            },
            requests: 2,
            duration_ms: 4_200,
            steps: vec![
                Step::Reasoning {
                    turn: 1,
                    text: "The alert names checkout-api; read its timeout config.".to_owned(),
                },
                Step::ToolCall {
                    turn: 1,
                    name: "read_file".to_owned(),
                    args: serde_json::json!({ "path": "services/checkout/config.yaml" }),
                    result: "no such file under repo_root; use a path relative to the repo root"
                        .to_owned(),
                    outcome: ToolOutcome::Failed {
                        kind: ToolFailure::NotFound,
                    },
                    truncated: false,
                },
                // Same turn as the call above: parallel tool calls are why
                // `turn` is explicit rather than implied by position.
                Step::ToolCall {
                    turn: 1,
                    name: "read_file".to_owned(),
                    args: serde_json::json!({ "path": "services/checkout/pool.toml" }),
                    result: "max_connections = 4\n".to_owned(),
                    outcome: ToolOutcome::Succeeded,
                    truncated: true,
                },
            ],
        }
    }

    #[test]
    fn transcript_serialization_is_stable() {
        insta::assert_yaml_snapshot!(sample(), {
            ".incident" => insta::dynamic_redaction(|value, _path| {
                assert!(value.as_str().is_some(), "IncidentId must serialize as a string");
                "[incident-id]"
            }),
        });
    }

    /// The prompt carries the parsed alert and what triage made of it. The raw
    /// payload is deliberately not in it: it is the source's wire format, not
    /// anything the investigation reasons about.
    #[test]
    fn rendered_prompt_carries_alert_and_triage_but_no_raw_payload() {
        let mut alert = crate::alert::test_alert();
        alert.raw_payload = serde_json::json!({ "secret": "raw-payload-marker" });
        let request = InvestigationRequest {
            incident: IncidentId::new(),
            alert,
            triage: crate::triage::TriageResult {
                severity: crate::triage::Severity::P2,
                service: Some("checkout".to_owned()),
                tags: vec!["database".to_owned()],
                summary: "Checkout DB latency is elevated.".to_owned(),
            },
        };

        let prompt = render_prompt(&request, Some("checkout-api"));

        assert!(
            !prompt.contains("raw-payload-marker"),
            "raw payloads must not reach the model: {prompt}"
        );
        for expected in [
            "repository: checkout-api",
            "alertname: CheckoutDbLatency",
            "  severity: P2",
            "  service: checkout",
            "  tags: database",
            "  summary: Checkout DB latency is elevated.",
        ] {
            assert!(prompt.contains(expected), "missing {expected:?}: {prompt}");
        }
    }

    #[test]
    fn tool_results_are_the_tool_call_outputs_in_order() {
        let transcript = sample();
        let results: Vec<&str> = transcript.tool_results().collect();
        assert_eq!(
            results,
            [
                "no such file under repo_root; use a path relative to the repo root",
                "max_connections = 4\n",
            ]
        );
    }
}
