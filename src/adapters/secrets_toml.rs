//! `secrets.toml` reader (ADR-0031).
//!
//! The daemon's LLM provider keys live in a sidecar TOML next to the
//! global `endpoint.toml`. The file is mode-`0600`, version-pinned, and
//! read by [`FileSecretStore`] every time the [`crate::ports::LlmProvider`]
//! factory is consulted (no in-memory cache — `SIGHUP` rotation works
//! by overwriting the file with `0600` semantics).
//!
//! ## On-disk shape
//!
//! ```toml
//! version = 1
//!
//! [llm.anthropic]
//! api_key = "sk-ant-..."
//!
//! [llm.openai]
//! api_key = "sk-..."
//! ```
//!
//! Missing sections are not an error; they map to `Ok(None)` from
//! [`FileSecretStore::provider_key`]. A missing file is also `Ok(None)`
//! — that is the legitimate state when the operator wants the daemon
//! to fall back to the env-source compatibility shim during the
//! deprecation window.

use std::fs;
use std::io::{Error as IoError, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ports::{ProviderId, SecretStore, SecretStoreError};

const fn default_version() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct SecretsFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    llm: LlmSection,
}

#[derive(Debug, Default, Deserialize)]
struct LlmSection {
    #[serde(default)]
    anthropic: ProviderSection,
    #[serde(default)]
    openai: ProviderSection,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderSection {
    #[serde(default)]
    api_key: Option<String>,
}

/// File-backed [`SecretStore`] reading the canonical `secrets.toml` path.
///
/// One instance per daemon process. `Arc::clone` is cheap — the inner state
/// is just a [`PathBuf`] and a [`ModeEnforcement`] mode.
#[derive(Debug, Clone)]
pub struct FileSecretStore {
    path: PathBuf,
    enforce_mode: ModeEnforcement,
}

/// Read-side mode-bit enforcement.
///
/// ADR-0031 §"Decision" item 1 says the daemon enforces `0600` on startup;
/// we default to [`Self::Refuse`] so the failure surfaces as a
/// [`SecretStoreError::InsecureMode`] rather than silently shipping a
/// permissive secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeEnforcement {
    /// Refuse to read — return [`SecretStoreError::InsecureMode`].
    Refuse,
    /// Best-effort `chmod 0600`. Logs a `WARN` if the repair fails;
    /// reads the file regardless.
    Repair,
    /// Skip the check (network filesystem, CI fixture, tests).
    Skip,
}

impl FileSecretStore {
    /// Build a store reading the canonical `secrets.toml` path with
    /// strict (`Refuse`) mode enforcement. Use [`Self::with_enforcement`]
    /// when integrating with networks shares or test fixtures that
    /// cannot guarantee POSIX mode bits.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self {
            path,
            enforce_mode: ModeEnforcement::Refuse,
        }
    }

    /// Build a store with an explicit mode-enforcement policy.
    #[must_use]
    pub const fn with_enforcement(path: PathBuf, enforce_mode: ModeEnforcement) -> Self {
        Self { path, enforce_mode }
    }

    /// Resolve the canonical `secrets.toml` path next to the global
    /// `endpoint.toml`.
    ///
    /// - macOS: `~/Library/Application Support/schema/secrets.toml`
    /// - Linux: `~/.local/state/schema/secrets.toml` (matches the
    ///   global endpoint path resolution in `main.rs`)
    ///
    /// # Errors
    /// Returns an error when the home directory cannot be resolved
    /// (rare — embedded systems / chroot without `$HOME`).
    pub fn canonical_path() -> Result<PathBuf, SecretStoreError> {
        let home = dirs::home_dir().ok_or_else(|| {
            SecretStoreError::Io(IoError::new(
                ErrorKind::NotFound,
                "could not resolve home directory for secrets.toml",
            ))
        })?;
        let suffix: &Path = if cfg!(target_os = "macos") {
            Path::new("Library/Application Support/schema/secrets.toml")
        } else {
            Path::new(".local/state/schema/secrets.toml")
        };
        Ok(home.join(suffix))
    }

    fn read_and_parse(&self) -> Result<Option<SecretsFile>, SecretStoreError> {
        match fs::metadata(&self.path) {
            Ok(meta) => self.enforce_or_repair(&meta)?,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SecretStoreError::Io(e)),
        }
        let raw = fs::read_to_string(&self.path)?;
        let parsed: SecretsFile =
            toml::from_str(&raw).map_err(|e| SecretStoreError::Parse(e.to_string()))?;
        if parsed.version != 1 {
            return Err(SecretStoreError::UnsupportedVersion(parsed.version));
        }
        Ok(Some(parsed))
    }

    fn enforce_or_repair(&self, meta: &fs::Metadata) -> Result<(), SecretStoreError> {
        let mode = meta.permissions().mode() & 0o777;
        if mode == 0o600 || self.enforce_mode == ModeEnforcement::Skip {
            return Ok(());
        }
        match self.enforce_mode {
            ModeEnforcement::Refuse => Err(SecretStoreError::InsecureMode {
                path: self.path.display().to_string(),
                mode,
            }),
            ModeEnforcement::Repair => self.repair_mode(mode),
            ModeEnforcement::Skip => Ok(()),
        }
    }

    fn repair_mode(&self, observed: u32) -> Result<(), SecretStoreError> {
        let mut perms = fs::metadata(&self.path)?.permissions();
        perms.set_mode(0o600);
        match fs::set_permissions(&self.path, perms) {
            Ok(()) => {
                tracing::warn!(
                    path = %self.path.display(),
                    observed_mode = format_args!("{observed:#o}"),
                    "secrets: repaired secrets.toml mode to 0600"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "secrets: could not repair secrets.toml mode; reading anyway"
                );
                Ok(())
            }
        }
    }
}

impl SecretStore for FileSecretStore {
    fn provider_key(&self, provider: ProviderId) -> Result<Option<String>, SecretStoreError> {
        let Some(parsed) = self.read_and_parse()? else {
            return Ok(None);
        };
        let key = match provider {
            ProviderId::Anthropic => parsed.llm.anthropic.api_key,
            ProviderId::OpenAi => parsed.llm.openai.api_key,
        };
        Ok(key.filter(|s| !s.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{FileSecretStore, ModeEnforcement};
    use crate::ports::{ProviderId, SecretStore, SecretStoreError};

    fn write_secrets_file(dir: &TempDir, contents: &str, mode: u32) -> PathBuf {
        let path = dir.path().join("secrets.toml");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&path)
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let store = FileSecretStore::new(dir.path().join("does-not-exist.toml"));
        let result = store.provider_key(ProviderId::Anthropic).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn reads_anthropic_key_from_file() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.anthropic]\napi_key = \"sk-test-A\"\n",
            0o600,
        );
        let store = FileSecretStore::new(path);
        let result = store.provider_key(ProviderId::Anthropic).unwrap();
        assert_eq!(result.as_deref(), Some("sk-test-A"));
    }

    #[test]
    fn reads_openai_key_from_file() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.openai]\napi_key = \"sk-test-O\"\n",
            0o600,
        );
        let store = FileSecretStore::new(path);
        let result = store.provider_key(ProviderId::OpenAi).unwrap();
        assert_eq!(result.as_deref(), Some("sk-test-O"));
    }

    #[test]
    fn missing_section_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.anthropic]\napi_key = \"sk-test-A\"\n",
            0o600,
        );
        let store = FileSecretStore::new(path);
        let result = store.provider_key(ProviderId::OpenAi).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_key_treated_as_absent() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.anthropic]\napi_key = \"\"\n",
            0o600,
        );
        let store = FileSecretStore::new(path);
        assert!(store.provider_key(ProviderId::Anthropic).unwrap().is_none());
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(&dir, "version = 99\n", 0o600);
        let store = FileSecretStore::new(path);
        let err = store.provider_key(ProviderId::Anthropic).unwrap_err();
        assert!(
            matches!(err, SecretStoreError::UnsupportedVersion(99)),
            "expected UnsupportedVersion(99), got: {err}"
        );
    }

    #[test]
    fn refuse_mode_rejects_world_readable_file() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.anthropic]\napi_key = \"sk-test\"\n",
            0o644,
        );
        let store = FileSecretStore::new(path);
        let err = store.provider_key(ProviderId::Anthropic).unwrap_err();
        assert!(
            matches!(err, SecretStoreError::InsecureMode { .. }),
            "expected InsecureMode, got: {err}"
        );
    }

    #[test]
    fn skip_mode_ignores_permissive_bits() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.anthropic]\napi_key = \"sk-test\"\n",
            0o644,
        );
        let store = FileSecretStore::with_enforcement(path, ModeEnforcement::Skip);
        let result = store.provider_key(ProviderId::Anthropic).unwrap();
        assert_eq!(result.as_deref(), Some("sk-test"));
    }

    #[test]
    fn repair_mode_chmods_to_0600() {
        let dir = TempDir::new().unwrap();
        let path = write_secrets_file(
            &dir,
            "version = 1\n[llm.anthropic]\napi_key = \"sk-test\"\n",
            0o644,
        );
        let store = FileSecretStore::with_enforcement(path.clone(), ModeEnforcement::Repair);
        store.provider_key(ProviderId::Anthropic).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "repair must downgrade mode to 0600");
    }
}
