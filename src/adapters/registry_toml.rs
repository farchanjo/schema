//! `registry.toml` reader/writer (ADR-0026 amendment of ADR-0020).
//!
//! The shared daemon owns the set of registered projects via
//! `schema project register / unregister / list` (ADR-0020 amendment).
//! Project membership is **explicit** under ADR-0026 — the daemon does
//! not infer projects from CLI flags any longer; it reads them from
//! this file at startup and refreshes its in-memory state when the CLI
//! verbs mutate it.
//!
//! Path conventions (resolved by [`default_path`]):
//!
//! - macOS: `~/Library/Application Support/schema/registry.toml`
//! - Linux: `~/.local/state/schema/registry.toml`
//!
//! Shape choices:
//!
//! - The on-disk format is `Vec<ProjectEntry>` under one `[[project]]`
//!   table-array, **not** a `HashMap` keyed by `project_id`. Keeping it
//!   ordered preserves the order projects were registered (operator
//!   muscle memory) and survives a stable `schema project list` output.
//! - Each entry stores the absolute path of the consumer's
//!   `schema.toml`. The daemon re-resolves [`crate::adapters::project_identity::ProjectId`]
//!   from the loaded config on startup, which means moving the consumer
//!   directory invalidates the entry — registered explicitly by the
//!   operator, fixed by `unregister` + `register`.
//! - File mode is the default `0644`. This file holds **paths and ids**,
//!   not secrets; bearer tokens stay in per-project `endpoint.toml`
//!   (ADR-0021, mode `0600`) and never appear here.
//!
//! Naming: `registry.toml` to keep visual distance from the consumer's
//! `schema.toml` and from per-project `endpoint.toml`.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::de::Error as TomlDeError;
use toml::ser::Error as TomlSerError;

/// One registered project. Bumping field shape requires bumping
/// [`Registry::version`] and writing a migration in
/// `arch/operations/runbook.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectEntry {
    /// Stable per-machine project identifier (output of
    /// [`crate::adapters::project_identity::ProjectId`] for the
    /// resolved consumer root). Unique across the registry.
    pub project_id: String,

    /// Project name as declared in the consumer's `schema.toml`
    /// `[project] name = "..."`. Stored for human-readable
    /// `schema project list` output; not load-bearing for resolution.
    pub name: String,

    /// Absolute path of the consumer's `schema.toml`. Re-read on every
    /// daemon startup to rediscover the corpus. Stored as `String` so
    /// the file round-trips through TOML unchanged across platforms.
    pub schema_toml_path: String,

    /// RFC 3339 timestamp of the most recent registration / re-registration.
    /// Diagnostic only.
    pub registered_at: String,
}

/// On-disk shape of `registry.toml`. The wrapping struct exists so the
/// schema version is part of the file header and so future fields
/// (like `[defaults]` knobs the daemon honours globally) have a home.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registry {
    /// Schema version. Always `1` in FASE 1.0; future bumps need an ADR.
    #[serde(default = "default_version")]
    pub version: u32,

    /// Registered projects, in insertion order.
    #[serde(default, rename = "project")]
    pub projects: Vec<ProjectEntry>,
}

const fn default_version() -> u32 {
    1
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("toml serialise: {0}")]
    Serialise(#[from] TomlSerError),
    #[error("toml parse: {0}")]
    Parse(#[from] TomlDeError),
    #[error("unsupported registry.toml version {0} (this build understands version 1)")]
    UnsupportedVersion(u32),
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 1,
            projects: Vec::new(),
        }
    }
}

impl Registry {
    /// Empty registry (version 1, zero projects).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve the platform-default registry path.
    ///
    /// macOS: `~/Library/Application Support/schema/registry.toml`.
    /// Linux: `~/.local/state/schema/registry.toml`.
    /// Other platforms: same as Linux as a best-effort fallback —
    /// formal Windows support is out of scope per ADR-0020.
    ///
    /// # Errors
    /// Returns an error if the home directory cannot be resolved.
    pub fn default_path() -> Result<PathBuf, RegistryError> {
        let home = dirs::home_dir().ok_or_else(|| {
            RegistryError::Io(io::Error::new(
                ErrorKind::NotFound,
                "could not resolve home directory for registry.toml",
            ))
        })?;
        let suffix: &Path = if cfg!(target_os = "macos") {
            Path::new("Library/Application Support/schema/registry.toml")
        } else {
            Path::new(".local/state/schema/registry.toml")
        };
        Ok(home.join(suffix))
    }

    /// Load the registry from `path`. A missing file is treated as
    /// equivalent to an empty registry (the daemon has just been
    /// installed and no project has been registered yet); other I/O
    /// errors propagate.
    ///
    /// # Errors
    /// Returns an error on parse failure or on an unsupported schema
    /// version. Missing-file does **not** error — it returns an empty
    /// [`Registry`] so a fresh install works without operator
    /// pre-creation.
    pub fn load(path: &Path) -> Result<Self, RegistryError> {
        match fs::read_to_string(path) {
            Ok(raw) => {
                let parsed: Self = toml::from_str(&raw)?;
                if parsed.version != 1 {
                    return Err(RegistryError::UnsupportedVersion(parsed.version));
                }
                Ok(parsed)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(RegistryError::Io(e)),
        }
    }

    /// Atomically write the registry to `path`. Parent directory is
    /// created if missing; default `0644` mode (file is not a secret).
    /// Tempfile + rename keeps readers from seeing a half-written
    /// state.
    ///
    /// # Errors
    /// Returns an error if the parent directory cannot be created, the
    /// TOML cannot be serialised, or the file cannot be written.
    pub fn save_atomic(&self, path: &Path) -> Result<(), RegistryError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialised = toml::to_string_pretty(self)?;

        let tmp = path.with_extension("toml.tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(serialised.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Insert (or replace) an entry by `project_id`. Replacement
    /// preserves position in the order; insertion appends. Returns
    /// `true` if an existing entry was overwritten (re-registration),
    /// `false` if the entry is new.
    pub fn upsert(&mut self, entry: ProjectEntry) -> bool {
        if let Some(existing) = self
            .projects
            .iter_mut()
            .find(|p| p.project_id == entry.project_id)
        {
            *existing = entry;
            true
        } else {
            self.projects.push(entry);
            false
        }
    }

    /// Remove an entry by `project_id`. Returns the removed entry, or
    /// `None` if nothing matched.
    #[must_use = "the removed entry may be inspected to confirm a prior registration"]
    pub fn remove(&mut self, project_id: &str) -> Option<ProjectEntry> {
        let index = self
            .projects
            .iter()
            .position(|p| p.project_id == project_id)?;
        Some(self.projects.remove(index))
    }

    /// Look up an entry by `project_id`.
    #[must_use]
    pub fn find(&self, project_id: &str) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.project_id == project_id)
    }

    /// Number of registered projects.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.projects.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use tempfile::TempDir;

    use super::{ProjectEntry, Registry, RegistryError, fs};

    fn entry(project_id: &str, name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            project_id: project_id.to_string(),
            name: name.to_string(),
            schema_toml_path: path.to_string(),
            registered_at: "2026-04-26T18:14:09Z".to_string(),
        }
    }

    #[test]
    fn missing_file_loads_as_empty_registry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.toml");
        let registry = Registry::load(&path).unwrap();
        assert_eq!(registry.version, 1);
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn round_trip_preserves_fields_and_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.toml");
        let mut registry = Registry::new();
        let new_alpha = registry.upsert(entry("alpha-1", "alpha", "/dev/alpha/schema.toml"));
        let new_beta = registry.upsert(entry("beta-2", "beta", "/dev/beta/schema.toml"));
        assert!(!new_alpha);
        assert!(!new_beta);
        registry.save_atomic(&path).unwrap();

        let loaded = Registry::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.projects[0].project_id, "alpha-1");
        assert_eq!(loaded.projects[1].project_id, "beta-2");
        assert_eq!(loaded, registry);
    }

    #[test]
    fn upsert_replaces_existing_entry_in_place() {
        let mut registry = Registry::new();
        registry.upsert(entry("alpha-1", "alpha", "/old/path/schema.toml"));
        registry.upsert(entry("beta-2", "beta", "/dev/beta/schema.toml"));

        let replaced = registry.upsert(entry("alpha-1", "alpha", "/new/path/schema.toml"));
        assert!(replaced, "re-registration should report a replace");
        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.projects[0].schema_toml_path,
            "/new/path/schema.toml"
        );
        // beta still in second slot — order preserved.
        assert_eq!(registry.projects[1].project_id, "beta-2");
    }

    #[test]
    fn remove_returns_dropped_entry() {
        let mut registry = Registry::new();
        registry.upsert(entry("alpha-1", "alpha", "/dev/alpha/schema.toml"));
        registry.upsert(entry("beta-2", "beta", "/dev/beta/schema.toml"));

        let removed = registry.remove("alpha-1").unwrap();
        assert_eq!(removed.name, "alpha");
        assert_eq!(registry.len(), 1);
        assert!(registry.find("alpha-1").is_none());
        assert!(registry.find("beta-2").is_some());
    }

    #[test]
    fn remove_unknown_project_returns_none() {
        let mut registry = Registry::new();
        registry.upsert(entry("alpha-1", "alpha", "/dev/alpha/schema.toml"));
        assert!(registry.remove("no-such-project").is_none());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.toml");
        fs::write(
            &path,
            "version = 99\n[[project]]\nproject_id = \"x\"\nname = \"x\"\nschema_toml_path = \"/x\"\nregistered_at = \"now\"\n",
        )
        .unwrap();
        let err = Registry::load(&path).unwrap_err();
        assert!(
            matches!(err, RegistryError::UnsupportedVersion(99)),
            "expected UnsupportedVersion(99), got: {err}"
        );
    }

    #[test]
    fn save_creates_parent_directory_if_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("schema/sub").join("registry.toml");
        let mut registry = Registry::new();
        registry.upsert(entry("alpha-1", "alpha", "/dev/alpha/schema.toml"));

        registry.save_atomic(&nested).unwrap();
        assert!(nested.exists());

        let loaded = Registry::load(&nested).unwrap();
        assert_eq!(loaded, registry);
    }

    #[test]
    fn default_path_lives_under_home_directory() {
        let path = Registry::default_path().unwrap();
        let home = dirs::home_dir().unwrap();
        assert!(
            path.starts_with(&home),
            "registry default path {} must live under home {}",
            path.display(),
            home.display(),
        );
        assert!(path.ends_with("registry.toml"));
    }
}
