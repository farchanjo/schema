//! `endpoint.toml` reader/writer (ADR-0019, ADR-0021).
//!
//! Each running `schema serve --http` writes `endpoint.toml` to its
//! per-project cache directory so that consumer-side MCP clients (and the
//! `schema service status` verb in ADR-0020) can discover the URL and
//! bearer token. The file lives at
//! `~/.cache/schema/projects/<project_id>/endpoint.toml` with mode `0600`.
//!
//! Naming choice (per ADR-0021): `endpoint.toml` rather than `server.toml`
//! to keep visual distance from the consumer's `schema.toml`.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::de::Error as TomlDeError;
use toml::ser::Error as TomlSerError;

/// On-disk shape of `endpoint.toml`. Bumping `version` is a breaking change
/// for any consumer reading the file; document the migration in
/// `arch/operations/`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Endpoint {
    /// Schema version. Always `1` in FASE 1.0; future bumps need an ADR.
    #[serde(default = "default_version")]
    pub version: u32,

    /// Full HTTP URL the server is bound to (e.g. `http://127.0.0.1:48291`).
    pub url: String,

    /// Bearer token expected on every authenticated request. Rotates on
    /// every server restart.
    pub token: String,

    /// PID of the running server, for `service status` debugging.
    pub pid: u32,

    /// Server start time as RFC 3339 string, for `service status` debugging.
    pub started_at: String,
}

const fn default_version() -> u32 {
    1
}

#[derive(Debug, Error)]
pub enum EndpointError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("toml serialise: {0}")]
    Serialise(#[from] TomlSerError),
    #[error("toml parse: {0}")]
    Parse(#[from] TomlDeError),
    #[error("unsupported endpoint.toml version {0} (this build understands version 1)")]
    UnsupportedVersion(u32),
}

impl Endpoint {
    /// Atomically write `endpoint.toml` at the given path with mode `0600`.
    ///
    /// Mode is enforced via `OpenOptions::mode(0o600)` *before* the first
    /// byte is written, defeating the default `0644` that a plain
    /// `fs::write` would produce. Parent directory is created if missing.
    ///
    /// # Errors
    /// Returns an error if the parent directory cannot be created, the
    /// TOML cannot be serialised, or the file cannot be written.
    pub fn write_atomic(&self, path: &Path) -> Result<(), EndpointError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialised = toml::to_string_pretty(self)?;

        // Write to a sibling tempfile then rename to keep readers from ever
        // seeing a half-written `endpoint.toml`.
        let tmp = path.with_extension("toml.tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(serialised.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Best-effort removal of `endpoint.toml`. ENOENT is swallowed; any
    /// other I/O error is returned. Used in graceful shutdown.
    ///
    /// # Errors
    /// Returns an error if the file exists but cannot be removed.
    pub fn remove_quiet(path: &Path) -> Result<(), EndpointError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(EndpointError::Io(e)),
        }
    }

    /// Load and parse `endpoint.toml`. Returns
    /// [`EndpointError::UnsupportedVersion`] if the file declares a
    /// version this build does not understand.
    ///
    /// # Errors
    /// Returns an error on missing file, parse failure, or unsupported
    /// schema version.
    pub fn load(path: &Path) -> Result<Self, EndpointError> {
        let raw = fs::read_to_string(path)?;
        let parsed: Self = toml::from_str(&raw)?;
        if parsed.version != 1 {
            return Err(EndpointError::UnsupportedVersion(parsed.version));
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::{Endpoint, EndpointError, fs};

    fn sample() -> Endpoint {
        Endpoint {
            version: 1,
            url: "http://127.0.0.1:48291".to_string(),
            token: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_string(),
            pid: 12345,
            started_at: "2026-04-26T18:14:09Z".to_string(),
        }
    }

    /// ADR-0021 fitness function — written `endpoint.toml` has mode `0600`.
    #[test]
    fn write_atomic_enforces_mode_0600() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("endpoint.toml");
        sample().write_atomic(&path).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o600,
            "endpoint.toml must be 0600 at write time"
        );
    }

    #[test]
    fn round_trip_preserves_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("endpoint.toml");
        sample().write_atomic(&path).unwrap();

        let loaded = Endpoint::load(&path).unwrap();
        assert_eq!(loaded, sample());
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("endpoint.toml");
        fs::write(
            &path,
            "version = 99\nurl = \"http://localhost\"\ntoken = \"x\"\npid = 1\nstarted_at = \"now\"\n",
        )
        .unwrap();
        let err = Endpoint::load(&path).unwrap_err();
        assert!(
            matches!(err, EndpointError::UnsupportedVersion(99)),
            "expected UnsupportedVersion(99), got: {err}"
        );
    }

    #[test]
    fn remove_quiet_swallows_enoent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("never-existed.toml");
        Endpoint::remove_quiet(&path).unwrap();
    }

    #[test]
    fn remove_quiet_drops_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("endpoint.toml");
        sample().write_atomic(&path).unwrap();
        assert!(path.exists());
        Endpoint::remove_quiet(&path).unwrap();
        assert!(!path.exists());
    }
}
