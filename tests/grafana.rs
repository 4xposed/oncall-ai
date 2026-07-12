//! Grafana adapter parsing tests.

use oncall_ai::alert::{Alert, AlertSource};
use oncall_ai::grafana::Grafana;

fn parse_fixture(body: &[u8]) -> Vec<Alert> {
    Grafana.parse(body).expect("fixture parses")
}

#[test]
fn firing_single_normalizes() {
    let alerts = parse_fixture(include_bytes!("../fixtures/grafana/firing_single.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn firing_batch_yields_one_alert_per_entry() {
    let alerts = parse_fixture(include_bytes!("../fixtures/grafana/firing_batch.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn resolved_normalizes_with_resolved_status() {
    let alerts = parse_fixture(include_bytes!("../fixtures/grafana/resolved.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn empty_batch_parses_to_no_alerts() {
    let alerts = parse_fixture(include_bytes!("../fixtures/grafana/empty_batch.json"));
    assert_eq!(alerts, vec![]);
}

fn parse_err(body: &str) -> String {
    Grafana
        .parse(body.as_bytes())
        .expect_err("must not parse")
        .to_string()
}

#[test]
fn missing_fingerprint_is_an_error_naming_the_field() {
    let error = parse_err(
        r#"{"alerts": [{"status": "firing", "labels": {}, "annotations": {},
            "startsAt": "2026-07-12T09:03:00Z"}]}"#,
    );
    assert!(
        error.contains("fingerprint"),
        "error must name the missing field: {error}"
    );
}

#[test]
fn unknown_status_is_an_error() {
    let error = parse_err(
        r#"{"alerts": [{"status": "acknowledged", "labels": {}, "annotations": {},
            "startsAt": "2026-07-12T09:03:00Z", "fingerprint": "abc"}]}"#,
    );
    assert!(
        error.contains("acknowledged"),
        "error must show the unexpected status: {error}"
    );
}

#[test]
fn non_json_body_is_an_error() {
    assert!(!parse_err("not json at all").is_empty());
}
