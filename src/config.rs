use std::fmt;
use std::path::{Path, PathBuf};

use config::{Environment, File, FileFormat, Source};
use serde::Deserialize;

/// Load defaults.
pub const DEFAULT_CONFIG: &str = include_str!("../seed/default_config.toml");

/// Where the home directory came from.
#[derive(Debug, PartialEq, Eq)]
pub enum HomeSource {
    /// The `ONCALL_HOME` environment variable.
    EnvVar,
    /// The running executable's directory.
    ExeDir,
}

impl fmt::Display for HomeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HomeSource::EnvVar => write!(f, "ONCALL_HOME"),
            HomeSource::ExeDir => write!(f, "exe dir"),
        }
    }
}

/// An error from resolving the home directory.
#[derive(Debug, thiserror::Error)]
pub enum HomeError {
    #[error("cannot determine executable path: {0}")]
    Exe(#[from] std::io::Error),
    #[error("executable path has no parent directory")]
    NoParent,
}

/// Resolves the agent's home directory.
///
/// `env_override` wins, otherwise the executable's directory is used.
///
/// # Errors
///
/// Fails when the executable path is unknown or has no parent.
pub fn resolve_home(env_override: Option<PathBuf>) -> Result<(PathBuf, HomeSource), HomeError> {
    if let Some(home) = env_override {
        return Ok((home, HomeSource::EnvVar));
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().ok_or(HomeError::NoParent)?;
    Ok((dir.to_path_buf(), HomeSource::ExeDir))
}

/// The full agent configuration.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct Config {
    pub log: LogConfig,
    pub webhook: WebhookConfig,
}

/// Webhook server settings.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct WebhookConfig {
    pub bind: std::net::SocketAddr,
    /// Largest accepted request body in bytes.
    pub body_limit_bytes: usize,
    /// Longest slice of a body echoed into the intake log line, in bytes.
    pub body_log_limit_bytes: usize,
}

/// Log output settings.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct LogConfig {
    pub format: LogFormat,
    /// A tracing filter directive such as `info`.
    pub level: String,
}

/// The log line format.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
}

/// Where the loaded configuration came from.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigSource {
    /// No `config.toml` found, embedded defaults used.
    Embedded,
    /// A `config.toml` at this path.
    File(PathBuf),
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::Embedded => write!(f, "embedded defaults"),
            ConfigSource::File(path) => write!(f, "file ({})", path.display()),
        }
    }
}

/// An error from loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The `config.toml` failed to parse or merge.
    #[error("malformed config {path}: {source}")]
    File {
        path: PathBuf,
        source: Box<config::ConfigError>,
    },
    /// The merged configuration is not a valid [`Config`].
    #[error("invalid config: {0}")]
    Invalid(Box<config::ConfigError>),
}

/// Loads configuration for the given home directory.
///
/// Layers embedded defaults, then `config.toml` under `home` if present,
/// then `ONCALL_*` environment variables. Later layers win.
///
/// # Errors
///
/// Fails when a present `config.toml` is malformed or the merged result is
/// not a valid [`Config`].
pub fn load(home: &Path) -> Result<(Config, ConfigSource), ConfigError> {
    load_with_env(
        home,
        Environment::with_prefix("ONCALL")
            .prefix_separator("_")
            .separator("__"),
    )
}

fn load_with_env(
    home: &Path,
    env: impl Source + Send + Sync + 'static,
) -> Result<(Config, ConfigSource), ConfigError> {
    let path = home.join("config.toml");
    let mut builder =
        config::Config::builder().add_source(File::from_str(DEFAULT_CONFIG, FileFormat::Toml));

    let source = if path.is_file() {
        builder = builder.add_source(File::from(path.clone()).format(FileFormat::Toml));
        ConfigSource::File(path)
    } else {
        ConfigSource::Embedded
    };

    let config = builder
        .add_source(env)
        .build()
        .and_then(|merged| merged.try_deserialize())
        .map_err(|e| match &source {
            ConfigSource::File(path) => ConfigError::File {
                path: path.clone(),
                source: Box::new(e),
            },
            ConfigSource::Embedded => ConfigError::Invalid(Box::new(e)),
        })?;

    Ok((config, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn home_with(config: Option<&str>) -> TempDir {
        let home = tempfile::tempdir().expect("create tempdir");
        if let Some(contents) = config {
            std::fs::write(home.path().join("config.toml"), contents).expect("write config.toml");
        }
        home
    }

    fn no_env() -> impl Source + Send + Sync + 'static {
        File::from_str("", FileFormat::Toml)
    }

    #[test]
    fn env_override_wins() {
        let (home, source) = resolve_home(Some(PathBuf::from("/custom/home"))).unwrap();
        assert_eq!(home, PathBuf::from("/custom/home"));
        assert_eq!(source, HomeSource::EnvVar);
    }

    #[test]
    fn falls_back_to_exe_dir() {
        let (home, source) = resolve_home(None).unwrap();
        assert_eq!(source, HomeSource::ExeDir);
        assert!(home.is_dir(), "exe dir must exist: {}", home.display());
    }

    #[test]
    fn embedded_default_parses() {
        let config: Config = config::Config::builder()
            .add_source(File::from_str(DEFAULT_CONFIG, FileFormat::Toml))
            .build()
            .and_then(|merged| merged.try_deserialize())
            .expect("embedded default_config.toml must parse into Config");
        assert_eq!(config.log.format, LogFormat::Pretty);
        assert_eq!(config.log.level, "info");
        assert_eq!(
            config.webhook.bind,
            "127.0.0.1:8080"
                .parse::<std::net::SocketAddr>()
                .expect("valid addr")
        );
        assert_eq!(config.webhook.body_limit_bytes, 1_048_576);
        assert_eq!(config.webhook.body_log_limit_bytes, 65_536);
    }

    #[test]
    fn invalid_bind_addr_fails_loud_with_path() {
        let home = home_with(Some("[webhook]\nbind = \"not-an-address\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("config.toml"),
            "error must name the file: {err}"
        );
    }

    #[test]
    fn missing_file_falls_back_to_embedded() {
        let home = home_with(None);
        let (config, source) = load_with_env(home.path(), no_env()).expect("load");
        assert_eq!(source, ConfigSource::Embedded);
        assert_eq!(config.log.level, "info");
    }

    #[test]
    fn file_overrides_embedded() {
        let home = home_with(Some("[log]\nlevel = \"debug\""));
        let (config, source) = load_with_env(home.path(), no_env()).expect("load");
        assert!(matches!(source, ConfigSource::File(_)));
        assert_eq!(config.log.level, "debug");
        // Keys absent from the file keep embedded values:
        assert_eq!(config.log.format, LogFormat::Pretty);
    }

    #[test]
    fn env_layer_overrides_file_overrides_embedded() {
        let home = home_with(Some("[log]\nlevel = \"debug\""));
        let env = File::from_str("[log]\nlevel = \"trace\"", FileFormat::Toml);
        let (config, _) = load_with_env(home.path(), env).expect("load");
        assert_eq!(config.log.level, "trace");
    }

    #[test]
    fn malformed_file_fails_loud_with_path() {
        let home = home_with(Some("[log\nlevel = "));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("config.toml"),
            "error must name the file: {err}"
        );
    }
}
