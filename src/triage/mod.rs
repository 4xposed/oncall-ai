use std::collections::BTreeMap;

mod triager;
mod worker;

pub use triager::{BuildError, ErrorClass, TriageError, Triager};
pub use worker::{Triage, worker};

pub const TRIAGE_PREAMBLE: &str = r#"You triage alerts for an on-call engineer.

Given one alert, produce:
- severity: P1 (page immediately, user-facing outage) … P4 (informational).
  Use "Unknown" when the alert genuinely does not tell you. The alert's own
  severity hints (labels, priority fields) are advisory context from the
  source — weigh them against the content; do not merely echo them.
- service: the affected service's name, exactly as the alert names it.
  Use null when no service is identifiable. Never guess.
- tags: short lowercase keywords an engineer would filter by (subsystem,
  environment, failure kind). Empty list is fine.
- summary: one plain sentence stating what is wrong.
"#;

#[must_use]
pub fn render_prompt(alert: &crate::alert::Alert) -> String {
    let mut prompt = format!(
        "source: {}\nstatus: {}\nstarted: {}\n",
        alert.source, alert.status, alert.starts_at
    );
    push_section(&mut prompt, "labels", &alert.labels);
    push_section(&mut prompt, "annotations", &alert.annotations);
    prompt
}

fn push_section(prompt: &mut String, name: &str, entries: &BTreeMap<String, String>) {
    if entries.is_empty() {
        return;
    }
    prompt.push_str(name);
    prompt.push_str(":\n");
    for (key, value) in entries {
        prompt.push_str("  ");
        prompt.push_str(key);
        prompt.push_str(": ");
        prompt.push_str(value);
        prompt.push('\n');
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(description = "Incident priority. Use Unknown when the alert does not say.")]
pub enum Severity {
    P1,
    P2,
    P3,
    P4,
    Unknown,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(
    description = "Triage assessment of a single alert. service is null only when no service is identifiable — never guess."
)]
pub struct TriageResult {
    pub severity: Severity,
    pub service: Option<String>,
    pub tags: Vec<String>,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::AlertSource;

    #[test]
    fn rendered_prompt_is_stable() {
        let alerts = crate::grafana::Grafana
            .parse(include_bytes!("../../fixtures/grafana/firing_single.json"))
            .expect("fixture parses");
        let alert = alerts.first().expect("one alert");
        insta::assert_snapshot!(render_prompt(alert));
    }

    #[test]
    fn severity_display_and_serde_are_stable() {
        let all = [
            Severity::P1,
            Severity::P2,
            Severity::P3,
            Severity::P4,
            Severity::Unknown,
        ];
        let rendered: Vec<(String, String)> = all
            .iter()
            .map(|severity| {
                (
                    severity.to_string(),
                    serde_json::to_string(severity).expect("severity serializes"),
                )
            })
            .collect();
        insta::assert_yaml_snapshot!(rendered);
    }

    #[test]
    fn triage_result_schema_is_stable() {
        let schema = schemars::schema_for!(TriageResult);
        insta::assert_yaml_snapshot!(schema);
    }
}
