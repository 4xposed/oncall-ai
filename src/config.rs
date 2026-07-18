use std::fmt;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};

use config::{Environment, File, FileFormat, Source};
use serde::{Deserialize, Serialize};

/// Load defaults.
pub const DEFAULT_CONFIG: &str = include_str!("../seed/default_config.toml");

/// Where the home directory came from.
#[derive(Debug, PartialEq, Eq)]
pub enum HomeSource {
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
    /// `current_exe` or its canonicalization failed.
    #[error("cannot resolve executable path: {0}")]
    Exe(#[from] std::io::Error),
    #[error("executable path has no parent directory")]
    NoParent,
}

/// Resolves the agent's home: `env_override`, else the executable's dir.
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

/// The full agent configuration. Lenient about unknown top-level keys:
/// `ONCALL_HOME` reaches the merged map as a top-level `home` key.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct Config {
    pub log: LogConfig,
    pub webhook: WebhookConfig,
    pub incidents: IncidentsConfig,
    pub triage: TriageConfig,
}

/// Incident store and dedupe settings.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncidentsConfig {
    pub queue_capacity: NonZeroUsize,
    pub idle_ttl_secs: NonZeroU32,
}

impl IncidentsConfig {
    #[must_use]
    pub fn idle_ttl(&self) -> chrono::TimeDelta {
        chrono::TimeDelta::seconds(i64::from(self.idle_ttl_secs.get()))
    }
}

/// Triage stage: model, endpoint, queue and retry policy.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageConfig {
    pub model: ModelSpec,
    pub endpoint: String,
    pub queue_capacity: NonZeroUsize,
    pub timeout_secs: NonZeroU64,
    pub backoff_initial_ms: NonZeroU64,
    pub backoff_max_ms: u64,
}

impl TriageConfig {
    #[must_use]
    pub fn backoff(&self) -> crate::retry::Backoff {
        crate::retry::Backoff {
            initial: std::time::Duration::from_millis(self.backoff_initial_ms.get()),
            max: std::time::Duration::from_millis(self.backoff_max_ms),
        }
    }
}

/// A model provider recognized in [`ModelSpec`]'s `<provider>:<model>` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProvider {
    Ollama,
}

impl fmt::Display for ModelProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ModelProvider::Ollama => "ollama",
        })
    }
}

/// A model in `<provider>:<model>` form, parsed at load: `ollama:qwen3:8b`
/// is Ollama's `qwen3:8b` (split at the first colon). Parsing is the only
/// constructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelSpec {
    pub(crate) provider: ModelProvider,
    pub(crate) model: String,
}

/// An error from parsing a [`ModelSpec`].
#[derive(Debug, thiserror::Error)]
pub enum ModelSpecError {
    #[error(
        "triage.model must be \"<provider>:<model>\" (e.g. \"ollama:qwen3:8b\"); \
         got {got:?}; supported providers: ollama"
    )]
    BadFormat { got: String },
    #[error("triage.model names unknown provider {provider:?}; supported providers: ollama")]
    UnknownProvider { provider: String },
}

impl TryFrom<String> for ModelSpec {
    type Error = ModelSpecError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.split_once(':') {
            Some((provider, model)) if !model.is_empty() => match provider {
                "ollama" => Ok(ModelSpec {
                    provider: ModelProvider::Ollama,
                    model: model.to_owned(),
                }),
                unknown => Err(ModelSpecError::UnknownProvider {
                    provider: unknown.to_owned(),
                }),
            },
            _ => Err(ModelSpecError::BadFormat { got: value }),
        }
    }
}

impl std::str::FromStr for ModelSpec {
    type Err = ModelSpecError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        ModelSpec::try_from(value.to_owned())
    }
}

impl From<ModelSpec> for String {
    fn from(spec: ModelSpec) -> Self {
        spec.to_string()
    }
}

impl fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.model)
    }
}

/// Per-source webhook settings.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Serve this source under `/webhook/{source}`.
    #[serde(default)]
    pub enabled: bool,
}

/// Webhook server settings. Unknown keys are rejected so a typo'd source
/// table fails loud instead of being silently ignored.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub bind: std::net::SocketAddr,
    /// Largest accepted request body in bytes.
    pub body_limit_bytes: usize,
    /// Longest slice of a body echoed into the intake log line, in bytes.
    pub body_log_limit_bytes: usize,
    #[serde(default)]
    pub grafana: SourceConfig,
    #[serde(default)]
    pub pagerduty: SourceConfig,
}

/// Log output settings.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Loads config: embedded defaults, then `config.toml` under `home`, then
/// `ONCALL_*` env vars. Later layers win.
///
/// # Errors
///
/// Fails on a malformed `config.toml` or an invalid merged result
/// (degenerate `[triage]` or `[incidents]` values, bad `triage.model` form, an unknown
/// `[webhook]` key).
pub fn load(home: &Path) -> Result<(Config, ConfigSource), ConfigError> {
    load_with_env(
        home,
        Environment::with_prefix("ONCALL")
            .prefix_separator("_")
            .separator("__")
            .try_parsing(true),
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

    let config: Config = builder
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
    validate(&config).map_err(|e| ConfigError::Invalid(Box::new(e)))?;

    Ok((config, source))
}

/// Rejects what the types cannot: a backoff cap below the initial delay
/// would shrink the first doubling.
fn validate(config: &Config) -> Result<(), config::ConfigError> {
    if config.triage.backoff_max_ms < config.triage.backoff_initial_ms.get() {
        return Err(config::ConfigError::Message(
            "triage.backoff_max_ms must be at least triage.backoff_initial_ms".to_owned(),
        ));
    }
    validate_endpoint(&config.triage.endpoint)
}

/// A malformed endpoint would otherwise surface as Transport-class errors
/// retried forever, stalling intake behind a boot-time typo.
fn validate_endpoint(endpoint: &str) -> Result<(), config::ConfigError> {
    let invalid = |reason: &str| {
        config::ConfigError::Message(format!(
            "triage.endpoint must be an absolute http(s) URL \
             (e.g. \"http://localhost:11434\"); {reason}: {endpoint:?}"
        ))
    };
    let uri: http::Uri = endpoint
        .parse()
        .map_err(|error| invalid(&format!("cannot parse ({error})")))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(invalid("missing http/https scheme"));
    }
    if uri.host().is_none() {
        return Err(invalid("missing host"));
    }
    Ok(())
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
        assert!(
            !config.webhook.grafana.enabled && !config.webhook.pagerduty.enabled,
            "sources are opt-in: nothing ships enabled"
        );
    }

    #[test]
    fn default_config_has_triage_defaults() {
        let home = home_with(None);
        let (config, _) = load_with_env(home.path(), no_env()).expect("defaults load");
        insta::assert_yaml_snapshot!(config.triage);
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
    fn zero_queue_capacity_fails_loud_with_key() {
        let home = home_with(Some("[triage]\nqueue_capacity = 0"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("triage.queue_capacity"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn zero_timeout_secs_fails_loud_with_key() {
        let home = home_with(Some("[triage]\ntimeout_secs = 0"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("triage.timeout_secs"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn zero_backoff_initial_ms_fails_loud_with_key() {
        let home = home_with(Some("[triage]\nbackoff_initial_ms = 0"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("triage.backoff_initial_ms"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn backoff_max_below_initial_fails_loud_with_key() {
        let home = home_with(Some(
            "[triage]\nbackoff_initial_ms = 100\nbackoff_max_ms = 50",
        ));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::Invalid(_)));
        let message = err.to_string();
        assert!(
            message.contains("triage.backoff_max_ms")
                && message.contains("triage.backoff_initial_ms"),
            "error must name both keys: {err}"
        );
    }

    /// Without a scheme every triage call would fail Transport-class and
    /// retry forever; the typo must die at load instead.
    #[test]
    fn endpoint_without_scheme_fails_loud_with_key() {
        let home = home_with(Some("[triage]\nendpoint = \"localhost:11434\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(
            err.to_string().contains("triage.endpoint"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn unparseable_endpoint_fails_loud_with_key() {
        let home = home_with(Some("[triage]\nendpoint = \" http://localhost:11434\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(
            err.to_string().contains("triage.endpoint"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn model_without_provider_fails_loud() {
        // The pre-provider format: "qwen3:8b" now reads as provider "qwen3".
        let home = home_with(Some("[triage]\nmodel = \"qwen3:8b\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        let message = err.to_string();
        assert!(
            message.contains("triage.model") && message.contains("ollama"),
            "error must name the key and the supported provider: {err}"
        );
    }

    #[test]
    fn unknown_provider_fails_loud_naming_it() {
        let home = home_with(Some("[triage]\nmodel = \"openai:gpt-5.5\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        let message = err.to_string();
        assert!(
            message.contains("openai") && message.contains("ollama"),
            "error must name the unknown provider and the supported ones: {err}"
        );
    }

    #[test]
    fn empty_model_after_provider_fails_loud() {
        let home = home_with(Some("[triage]\nmodel = \"ollama:\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("triage.model"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn triage_backoff_maps_the_millisecond_keys() {
        let home = home_with(Some(
            "[triage]\nbackoff_initial_ms = 100\nbackoff_max_ms = 200",
        ));
        let (config, _) = load_with_env(home.path(), no_env()).expect("valid config loads");
        let backoff = config.triage.backoff();
        assert_eq!(backoff.initial, std::time::Duration::from_millis(100));
        assert_eq!(backoff.max, std::time::Duration::from_millis(200));
    }

    #[test]
    fn model_spec_parses_from_str() {
        let spec: ModelSpec = "ollama:qwen3:8b".parse().expect("valid spec parses");
        assert_eq!(spec.to_string(), "ollama:qwen3:8b");
        "qwen3:8b"
            .parse::<ModelSpec>()
            .expect_err("provider-less spec must not parse");
    }

    #[test]
    fn provider_model_splits_on_first_colon() {
        let home = home_with(Some("[triage]\nmodel = \"ollama:qwen3:8b\""));
        let (config, _) = load_with_env(home.path(), no_env()).expect("valid model loads");
        assert_eq!(
            config.triage.model,
            ModelSpec {
                provider: ModelProvider::Ollama,
                model: "qwen3:8b".to_owned(),
            }
        );
    }

    #[test]
    fn unknown_source_fails_loud_naming_the_supported_ones() {
        let home = home_with(Some("[webhook.datadog]"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        let message = err.to_string();
        assert!(
            message.contains("datadog") && message.contains("grafana"),
            "error must name the unknown source and the supported ones: {err}"
        );
    }

    /// Sources are opt-in: enabling one must not drag the others along.
    #[test]
    fn enabling_one_source_keeps_the_rest_disabled() {
        let home = home_with(Some("[webhook.grafana]\nenabled = true"));
        let (config, _) = load_with_env(home.path(), no_env()).expect("subset loads");
        assert!(
            config.webhook.grafana.enabled,
            "grafana is enabled by the file"
        );
        assert!(
            !config.webhook.pagerduty.enabled,
            "pagerduty keeps its disabled default"
        );
    }

    /// A bare table is not an opt-in; only `enabled = true` is.
    #[test]
    fn bare_source_table_stays_disabled() {
        let home = home_with(Some("[webhook.grafana]"));
        let (config, _) = load_with_env(home.path(), no_env()).expect("bare table loads");
        assert!(!config.webhook.grafana.enabled);
    }

    /// `secret` arrives with signature verification; until then a stray
    /// per-source key is a typo and must not be silently ignored.
    #[test]
    fn unknown_per_source_key_fails_loud() {
        let home = home_with(Some("[webhook.grafana]\nsecret = \"hunter2\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("secret"),
            "error must name the unknown key: {err}"
        );
    }

    /// The webhook tables' fail-loud philosophy, applied to `[triage]`.
    #[test]
    fn unknown_triage_key_fails_loud() {
        let home = home_with(Some("[triage]\nbackoff_maximum_ms = 500"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("backoff_maximum_ms"),
            "error must name the unknown key: {err}"
        );
    }

    #[test]
    fn default_config_has_incident_defaults() {
        let home = home_with(None);
        let (config, _) = load_with_env(home.path(), no_env()).expect("defaults load");
        insta::assert_yaml_snapshot!(config.incidents);
    }

    #[test]
    fn zero_incident_queue_capacity_fails_loud_with_key() {
        let home = home_with(Some("[incidents]\nqueue_capacity = 0"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("incidents.queue_capacity"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn zero_idle_ttl_fails_loud_with_key() {
        let home = home_with(Some("[incidents]\nidle_ttl_secs = 0"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("incidents.idle_ttl_secs"),
            "error must name the key: {err}"
        );
    }

    #[test]
    fn unknown_incidents_key_fails_loud() {
        let home = home_with(Some("[incidents]\nidle_ttl = 60"));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("idle_ttl"),
            "error must name the unknown key: {err}"
        );
    }

    #[test]
    fn idle_ttl_maps_to_a_duration() {
        let home = home_with(Some("[incidents]\nidle_ttl_secs = 60"));
        let (config, _) = load_with_env(home.path(), no_env()).expect("valid config loads");
        assert_eq!(config.incidents.idle_ttl(), chrono::TimeDelta::seconds(60));
    }

    #[test]
    fn unknown_log_key_fails_loud() {
        let home = home_with(Some("[log]\nfromat = \"json\""));
        let err = load_with_env(home.path(), no_env()).expect_err("must fail");
        assert!(matches!(err, ConfigError::File { .. }));
        assert!(
            err.to_string().contains("fromat"),
            "error must name the unknown key: {err}"
        );
    }

    /// `ONCALL_HOME` reaches the merged map as a top-level `home` key, so
    /// the top level must stay lenient or every boot that sets it fails.
    #[test]
    fn top_level_home_key_is_tolerated() {
        let home = home_with(None);
        let env = File::from_str("home = \"/somewhere\"", FileFormat::Toml);
        load_with_env(home.path(), env).expect("top-level home key must not fail the load");
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

    #[test]
    fn env_caused_validation_error_is_not_blamed_on_file() {
        let home = home_with(Some("[log]\nlevel = \"debug\""));
        let env = File::from_str("[triage]\nbackoff_max_ms = 50", FileFormat::Toml);
        let err = load_with_env(home.path(), env).expect_err("must fail");
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(
            !err.to_string().contains("config.toml"),
            "error must not name the file: {err}"
        );
    }
}
