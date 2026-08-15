// Selective include: common/mod.rs's parser helpers would be dead code here.
mod common {
    pub mod harness;
}

use std::io::{Read, Write};
use std::net::TcpStream;

use common::harness::{
    assert_clean_exit, post, read_until_contains, read_until_ready, ready_addr, send_sigterm,
    sigterm_and_assert_clean_exit, spawn_agent,
};

#[test]
fn runs_until_sigterm_then_exits_cleanly() {
    let (child, mut stdout, watchdog) = spawn_agent(&[]);
    let lines = read_until_ready(&mut stdout);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("no webhook sources enabled")),
        "an all-defaults boot serves nothing and must warn about it, got:\n{}",
        lines.join("")
    );
    sigterm_and_assert_clean_exit(child, watchdog);
}

#[test]
fn oncall_env_vars_set_home_and_log_format() {
    let home = tempfile::tempdir().expect("create tempdir");
    let home_str = home.path().to_str().expect("utf-8 tempdir path");

    let (child, mut stdout, watchdog) =
        spawn_agent(&[("ONCALL_HOME", home_str), ("ONCALL_LOG__FORMAT", "json")]);
    let lines = read_until_ready(&mut stdout);

    let first = lines.first().expect("at least one log line");
    assert!(
        first.starts_with('{'),
        "expected JSON log output, got: {first}"
    );
    assert!(
        first.contains("\"source\":\"ONCALL_HOME\"") && first.contains(home_str),
        "expected home resolved from ONCALL_HOME to {home_str}, got: {first}"
    );

    sigterm_and_assert_clean_exit(child, watchdog);
}

#[test]
fn webhook_roundtrip_returns_202_and_logs_alerts() {
    let (child, mut stdout, watchdog) = spawn_agent(&[
        ("ONCALL_LOG__FORMAT", "json"),
        ("ONCALL_WEBHOOK__GRAFANA__ENABLED", "true"),
    ]);
    let addr = ready_addr(&read_until_ready(&mut stdout));

    let body = include_str!("../fixtures/grafana/firing_single.json");
    let response = post(addr, "/webhook/grafana", body);
    assert!(
        response.starts_with("HTTP/1.1 202"),
        "expected 202 Accepted, got: {response}"
    );

    let lines = read_until_contains(&mut stdout, "webhook received");
    let intake = lines.last().expect("intake line present");
    assert!(
        intake.contains(r#""source":"grafana""#) && intake.contains("HighErrorRate"),
        "intake line must carry source and raw body: {intake}"
    );

    let lines = read_until_contains(&mut stdout, "webhook parsed");
    let alert_line = lines
        .iter()
        .find(|line| line.contains(r#""source_alert_id":"c4f3a2b1d8e90f67""#));
    assert!(
        alert_line.is_some_and(|line| line.contains(r#""status":"firing""#)),
        "expected a per-alert line with fingerprint and status, got:\n{}",
        lines.join("")
    );

    sigterm_and_assert_clean_exit(child, watchdog);
}

#[test]
fn only_opted_in_sources_are_served() {
    let (child, mut stdout, watchdog) = spawn_agent(&[
        ("ONCALL_LOG__FORMAT", "json"),
        ("ONCALL_WEBHOOK__GRAFANA__ENABLED", "true"),
    ]);
    let addr = ready_addr(&read_until_ready(&mut stdout));

    let unconfigured = post(
        addr,
        "/webhook/pagerduty",
        include_str!("../fixtures/pagerduty/triggered.json"),
    );
    assert!(
        unconfigured.starts_with("HTTP/1.1 404"),
        "a source nobody enabled must 404, got: {unconfigured}"
    );

    let enabled = post(
        addr,
        "/webhook/grafana",
        include_str!("../fixtures/grafana/firing_single.json"),
    );
    assert!(
        enabled.starts_with("HTTP/1.1 202"),
        "an opted-in source must be served, got: {enabled}"
    );

    sigterm_and_assert_clean_exit(child, watchdog);
}

#[test]
fn triage_worker_readiness_line_appears_before_running_line() {
    let (child, mut stdout, watchdog) = spawn_agent(&[("ONCALL_LOG__FORMAT", "json")]);
    let lines = read_until_ready(&mut stdout);

    let started = lines
        .iter()
        .find(|line| line.contains(r#""message":"triage worker started""#));
    assert!(
        started.is_some(),
        "no triage worker started line before the running line, got:\n{}",
        lines.join("")
    );
    let started = started.expect("checked by assert above");
    assert!(
        started.contains(r#""model":""#) && started.contains(r#""endpoint":""#),
        "worker readiness line must carry model and endpoint: {started}"
    );

    send_sigterm(&child);
    let lines = read_until_contains(&mut stdout, "triage queue drained");
    let drained = lines.last().expect("drained line present");
    assert!(
        drained.contains(r#""dropped":0"#),
        "an idle queue must drain zero alerts: {drained}"
    );
    assert_clean_exit(child, watchdog);
}

#[test]
fn investigation_worker_readiness_line_appears_before_running_line() {
    let (child, mut stdout, watchdog) = spawn_agent(&[("ONCALL_LOG__FORMAT", "json")]);
    let lines = read_until_ready(&mut stdout);

    let started = lines
        .iter()
        .find(|line| line.contains(r#""message":"investigation worker started""#));
    assert!(
        started.is_some(),
        "no investigation worker started line before the running line, got:\n{}",
        lines.join("")
    );
    let started = started.expect("checked by assert above");
    assert!(
        started.contains(r#""model":""#) && started.contains(r#""max_turns":"#),
        "worker readiness line must carry model and max_turns: {started}"
    );
    // repo_root defaults to ".", which resolves against the working directory:
    // the line must carry what the tool resolved, not what was configured.
    assert!(
        started.contains(r#""repo_root":"/"#),
        "repo_root must be logged as the resolved absolute path: {started}"
    );

    sigterm_and_assert_clean_exit(child, watchdog);
}

/// The key is read at startup, so a missing one must kill the boot before
/// anything claims to be running.
#[test]
fn anthropic_investigation_without_a_key_fails_before_the_readiness_line() {
    let (mut child, mut stdout, watchdog) = spawn_agent(&[
        ("ONCALL_LOG__FORMAT", "json"),
        ("ONCALL_INVESTIGATION__MODEL", "anthropic:claude-sonnet-5"),
    ]);

    let mut logs = String::new();
    stdout
        .read_to_string(&mut logs)
        .expect("read stdout to EOF");
    assert!(
        !logs.contains("running; ctrl-C to stop"),
        "boot must fail before the readiness line, got:\n{logs}"
    );
    let status = child.wait().expect("wait for exit");
    assert!(
        !status.success(),
        "a missing ANTHROPIC_API_KEY must fail the boot, got: {status}"
    );
    watchdog.disarm();
}

#[test]
fn inflight_request_drains_through_shutdown() {
    let (child, mut stdout, watchdog) = spawn_agent(&[
        ("ONCALL_LOG__FORMAT", "json"),
        ("ONCALL_WEBHOOK__GRAFANA__ENABLED", "true"),
    ]);
    let addr = ready_addr(&read_until_ready(&mut stdout));

    let body = r#"{"alerts":[]}"#;
    let (head, tail) = body.split_at_checked(4).expect("split ascii body");
    let mut stream = TcpStream::connect(addr).expect("connect to server");
    stream
        .write_all(
            format!(
                "POST /webhook/grafana HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{head}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("write partial request");

    send_sigterm(&child);
    read_until_contains(&mut stdout, "draining");

    stream.write_all(tail.as_bytes()).expect("write body tail");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    assert!(
        response.starts_with("HTTP/1.1 202"),
        "in-flight request must complete during drain, got: {response}"
    );

    assert_clean_exit(child, watchdog);
}
