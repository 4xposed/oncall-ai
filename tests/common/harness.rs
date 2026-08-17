use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Watchdog {
    disarm: std::sync::mpsc::Sender<()>,
    _home: tempfile::TempDir,
}

impl Watchdog {
    pub fn disarm(self) {
        if self.disarm.send(()).is_err() {
            eprintln!("watchdog: already fired");
        }
    }
}

pub fn spawn_agent_with_config(
    config: &str,
    envs: &[(&str, &str)],
) -> (Child, BufReader<ChildStdout>, Watchdog) {
    let home = tempfile::tempdir().expect("create process config home");
    std::fs::write(
        home.path().join("config.toml"),
        format!("[webhook]\nbind = \"127.0.0.1:0\"\n{config}"),
    )
    .expect("write process config");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oncall-ai"));
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.env("ONCALL_HOME", home.path());
    cmd.env_remove("ANTHROPIC_API_KEY");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn binary");

    let (disarm_tx, disarm_rx) = std::sync::mpsc::channel();
    let pid = child.id().to_string();
    std::thread::spawn(move || {
        if disarm_rx.recv_timeout(Duration::from_secs(10)).is_ok() {
            return;
        }
        if Command::new("kill").args(["-KILL", &pid]).status().is_err() {
            eprintln!("watchdog: failed to send SIGKILL to {pid}");
        }
    });

    let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    (
        child,
        stdout,
        Watchdog {
            disarm: disarm_tx,
            _home: home,
        },
    )
}

pub fn read_until_contains(stdout: &mut BufReader<ChildStdout>, needle: &str) -> Vec<String> {
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

pub fn read_until_ready(stdout: &mut BufReader<ChildStdout>) -> Vec<String> {
    read_until_contains(stdout, "running; ctrl-C to stop")
}

pub fn ready_addr(lines: &[String]) -> SocketAddr {
    let line = lines.last().expect("readiness line present");
    let json: serde_json::Value = serde_json::from_str(line).expect("readiness line is JSON");
    let field = json
        .pointer("/fields/addr")
        .and_then(serde_json::Value::as_str);
    assert!(field.is_some(), "no addr field on readiness line: {line}");
    field
        .expect("checked by assert above")
        .parse()
        .expect("addr field parses as socket address")
}

pub fn post(addr: SocketAddr, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to server");
    stream
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

pub fn send_sigterm(child: &Child) {
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
}

pub fn assert_clean_exit(mut child: Child, watchdog: Watchdog) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "expected exit 0, got {status}");
            watchdog.disarm();
            return;
        }
        assert!(Instant::now() < deadline, "no exit within 5s of SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn sigterm_and_assert_clean_exit(child: Child, watchdog: Watchdog) {
    send_sigterm(&child);
    assert_clean_exit(child, watchdog);
}
