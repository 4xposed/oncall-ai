//! Grafana adapter parsing tests.

mod common;

use oncall_ai::alert::Alert;
use oncall_ai::grafana::Grafana;

fn parse_fixture(body: &[u8]) -> Vec<Alert> {
    common::parse_fixture(Grafana, body)
}

fn parse_err(body: &str) -> String {
    common::parse_err(Grafana, body)
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
fn raw_payload_is_the_alert_slice() {
    let body = include_bytes!("../fixtures/grafana/firing_batch.json");
    let alerts = parse_fixture(body);
    let full: serde_json::Value = serde_json::from_slice(body).expect("fixture is JSON");
    let raw_alerts = full
        .get("alerts")
        .and_then(serde_json::Value::as_array)
        .expect("alerts array");
    let parsed: Vec<_> = alerts.iter().map(|alert| &alert.raw_payload).collect();
    let expected: Vec<_> = raw_alerts.iter().collect();
    assert_eq!(parsed, expected);
}

#[test]
fn empty_batch_parses_to_no_alerts() {
    let alerts = parse_fixture(include_bytes!("../fixtures/grafana/empty_batch.json"));
    assert_eq!(alerts, vec![]);
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
