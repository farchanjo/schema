//! `schema secrets migrate` — one-shot env → `secrets.toml` migration
//! per ADR-0031.
//!
//! Reads `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` from the operator's current
//! process environment and writes them into a mode-`0600` `secrets.toml`
//! at the canonical path. Idempotent — refuses to overwrite an existing
//! file unless `--force` is passed. The legacy env-source keeps working
//! during the 30-day deprecation window defined by ADR-0031 (cutover
//! 2026-05-27).

use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::adapters::secrets_toml::FileSecretStore;

/// Parameters for [`migrate`]. Built from clap by the binary.
#[derive(Debug, Clone)]
pub struct MigrateInputs {
    /// Override the destination path (defaults to
    /// [`FileSecretStore::canonical_path`]). Useful for testing.
    pub destination: Option<PathBuf>,
    /// Allow overwriting an existing `secrets.toml`.
    pub force: bool,
}

/// Provider key pair as `(provider_id, api_key)`. The provider id is the
/// stable string emitted in the TOML section header (`anthropic` /
/// `openai`).
pub type KeyPair = (&'static str, String);

/// Perform the migration.
///
/// Returns the destination path on success so the CLI can echo it to the
/// operator. Reads the current process environment for provider keys.
///
/// # Errors
/// Returns an error when no provider key is set in the environment, when
/// the destination already exists and `force` is false, or when the
/// destination cannot be written with mode `0600`.
pub fn migrate(inputs: MigrateInputs) -> Result<PathBuf> {
    let pairs = collect_env_pairs();
    if pairs.is_empty() {
        anyhow::bail!(
            "no provider key set in environment (ANTHROPIC_API_KEY / OPENAI_API_KEY); \
             nothing to migrate"
        );
    }
    migrate_with_pairs(inputs, &pairs)
}

/// Lower-level migration entry point — caller supplies the key pairs
/// directly. Used by tests to avoid mutating process-global env state.
///
/// # Errors
/// Same as [`migrate`].
pub fn migrate_with_pairs(inputs: MigrateInputs, pairs: &[KeyPair]) -> Result<PathBuf> {
    if pairs.is_empty() {
        anyhow::bail!("no provider keys supplied; nothing to migrate");
    }
    let dest = resolve_destination(inputs.destination)?;
    if dest.exists() && !inputs.force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite (mode-0600 will be re-applied)",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }
    write_secrets_file(&dest, pairs)?;
    Ok(dest)
}

fn resolve_destination(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p);
    }
    FileSecretStore::canonical_path()
        .map_err(|e| anyhow::anyhow!("resolving canonical secrets.toml path: {e}"))
}

fn collect_env_pairs() -> Vec<KeyPair> {
    let mut out = Vec::new();
    if let Ok(v) = env::var("ANTHROPIC_API_KEY")
        && !v.is_empty()
    {
        out.push(("anthropic", v));
    }
    if let Ok(v) = env::var("OPENAI_API_KEY")
        && !v.is_empty()
    {
        out.push(("openai", v));
    }
    out
}

fn write_secrets_file(dest: &Path, pairs: &[KeyPair]) -> Result<()> {
    let body = render_toml(pairs);
    let tmp = dest.with_extension("toml.tmp");
    write_tmp(&tmp, body.as_bytes())?;
    rename_or_recover(&tmp, dest)
}

fn write_tmp(tmp: &Path, body: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)
        .with_context(|| format!("opening {} for write", tmp.display()))?;
    file.write_all(body)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", tmp.display()))
}

fn rename_or_recover(tmp: &Path, dest: &Path) -> Result<()> {
    match fs::rename(tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(tmp).ok();
            Err(anyhow::anyhow!(
                "{} appeared between check and rename; aborting",
                dest.display()
            ))
        }
        Err(e) => Err(anyhow::Error::from(e))
            .with_context(|| format!("renaming {} to {}", tmp.display(), dest.display())),
    }
}

fn render_toml(pairs: &[KeyPair]) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("# Written by `schema secrets migrate` (ADR-0031).\n");
    out.push_str("# Mode 0600 enforced; do NOT commit this file.\n");
    out.push_str("version = 1\n\n");
    for (provider, key) in pairs {
        let _ = writeln!(out, "[llm.{provider}]");
        let _ = writeln!(out, "api_key = \"{}\"", escape(key));
        out.push('\n');
    }
    out
}

fn escape(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::fs::{metadata, read_to_string};
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::{KeyPair, MigrateInputs, migrate_with_pairs, render_toml};

    fn pair(provider: &'static str, key: &str) -> KeyPair {
        (provider, key.to_owned())
    }

    #[test]
    fn render_toml_pins_version_and_emits_provider_sections() {
        let body = render_toml(&[pair("anthropic", "sk-A"), pair("openai", "sk-O")]);
        assert!(body.contains("version = 1"));
        assert!(body.contains("[llm.anthropic]"));
        assert!(body.contains("api_key = \"sk-A\""));
        assert!(body.contains("[llm.openai]"));
        assert!(body.contains("api_key = \"sk-O\""));
    }

    #[test]
    fn render_toml_escapes_quotes_in_keys() {
        let body = render_toml(&[pair("anthropic", "sk-with\"quote")]);
        assert!(body.contains("api_key = \"sk-with\\\"quote\""));
    }

    #[test]
    fn migrate_with_pairs_writes_mode_0600_file() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("secrets.toml");
        let pairs = vec![pair("anthropic", "sk-test-migrate")];
        let written = migrate_with_pairs(
            MigrateInputs {
                destination: Some(dest.clone()),
                force: false,
            },
            &pairs,
        )
        .unwrap();
        assert_eq!(written, dest);
        let mode = metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secrets.toml must be 0600");
        let body = read_to_string(&dest).unwrap();
        assert!(body.contains("[llm.anthropic]"));
        assert!(body.contains("sk-test-migrate"));
    }

    #[test]
    fn migrate_with_pairs_refuses_to_overwrite_without_force() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("secrets.toml");
        let pairs = vec![pair("anthropic", "sk-A")];
        migrate_with_pairs(
            MigrateInputs {
                destination: Some(dest.clone()),
                force: false,
            },
            &pairs,
        )
        .unwrap();
        let err = migrate_with_pairs(
            MigrateInputs {
                destination: Some(dest),
                force: false,
            },
            &pairs,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn migrate_with_pairs_force_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("secrets.toml");
        let first = vec![pair("anthropic", "sk-first")];
        migrate_with_pairs(
            MigrateInputs {
                destination: Some(dest.clone()),
                force: false,
            },
            &first,
        )
        .unwrap();
        let second = vec![pair("anthropic", "sk-second")];
        migrate_with_pairs(
            MigrateInputs {
                destination: Some(dest.clone()),
                force: true,
            },
            &second,
        )
        .unwrap();
        let body = read_to_string(&dest).unwrap();
        assert!(body.contains("sk-second"));
        assert!(!body.contains("sk-first"));
    }

    #[test]
    fn migrate_with_pairs_rejects_empty_input() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("secrets.toml");
        let err = migrate_with_pairs(
            MigrateInputs {
                destination: Some(dest),
                force: false,
            },
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing to migrate"));
    }
}
