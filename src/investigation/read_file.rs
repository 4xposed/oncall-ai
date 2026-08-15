use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};

use crate::config::InvestigationConfig;
use crate::investigation::Truncated;
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use tokio::io::AsyncReadExt as _;

const TRUNCATION_PREFIX: &str = "\n[truncated:";

/// An error from constructing [`ReadFile`].
#[derive(Debug, thiserror::Error)]
#[error("cannot resolve investigation.repo_root ({}): {source}", path.display())]
pub struct BuildError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadFileError {
    #[error("path must be relative to the repo root, not absolute")]
    Absolute,
    #[error("path escapes the repo root; use a path inside the repo root, with no `..` segments")]
    Escapes,
    #[error("no such file under the repo root")]
    NotFound,
    #[error("that path is a directory, not a file; name a file inside it")]
    IsADirectory,
    #[error("that file is not UTF-8 text; read_file only reads text files")]
    NotText,
    #[error("cannot read that path; check it names a readable file under the repo root")]
    Unreadable {
        #[source]
        source: std::io::Error,
    },
}

/// Arguments for the `read_file` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct Args {
    #[schemars(
        description = "Path of the file to read, relative to the repository root, e.g. \"src/main.rs\". Absolute paths and paths outside the repository are rejected."
    )]
    pub path: String,
}

pub struct ReadFile {
    repo_root: PathBuf,
    max_file_bytes: NonZeroUsize,
}

impl ReadFile {
    /// # Errors
    ///
    /// Fails when `repo_root` cannot be canonicalized.
    pub fn new(config: &InvestigationConfig) -> Result<Self, BuildError> {
        let repo_root = config
            .repo_root
            .canonicalize()
            .map_err(|source| BuildError {
                path: config.repo_root.clone(),
                source,
            })?;
        Ok(Self {
            repo_root,
            max_file_bytes: config.max_file_bytes,
        })
    }

    /// The directory reads are pinned to, as resolved at construction.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    async fn resolve(&self, path: &str) -> Result<PathBuf, ReadFileError> {
        let requested = Path::new(path);
        if requested.is_absolute() {
            return Err(ReadFileError::Absolute);
        }
        // Before touching the filesystem, so a traversal to a path that
        // happens not to exist is not reported as a missing file.
        if requested
            .components()
            .any(|part| part == Component::ParentDir)
        {
            return Err(ReadFileError::Escapes);
        }
        let joined = self.repo_root.join(requested);
        let resolved =
            tokio::fs::canonicalize(joined)
                .await
                .map_err(|source| match source.kind() {
                    std::io::ErrorKind::NotFound => ReadFileError::NotFound,
                    _ => ReadFileError::Unreadable { source },
                })?;
        // Canonical on both sides, so this also catches symlinks out.
        if !resolved.starts_with(&self.repo_root) {
            return Err(ReadFileError::Escapes);
        }
        Ok(resolved)
    }

    /// Reads at most one byte past the cap, so a file the model names can never be allocated whole.
    /// The extra byte is what tells "exactly at the cap" apart from "longer than it".
    async fn read_capped(&self, path: &Path) -> Result<(String, u64), ReadFileError> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|source| ReadFileError::Unreadable { source })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| ReadFileError::Unreadable { source })?;
        if metadata.is_dir() {
            return Err(ReadFileError::IsADirectory);
        }
        let limit = u64::try_from(self.max_file_bytes.get())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();

        file.take(limit)
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| ReadFileError::Unreadable { source })?;

        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error)
                if error.error_len().is_none()
                    && metadata.len() > u64::try_from(bytes.len()).unwrap_or(u64::MAX) =>
            {
                bytes
                    .get(..error.valid_up_to())
                    .and_then(|valid| std::str::from_utf8(valid).ok())
                    .ok_or(ReadFileError::NotText)?
            }
            Err(_) => return Err(ReadFileError::NotText),
        };
        Ok((text.to_owned(), metadata.len()))
    }

    /// Appends a notice unless the model is seeing every byte of the file: it
    /// must know when it did not see everything. The flag says the same thing
    /// to the transcript, where prose in the output cannot be trusted.
    fn present(&self, text: &str, total_bytes: u64) -> (String, Truncated) {
        let kept = crate::text::truncate_utf8(text, self.max_file_bytes.get());
        if usize::try_from(total_bytes).is_ok_and(|total| kept.len() == total) {
            return (kept.to_owned(), Truncated(false));
        }
        (
            format!(
                "{kept}{TRUNCATION_PREFIX} first {} of {total_bytes} bytes shown]",
                kept.len(),
            ),
            Truncated(true),
        )
    }
}

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Args = Args;
    type Output = String;
    type Error = ReadFileError;

    fn description(&self) -> String {
        "Read a UTF-8 text file from the repository under investigation. Paths are relative \
         to the repository root; paths that resolve outside it are rejected. Long files are \
         truncated."
            .to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        schemars::schema_for!(Args).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        let message = error.to_string();
        let mapped = match &error {
            ReadFileError::Absolute | ReadFileError::Escapes => {
                ToolExecutionError::permission_denied(message)
            }
            ReadFileError::NotFound => ToolExecutionError::not_found(message),
            ReadFileError::IsADirectory | ReadFileError::NotText => {
                ToolExecutionError::invalid_args(message)
            }
            ReadFileError::Unreadable { .. } => ToolExecutionError::other(message),
        };
        mapped.with_source(error)
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let path = self.resolve(&args.path).await?;
        let (text, total_bytes) = self.read_capped(&path).await?;
        let (presented, truncated) = self.present(&text, total_bytes);
        context.insert_result(truncated);
        Ok(presented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InvestigationConfig, ModelProvider, ModelSpec};
    use std::num::{NonZeroU64, NonZeroUsize};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn test_config(repo_root: &std::path::Path, max_file_bytes: usize) -> InvestigationConfig {
        InvestigationConfig {
            model: ModelSpec {
                provider: ModelProvider::Ollama,
                model: "test-model".to_owned(),
            },
            endpoint: "http://127.0.0.1:11434".to_owned(),
            repo_root: repo_root.to_path_buf(),
            max_turns: NonZeroUsize::new(8).expect("nonzero"),
            timeout_secs: NonZeroU64::new(5).expect("nonzero"),
            max_file_bytes: NonZeroUsize::new(max_file_bytes).expect("nonzero"),
            queue_capacity: NonZeroUsize::new(8).expect("nonzero"),
        }
    }

    /// A repo root nested inside the tempdir, so files can be planted outside
    /// the root without escaping the fixture.
    struct Fixture {
        base: TempDir,
        tool: ReadFile,
    }

    impl Fixture {
        fn new(max_file_bytes: usize) -> Self {
            let base = tempfile::tempdir().expect("create tempdir");
            let root = base.path().join("repo");
            std::fs::create_dir_all(&root).expect("create repo root");
            let tool =
                ReadFile::new(&test_config(&root, max_file_bytes)).expect("read_file tool builds");
            Self { base, tool }
        }

        fn root(&self) -> PathBuf {
            self.base.path().join("repo")
        }

        fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
            let path = self.root().join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(path, contents).expect("write file");
        }

        async fn read(&self, path: &str) -> Result<String, ReadFileError> {
            self.read_with_context(path).await.0
        }

        async fn read_with_context(
            &self,
            path: &str,
        ) -> (Result<String, ReadFileError>, ToolContext) {
            let mut context = ToolContext::new();
            let text = self
                .tool
                .call(
                    &mut context,
                    Args {
                        path: path.to_owned(),
                    },
                )
                .await;
            (text, context)
        }
    }

    #[tokio::test]
    async fn reads_a_file_under_the_root() {
        let fixture = Fixture::new(1024);
        fixture.write("src/main.rs", "fn main() {}\n");
        let text = fixture.read("src/main.rs").await.expect("reads the file");
        assert_eq!(text, "fn main() {}\n");
    }

    #[tokio::test]
    async fn incomplete_utf8_at_eof_is_rejected_as_not_text() {
        let fixture = Fixture::new(1024);
        fixture.write("src/main.rs", [0xc3]);

        let error = fixture
            .read("src/main.rs")
            .await
            .expect_err("incomplete UTF-8 at EOF is not text");
        assert!(matches!(error, ReadFileError::NotText));
    }

    #[test]
    fn model_guidance_states_the_lookup_time_containment_rule() {
        let fixture = Fixture::new(1024);
        let description = fixture.tool.description();
        let precise_claim = "paths that resolve outside it are rejected";
        let absolute_claim = "nothing outside it can be read";

        for guidance in [
            description.as_str(),
            crate::investigation::INVESTIGATION_PREAMBLE,
        ] {
            assert!(guidance.contains(precise_claim), "guidance: {guidance}");
            assert!(!guidance.contains(absolute_claim), "guidance: {guidance}");
        }
    }

    /// rig's default `map_error` would replace the message with generic
    /// kind-level feedback, leaving the model nothing to act on.
    #[test]
    fn failures_reach_the_model_as_readable_feedback() {
        let fixture = Fixture::new(1024);
        let escape = fixture.tool.map_error(ReadFileError::Escapes);
        assert_eq!(
            escape.kind(),
            rig_agent::tool::ToolErrorKind::PermissionDenied
        );
        assert_eq!(
            escape.model_feedback(),
            Some(ReadFileError::Escapes.to_string().as_str())
        );
        let missing = fixture.tool.map_error(ReadFileError::NotFound);
        assert_eq!(missing.kind(), rig_agent::tool::ToolErrorKind::NotFound);
    }

    /// The root is resolved once, at construction, so an operator can see what
    /// a relative `repo_root` actually pinned the tool to.
    #[test]
    fn repo_root_is_canonicalized_at_construction() {
        let fixture = Fixture::new(1024);
        let canonical = fixture.root().canonicalize().expect("root canonicalizes");
        assert_eq!(fixture.tool.repo_root(), canonical);
    }

    #[tokio::test]
    async fn rejects_parent_traversal() {
        let fixture = Fixture::new(1024);
        std::fs::write(fixture.base.path().join("secrets.txt"), "top secret\n")
            .expect("write secret outside the root");
        let error = fixture
            .read("../secrets.txt")
            .await
            .expect_err("traversal must be rejected");
        assert!(matches!(error, ReadFileError::Escapes), "got: {error:?}");
        assert!(
            error.to_string().contains("repo root"),
            "message must tell the model what to do instead: {error}"
        );

        // A traversal onto a path that happens not to exist is still an
        // escape: canonicalization alone would report it as a missing file.
        let error = fixture
            .read("../no-such-secret.txt")
            .await
            .expect_err("traversal must be rejected");
        assert!(matches!(error, ReadFileError::Escapes), "got: {error:?}");
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let fixture = Fixture::new(1024);
        let error = fixture
            .read("/etc/passwd")
            .await
            .expect_err("absolute paths must be rejected");
        assert!(matches!(error, ReadFileError::Absolute), "got: {error:?}");
        assert!(
            error.to_string().contains("relative"),
            "message must tell the model what to do instead: {error}"
        );
    }

    /// Only canonicalization catches this: the path is plainly relative and
    /// has no `..`, yet it lands outside the root.
    #[tokio::test]
    async fn rejects_symlink_escaping_the_root() {
        let fixture = Fixture::new(1024);
        let secret = fixture.base.path().join("secrets.txt");
        std::fs::write(&secret, "top secret\n").expect("write secret outside the root");
        std::os::unix::fs::symlink(&secret, fixture.root().join("link.txt"))
            .expect("create symlink");
        let error = fixture
            .read("link.txt")
            .await
            .expect_err("a symlink out of the root must be rejected");
        assert!(matches!(error, ReadFileError::Escapes), "got: {error:?}");
    }

    #[tokio::test]
    async fn truncates_at_the_byte_cap_on_a_char_boundary() {
        // 'é' is two bytes, so a cut at 9 would split the fifth one.
        let fixture = Fixture::new(9);
        fixture.write("notes.txt", "é".repeat(10));
        let text = fixture.read("notes.txt").await.expect("reads the file");
        let (body, notice) = text
            .split_once(TRUNCATION_PREFIX)
            .expect("truncation must be stated in the returned text");
        assert_eq!(body, "é".repeat(4), "the cut must land on a char boundary");
        assert!(body.len() <= 9, "body must fit the cap: {}", body.len());
        assert!(
            notice.contains("20"),
            "notice must state the full size: {notice}"
        );
    }

    /// The transcript records whether the model saw a whole file, and must not
    /// have to infer it from prose the file itself could contain.
    #[tokio::test]
    async fn truncation_is_published_as_result_metadata() {
        let fixture = Fixture::new(4);
        fixture.write("notes.txt", "abcdefgh");
        let (text, context) = fixture.read_with_context("notes.txt").await;
        assert!(text.is_ok(), "reads the file");
        assert_eq!(context.result::<Truncated>(), Some(&Truncated(true)));

        fixture.write("short.txt", "abc");
        let (_, context) = fixture.read_with_context("short.txt").await;
        assert_eq!(context.result::<Truncated>(), Some(&Truncated(false)));
    }

    /// A file whose own contents end in the notice is not a truncated read.
    #[tokio::test]
    async fn output_that_merely_looks_truncated_carries_no_truncation() {
        let fixture = Fixture::new(1024);
        fixture.write(
            "notes.txt",
            "log line\n[truncated: first 8 of 99 bytes shown]",
        );
        let (_, context) = fixture.read_with_context("notes.txt").await;
        assert_eq!(context.result::<Truncated>(), Some(&Truncated(false)));
    }

    /// The cap must bound the read itself, not just what the model is shown:
    /// a file the model names must never be allocated whole.
    #[tokio::test]
    async fn stops_reading_at_the_cap() {
        // 'é' is two bytes, so the last byte read splits one: the cut must
        // yield whole characters, not an "unreadable" file.
        let fixture = Fixture::new(8);
        let mut contents = "é".repeat(5).into_bytes();
        // Invalid UTF-8 past the cap. Reading the whole file would fail here,
        // so this stays green only while the bytes past the cap go untouched.
        contents.resize(4 * 1024 * 1024, 0xff);
        fixture.write("huge.log", &contents);

        let text = fixture.read("huge.log").await.expect("reads the file");
        let (body, notice) = text
            .split_once(TRUNCATION_PREFIX)
            .expect("truncation must be stated in the returned text");
        assert_eq!(body, "é".repeat(4), "the cut must land on a char boundary");
        assert!(
            notice.contains(&contents.len().to_string()),
            "notice must report the whole file's size ({}): {notice}",
            contents.len()
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_error_result_not_a_panic() {
        let fixture = Fixture::new(1024);
        let error = fixture
            .read("no/such/file.rs")
            .await
            .expect_err("a missing file must be an error result");
        assert!(matches!(error, ReadFileError::NotFound), "got: {error:?}");
    }

    #[tokio::test]
    async fn directory_is_an_error_result_not_an_io_error() {
        let fixture = Fixture::new(1024);
        fixture.write("src/main.rs", "fn main() {}\n");
        let error = fixture
            .read("src")
            .await
            .expect_err("a directory must be an error result");
        assert!(
            matches!(error, ReadFileError::IsADirectory),
            "got: {error:?}"
        );
    }
}
