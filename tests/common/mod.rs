use oncall_ai::alert::{Alert, AlertSource};

pub fn parse_fixture(source: impl AlertSource, body: &[u8]) -> Vec<Alert> {
    source.parse(body).expect("fixture parses")
}

pub fn parse_err(source: impl AlertSource, body: &str) -> String {
    source
        .parse(body.as_bytes())
        .expect_err("must not parse")
        .to_string()
}
