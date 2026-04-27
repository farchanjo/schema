//! `MetadataStore` adapter — TOML serialiser for the [`Metadata`] manifest.
//!
//! Splits the I/O off [`Metadata`] (which now stays pure in `domain.rs`) so
//! tests can swap in an in-memory store without dragging the filesystem in.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::domain::Metadata;
use crate::ports::{MetadataStore, MetadataStoreError};

/// Reads/writes `metadata.toml` at a fixed path on disk.
#[derive(Debug, Clone)]
pub struct TomlMetadataStore {
    path: PathBuf,
}

impl TomlMetadataStore {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl MetadataStore for TomlMetadataStore {
    fn load(&self) -> Result<Metadata, MetadataStoreError> {
        if !self.path.exists() {
            return Ok(Metadata::default());
        }
        let raw = fs::read_to_string(&self.path)?;
        toml::from_str(&raw).map_err(|e| MetadataStoreError::Parse(e.to_string()))
    }

    fn save(&self, metadata: &Metadata) -> Result<(), MetadataStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(metadata)
            .map_err(|e| MetadataStoreError::Serialise(e.to_string()))?;
        fs::write(&self.path, raw)?;
        Ok(())
    }

    fn reset(&self) -> Result<(), MetadataStoreError> {
        self.save(&Metadata::default())
    }
}

/// Compute the BLAKE3 hex digest of a file's content.
///
/// # Errors
/// Returns an error if the file cannot be read.
pub fn file_content_hash(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    let hash = blake3::hash(&bytes);
    Ok(hash.to_hex().to_string())
}

/// Read mtime as Unix timestamp seconds.
///
/// # Errors
/// Returns an error if the file's metadata cannot be read.
pub fn file_mtime(path: &Path) -> io::Result<i64> {
    let meta = fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    Ok(mtime)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]
    use super::*;
    use crate::domain::FileMeta;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("metadata.toml");
        let store = TomlMetadataStore::new(path);

        let mut meta = Metadata::default();
        meta.upsert(
            &PathBuf::from("docs/decisions/0001.md"),
            FileMeta {
                mtime: 100,
                size_bytes: 4096,
                content_hash: "deadbeef".into(),
                chunk_count: 3,
            },
        );
        store.save(&meta).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.files.len(), 1);
        let f = loaded.get(Path::new("docs/decisions/0001.md")).unwrap();
        assert_eq!(f.chunk_count, 3);
    }

    #[test]
    fn returns_default_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store = TomlMetadataStore::new(tmp.path().join("nope.toml"));
        let meta = store.load().unwrap();
        assert!(meta.files.is_empty());
    }

    /// Build a [`FileMeta`] with the given fields.
    fn meta(mtime: i64, size: u64, hash: &str, chunks: usize) -> FileMeta {
        FileMeta {
            mtime,
            size_bytes: size,
            content_hash: hash.into(),
            chunk_count: chunks,
        }
    }

    /// ADR-0015 fitness function — `reset` overwrites the manifest with
    /// `Metadata::default()`.
    #[test]
    fn reset_writes_default_manifest() {
        let tmp = TempDir::new().unwrap();
        let store = TomlMetadataStore::new(tmp.path().join("metadata.toml"));

        let mut seeded = Metadata::default();
        seeded.upsert(&PathBuf::from("a.md"), meta(1, 10, "aaa", 1));
        seeded.upsert(&PathBuf::from("b.md"), meta(2, 20, "bbb", 2));
        store.save(&seeded).unwrap();
        assert_eq!(store.load().unwrap().files.len(), 2);

        store.reset().unwrap();

        let after = store.load().unwrap();
        assert!(after.files.is_empty(), "reset must drop every file entry");
        assert_eq!(after.version, 1, "default version must be 1");
    }
}
