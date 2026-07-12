//! Spawns the real binary and checks boot, config handling, the webhook
//! server and clean exit on signals.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// Spawns the binary with the given env vars and a watchdog that kills it
/// after ten seconds so a hang fails the test.
///
/// Binds port 0 by default since parallel tests would race for a fixed
/// port. A caller-provided `ONCALL_WEBHOOK__BIND` wins.
fn spawn_agent(envs: &[(&str, &str)]) -> (Child, BufReader<ChildStdout>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oncall-ai"));
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.env("ONCALL_WEBHOOK__BIND", "127.0.0.1:0");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn binary");

    let pid = child.id().to_string();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        if Command::new("kill").args(["-KILL", &pid]).status().is_err() {
            eprintln!("watchdog: failed to send SIGKILL to {pid}");
        }
    });

    let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    (child, stdout)
}

/// Reads stdout until a line contains `needle`, returning everything read.
///
/// # Panics
///
/// Panics on EOF so a crashed binary fails the test with context.
fn read_until_contains(stdout: &mut BufReader<ChildStdout>, needle: &str) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read stdout");
        assert!(
            n > 0,
            "binary exited (stdout EOF) before logging {needle:?}; got:\n{}",
            lines.join("")
        );
        let found = line.contains(needle);
        lines.push(line);
        if found {
            return lines;
        }
    }
}

/// Reads stdout until the readiness line.
///
/// The binary binds and sets up signal handlers before logging readiness,
/// so afterwards the server accepts connections and catches SIGTERM.
fn read_until_ready(stdout: &mut BufReader<ChildStdout>) -> Vec<String> {
    read_until_contains(stdout, "running; ctrl-C to stop")
}

/// Extracts the bound address from the readiness line.
///
/// Needs JSON logs. The pretty formatter wraps field names in ANSI escapes.
fn ready_addr(lines: &[String]) -> SocketAddr {
    let line = lines.last().expect("readiness line present");
    let field = line
        .split(r#""addr":""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next());
    assert!(field.is_some(), "no addr field on readiness line: {line}");
    field
        .expect("checked by assert above")
        .parse()
        .expect("addr field parses as socket address")
}

fn send_sigterm(child: &Child) {
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
}

fn assert_clean_exit(mut child: Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "expected exit 0, got {status}");
            return;
        }
        assert!(Instant::now() < deadline, "no exit within 5s of SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sigterm_and_assert_clean_exit(child: Child) {
    send_sigterm(&child);
    assert_clean_exit(child);
}

#[test]
fn runs_until_sigterm_then_exits_cleanly() {
    let (child, mut stdout) = spawn_agent(&[]);
    read_until_ready(&mut stdout);
    sigterm_and_assert_clean_exit(child);
}

/// `ONCALL_HOME` relocates home and `ONCALL_LOG__FORMAT` reaches
/// `log.format`, covering prefix stripping and `__` splitting.
#[test]
fn oncall_env_vars_set_home_and_log_format() {
    let home = tempfile::tempdir().expect("create tempdir");
    let home_str = home.path().to_str().expect("utf-8 tempdir path");

    let (child, mut stdout) =
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

    sigterm_and_assert_clean_exit(child);
}

/// A POSTed Grafana fixture gets 202, shows up in the intake log and yields
/// one log line per alert.
#[test]
fn webhook_roundtrip_returns_202_and_logs_alerts() {
    let (child, mut stdout) = spawn_agent(&[("ONCALL_LOG__FORMAT", "json")]);
    let addr = ready_addr(&read_until_ready(&mut stdout));

    let body = include_str!("../fixtures/grafana/firing_single.json");
    let mut stream = TcpStream::connect(addr).expect("connect to server");
    stream
        .write_all(
            format!(
                "POST /webhook/grafana HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
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

    sigterm_and_assert_clean_exit(child);
}

/// A request in flight when SIGTERM lands still completes with 202. Sends
/// half the body, signals, syncs on the draining line, then sends the rest.
#[test]
fn inflight_request_drains_through_shutdown() {
    let (child, mut stdout) = spawn_agent(&[("ONCALL_LOG__FORMAT", "json")]);
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

    assert_clean_exit(child);
}
