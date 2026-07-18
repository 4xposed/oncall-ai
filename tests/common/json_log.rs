use std::io::{BufRead, BufReader};
use std::process::ChildStdout;

pub fn read_until_field(
    stdout: &mut BufReader<ChildStdout>,
    key: &str,
    value: &str,
) -> serde_json::Value {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read stdout");
        assert!(
            n > 0,
            "binary exited (stdout EOF) before logging fields.{key}={value:?}; got:\n{}",
            lines.join("")
        );
        let fields = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|mut json| json.get_mut("fields").map(serde_json::Value::take));
        lines.push(line);
        if let Some(fields) = fields
            && fields.get(key).and_then(serde_json::Value::as_str) == Some(value)
        {
            return fields;
        }
    }
}
