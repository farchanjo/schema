//! Project identity + cache directory resolution.
//!
//! The cache root is `~/.cache/schema/projects/<id>/`, where `<id>` is derived
//! from the project's *name* and the BLAKE3 hash of its *canonical absolute
//! path*. Renaming or moving the project produces a fresh cache directory; two
//! projects with the same name in different paths never collide.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HASH_PREFIX_LEN: usize = 8; // 64 bits — collision-safe for human-scale namespaces

/// Stable identifier for a project on this machine.
///
/// Format: `<sanitised-name>-<8-hex-of-blake3-of-canonical-path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectId(String);

impl ProjectId {
    /// Compute the project ID from a project name and its canonical absolute path.
    pub fn new(name: &str, canonical_path: &Path) -> Self {
        let hash = blake3::hash(canonical_path.as_os_str().as_encoded_bytes());
        let prefix: String = hash
            .to_hex()
            .chars()
            .take(HASH_PREFIX_LEN * 2) // each hex char = 4 bits → 16 chars = 64 bits
            .collect();
        let sanitised = sanitise(name);
        Self(format!("{sanitised}-{prefix}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolved project paths and identity.
#[derive(Debug, Clone)]
pub struct ProjectIdentity {
    pub id: ProjectId,
    pub root: PathBuf,
    pub cache_dir: PathBuf,
    pub lance_dir: PathBuf,
    pub metadata_path: PathBuf,
    pub lock_path: PathBuf,
}

impl ProjectIdentity {
    /// Resolve identity for a project rooted at `project_root` with the given name.
    pub fn resolve(name: &str, project_root: &Path) -> Result<Self> {
        let canonical_root = project_root
            .canonicalize()
            .with_context(|| format!("canonicalising project root {}", project_root.display()))?;
        let id = ProjectId::new(name, &canonical_root);
        let cache_root = cache_root()?;
        let cache_dir = cache_root.join("projects").join(id.as_str());
        let lance_dir = cache_dir.join("lance");
        let metadata_path = cache_dir.join("metadata.toml");
        let lock_path = cache_dir.join("lock");

        Ok(Self {
            id,
            root: canonical_root,
            cache_dir,
            lance_dir,
            metadata_path,
            lock_path,
        })
    }

    /// Ensure the cache directory exists on disk. Idempotent.
    pub fn ensure_cache_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("creating cache dir {}", self.cache_dir.display()))?;
        Ok(())
    }
}

/// Resolve `~/.cache/schema/` (or platform equivalent via `dirs::cache_dir`).
pub fn cache_root() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve user cache dir (HOME unset?)"))?;
    Ok(base.join("schema"))
}

/// Sanitise a project name into a filesystem-safe identifier prefix.
/// Allows `[a-z0-9-]`; replaces other chars with `-` and lowercases.
fn sanitise(name: &str) -> String {
    let lowered = name.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_dash = false;
    for ch in lowered.chars() {
        let safe = matches!(ch, 'a'..='z' | '0'..='9' | '-');
        if safe {
            out.push(ch);
            prev_dash = ch == '-';
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]
    use super::*;

    #[test]
    fn sanitises_unicode_and_special_chars() {
        assert_eq!(sanitise("Lowcow Platform"), "lowcow-platform");
        assert_eq!(sanitise("foo/bar"), "foo-bar");
        assert_eq!(sanitise("---a---"), "a");
        assert_eq!(sanitise(""), "unnamed");
    }

    #[test]
    fn project_id_is_deterministic() {
        let path = PathBuf::from("/tmp/foo");
        let a = ProjectId::new("demo", &path);
        let b = ProjectId::new("demo", &path);
        assert_eq!(a, b);
    }

    #[test]
    fn project_id_changes_with_path() {
        let a = ProjectId::new("demo", &PathBuf::from("/tmp/foo"));
        let b = ProjectId::new("demo", &PathBuf::from("/tmp/bar"));
        assert_ne!(a, b);
    }

    #[test]
    fn project_id_format() {
        let id = ProjectId::new("Lowcow Platform", &PathBuf::from("/tmp/x"));
        assert!(id.as_str().starts_with("lowcow-platform-"));
        // Length: "lowcow-platform-" + 16 hex chars
        assert_eq!(id.as_str().len(), "lowcow-platform-".len() + 16);
    }
}
