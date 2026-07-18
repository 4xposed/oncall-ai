mod common;

use oncall_ai::alert::Alert;
use oncall_ai::pagerduty::Pagerduty;

fn parse_fixture(body: &[u8]) -> Vec<Alert> {
    common::parse_fixture(Pagerduty, body)
}

fn parse_err(body: &str) -> String {
    common::parse_err(Pagerduty, body)
}

#[test]
fn triggered_normalizes_to_firing() {
    let alerts = parse_fixture(include_bytes!("../fixtures/pagerduty/triggered.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn raw_payload_is_the_full_event() {
    let body = include_bytes!("../fixtures/pagerduty/triggered.json");
    let alerts = parse_fixture(body);
    let alert = alerts.first().expect("one alert");
    let full: serde_json::Value = serde_json::from_slice(body).expect("fixture is JSON");
    assert_eq!(Some(&alert.raw_payload), full.get("event"));
}

#[test]
fn acknowledged_normalizes_to_acknowledged() {
    let alerts = parse_fixture(include_bytes!("../fixtures/pagerduty/acknowledged.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn unacknowledged_maps_back_to_firing() {
    let alerts = parse_fixture(include_bytes!("../fixtures/pagerduty/unacknowledged.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn resolved_normalizes_to_resolved() {
    let alerts = parse_fixture(include_bytes!("../fixtures/pagerduty/resolved.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn reopened_maps_to_firing() {
    let alerts = parse_fixture(include_bytes!("../fixtures/pagerduty/reopened.json"));
    insta::assert_yaml_snapshot!(alerts, { "[].raw_payload" => "[raw]" });
}

#[test]
fn priority_updated_is_skipped() {
    let alerts = parse_fixture(include_bytes!(
        "../fixtures/pagerduty/priority_updated.json"
    ));
    assert_eq!(alerts, vec![]);
}

#[test]
fn non_incident_event_is_skipped_without_reading_data() {
    let alerts = parse_fixture(
        br#"{"event": {"event_type": "service.updated", "data": {"unrelated": "shape"}}}"#,
    );
    assert_eq!(alerts, vec![]);
}

#[test]
fn missing_event_envelope_is_an_error_naming_the_field() {
    let error = parse_err(r#"{"data": {}}"#);
    assert!(
        error.contains("event"),
        "error must name the missing field: {error}"
    );
}

#[test]
fn mapped_event_with_missing_created_at_is_an_error_naming_the_field() {
    let error = parse_err(
        r#"{"event": {"event_type": "incident.triggered", "data": {
            "id": "PGR0VU2", "title": "t",
            "service": {"summary": "checkout"}, "urgency": "high",
            "priority": null}}}"#,
    );
    assert!(
        error.contains("created_at"),
        "error must name the missing field: {error}"
    );
}

#[test]
fn non_json_body_is_an_error() {
    assert!(!parse_err("not json at all").is_empty());
}
