use tracing_subscriber::EnvFilter;

use crate::config::{LogConfig, LogFormat};

#[derive(Debug, thiserror::Error)]
#[error("invalid log.level filter {level:?}: {source}")]
pub struct LoggingError {
    level: String,
    source: tracing_subscriber::filter::ParseError,
}

pub fn init(cfg: &LogConfig) -> Result<(), LoggingError> {
    let filter = EnvFilter::try_new(&cfg.level).map_err(|source| LoggingError {
        level: cfg.level.clone(),
        source,
    })?;
    match cfg.format {
        LogFormat::Pretty => tracing_subscriber::fmt().with_env_filter(filter).init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LogConfig, LogFormat};

    #[test]
    fn bad_level_filter_fails_loud() {
        let cfg = LogConfig {
            format: LogFormat::Pretty,
            level: "no/such==filter".into(),
        };
        let err = init(&cfg).expect_err("invalid filter must be rejected");
        assert!(err.to_string().contains("no/such==filter"), "{err}");
    }
}
