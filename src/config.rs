use std::fmt;
use std::path::{Path, PathBuf};

use config::{Environment, File, FileFormat, Source};
use serde::Deserialize;

pub const DEFAULT_CONFIG: &str = include_str!("../seed/default_config.toml");

#[derive(Debug, PartialEq)]
pub enum HomeSource {
    EnvVar,
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

#[derive(Debug, thiserror::Error)]
pub enum HomeError {
    #[error("cannot determine executable path: {0}")]
    Exe(#[from] std::io::Error),
    #[error("executable path has no parent directory")]
    NoParent,
}

pub fn resolve_home(env_override: Option<PathBuf>) -> Result<(PathBuf, HomeSource), HomeError> {
    if let Some(home) = env_override {
        return Ok((home, HomeSource::EnvVar));
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().ok_or(HomeError::NoParent)?;
    Ok((dir.to_path_buf(), HomeSource::ExeDir))
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct Config {
    pub log: LogConfig,
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct LogConfig {
    pub format: LogFormat,
    pub level: String,
}

#[derive(Debug, PartialEq, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, PartialEq)]
pub enum ConfigSource {
    Embedded,
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

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("malformed config {path}: {source}")]
    File {
        path: PathBuf,
        source: Box<config::ConfigError>,
    },
    #[error("invalid config: {0}")]
    Invalid(Box<config::ConfigError>),
}

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
        ConfigSource::File(path.clone())
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
