//! Persisted metadata for delta-sync.
//!
//! `metadata.toml` lives at `<project-cache-dir>/metadata.toml` and records,
//! per-file, the modification time and BLAKE3 content hash from the most
//! recent indexing. A startup pass compares each declared corpus file against
//! these records and emits "create / update / delete" instructions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Single-file record. Stored under the file's relative-to-project path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: i64,
    pub size_bytes: u64,
    pub content_hash: String,
    /// Number of chunks produced last time we indexed this file. Drives
    /// pruning when count drops on a re-index.
    pub chunk_count: usize,
}

/// On-disk shape of `metadata.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// Schema version of the metadata file itself.
    #[serde(default = "default_meta_version")]
    pub version: u32,

    /// Map: relative-path-as-string → file metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

fn default_meta_version() -> u32 {
    1
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            version: default_meta_version(),
            files: BTreeMap::new(),
        }
    }
}

impl Metadata {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let meta: Metadata =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(meta)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self).context("serialising metadata.toml")?;
        std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn upsert(&mut self, relative_path: PathBuf, meta: FileMeta) {
        self.files
            .insert(relative_path.to_string_lossy().to_string(), meta);
    }

    pub fn remove(&mut self, relative_path: &Path) -> Option<FileMeta> {
        self.files
            .remove(&relative_path.to_string_lossy().to_string())
    }

    pub fn get(&self, relative_path: &Path) -> Option<&FileMeta> {
        self.files.get(&relative_path.to_string_lossy().to_string())
    }
}

/// Compute the BLAKE3 hex digest of a file's content.
pub fn file_content_hash(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let hash = blake3::hash(&bytes);
    Ok(hash.to_hex().to_string())
}

/// Read mtime as Unix timestamp seconds.
pub fn file_mtime(path: &Path) -> std::io::Result<i64> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(mtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("metadata.toml");

        let mut meta = Metadata::default();
        meta.upsert(
            PathBuf::from("docs/decisions/0001.md"),
            FileMeta {
                mtime: 100,
                size_bytes: 4096,
                content_hash: "deadbeef".into(),
                chunk_count: 3,
            },
        );
        meta.save(&path).unwrap();

        let loaded = Metadata::load_or_default(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.files.len(), 1);
        let f = loaded.get(Path::new("docs/decisions/0001.md")).unwrap();
        assert_eq!(f.chunk_count, 3);
    }

    #[test]
    fn returns_default_when_missing() {
        let tmp = TempDir::new().unwrap();
        let meta = Metadata::load_or_default(&tmp.path().join("nope.toml")).unwrap();
        assert!(meta.files.is_empty());
    }
}
