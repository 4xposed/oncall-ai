use oncall_ai::alert::{Alert, AlertSource};
use oncall_ai::config::TriageConfig;
use oncall_ai::grafana::Grafana;
use oncall_ai::pagerduty::Pagerduty;
use oncall_ai::triage::Triager;

fn default_triage_config() -> TriageConfig {
    let home = tempfile::tempdir().expect("create tempdir");
    let (config, _source) = oncall_ai::config::load(home.path()).expect("defaults load");
    config.triage
}

async fn eval_fixture(fixture: &str, alerts: &[Alert]) {
    assert!(!alerts.is_empty(), "fixture {fixture} must yield alerts");
    let triager = Triager::new(&default_triage_config()).expect("triager builds");
    for (index, alert) in alerts.iter().enumerate() {
        let result = triager.triage(alert).await.expect("triage succeeds");
        insta::assert_yaml_snapshot!(format!("{fixture}_{index}"), result);
    }
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn grafana_firing_single() {
    let alerts = Grafana
        .parse(include_bytes!("../fixtures/grafana/firing_single.json"))
        .expect("fixture parses");
    eval_fixture("grafana_firing_single", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn grafana_firing_batch() {
    let alerts = Grafana
        .parse(include_bytes!("../fixtures/grafana/firing_batch.json"))
        .expect("fixture parses");
    eval_fixture("grafana_firing_batch", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn grafana_resolved() {
    let alerts = Grafana
        .parse(include_bytes!("../fixtures/grafana/resolved.json"))
        .expect("fixture parses");
    eval_fixture("grafana_resolved", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn pagerduty_triggered() {
    let alerts = Pagerduty
        .parse(include_bytes!("../fixtures/pagerduty/triggered.json"))
        .expect("fixture parses");
    eval_fixture("pagerduty_triggered", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn pagerduty_acknowledged() {
    let alerts = Pagerduty
        .parse(include_bytes!("../fixtures/pagerduty/acknowledged.json"))
        .expect("fixture parses");
    eval_fixture("pagerduty_acknowledged", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn pagerduty_unacknowledged() {
    let alerts = Pagerduty
        .parse(include_bytes!("../fixtures/pagerduty/unacknowledged.json"))
        .expect("fixture parses");
    eval_fixture("pagerduty_unacknowledged", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn pagerduty_resolved() {
    let alerts = Pagerduty
        .parse(include_bytes!("../fixtures/pagerduty/resolved.json"))
        .expect("fixture parses");
    eval_fixture("pagerduty_resolved", &alerts).await;
}

#[tokio::test]
#[ignore = "requires local Ollama"]
async fn pagerduty_reopened() {
    let alerts = Pagerduty
        .parse(include_bytes!("../fixtures/pagerduty/reopened.json"))
        .expect("fixture parses");
    eval_fixture("pagerduty_reopened", &alerts).await;
}
