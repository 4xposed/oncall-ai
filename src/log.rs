use tracing_subscriber::EnvFilter;

use crate::config::{LogConfig, LogFormat};

/// An error from setting up logging.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("invalid log.level filter {level:?}: {source}")]
    Filter {
        level: String,
        source: tracing_subscriber::filter::ParseError,
    },
    #[error("cannot install tracing subscriber: {0}")]
    Init(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Sets up the global tracing subscriber.
///
/// # Errors
///
/// Fails when `cfg.level` is not a valid tracing filter or a global subscriber is already setup.
pub fn init(cfg: &LogConfig) -> Result<(), LoggingError> {
    let filter = EnvFilter::try_new(&cfg.level).map_err(|source| LoggingError::Filter {
        level: cfg.level.clone(),
        source,
    })?;
    match cfg.format {
        LogFormat::Pretty => tracing_subscriber::fmt().with_env_filter(filter).try_init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .try_init(),
    }?;
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
