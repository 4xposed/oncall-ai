mod common {
    pub mod harness;
    pub mod json_log;
}

use common::harness::{
    post, read_until_ready, ready_addr, sigterm_and_assert_clean_exit, spawn_agent,
};
use common::json_log::read_until_field;
use std::io::BufReader;
use std::net::SocketAddr;
use std::process::ChildStdout;

const ENVS: &[(&str, &str)] = &[
    ("ONCALL_LOG__FORMAT", "json"),
    ("ONCALL_WEBHOOK__GRAFANA__ENABLED", "true"),
    ("ONCALL_WEBHOOK__PAGERDUTY__ENABLED", "true"),
    ("ONCALL_TRIAGE__ENDPOINT", "http://127.0.0.1:9"),
];

fn post_accepted(addr: SocketAddr, path: &str, body: &str) {
    let response = post(addr, path, body);
    assert!(
        response.starts_with("HTTP/1.1 202"),
        "expected 202 Accepted for {path}, got: {response}"
    );
}

fn decision(stdout: &mut BufReader<ChildStdout>, decision: &str) -> serde_json::Value {
    read_until_field(stdout, "decision", decision)
}

fn str_field<'a>(fields: &'a serde_json::Value, key: &str) -> &'a str {
    let value = fields.get(key).and_then(serde_json::Value::as_str);
    assert!(value.is_some(), "no string field {key} in: {fields}");
    value.expect("checked by assert above")
}

#[test]
fn grafana_lifecycle_dedupes_and_closes() {
    let (child, mut stdout, watchdog) = spawn_agent(ENVS);
    let addr = ready_addr(&read_until_ready(&mut stdout));
    let firing = include_str!("../fixtures/grafana/firing_single.json");

    post_accepted(addr, "/webhook/grafana", firing);
    let opened = decision(&mut stdout, "opened");
    let incident_id = str_field(&opened, "incident_id").to_owned();

    post_accepted(addr, "/webhook/grafana", firing);
    let updated = decision(&mut stdout, "updated");
    assert_eq!(
        str_field(&updated, "incident_id"),
        incident_id,
        "a re-fire must land on the incident it opened"
    );
    assert_eq!(str_field(&updated, "rule"), "exact");

    post_accepted(
        addr,
        "/webhook/grafana",
        include_str!("../fixtures/grafana/resolved.json"),
    );
    let closed = decision(&mut stdout, "closed");
    assert_eq!(
        str_field(&closed, "incident_id"),
        incident_id,
        "the resolve must close the incident the fire opened"
    );
    assert_eq!(str_field(&closed, "reason"), "resolved");

    sigterm_and_assert_clean_exit(child, watchdog);
}

#[test]
fn pagerduty_siblings_share_an_incident() {
    let (child, mut stdout, watchdog) = spawn_agent(ENVS);
    let addr = ready_addr(&read_until_ready(&mut stdout));

    post_accepted(
        addr,
        "/webhook/pagerduty",
        include_str!("../fixtures/pagerduty/triggered.json"),
    );
    let opened = decision(&mut stdout, "opened");
    let incident_id = str_field(&opened, "incident_id").to_owned();

    post_accepted(
        addr,
        "/webhook/pagerduty",
        include_str!("../fixtures/pagerduty/triggered_sibling.json"),
    );
    let attached = decision(&mut stdout, "attached");
    assert_eq!(
        str_field(&attached, "incident_id"),
        incident_id,
        "same labels, different PD id must attach, not open"
    );
    assert_eq!(str_field(&attached, "rule"), "fingerprint");

    sigterm_and_assert_clean_exit(child, watchdog);
}
