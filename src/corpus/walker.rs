//! Filesystem walker for corpus entries.
//!
//! Honours per-corpus `exclude` globs and the global `security.exclude_default`
//! list. Refuses to descend into symlinks unless `security.follow_symlinks` is
//! `true`. Skips files larger than `retrieval.file_size_max` with a `warn` log.

use std::path::{Path, PathBuf};

use thiserror::Error;
use walkdir::WalkDir;

use crate::config::{Corpus, CorpusKind, SchemaConfig};

#[derive(Debug, Error)]
pub enum WalkerError {
    #[error("corpus path {0} does not exist")]
    PathNotFound(PathBuf),
    #[error("io error walking {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A file discovered by the walker, paired with the kind from its corpus entry.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub absolute_path: PathBuf,
    /// Path relative to the project root.
    pub relative_path: PathBuf,
    pub kind: CorpusKind,
    pub size_bytes: u64,
}

/// Walks every corpus entry in a [`SchemaConfig`] under a given project root.
pub struct Walker<'a> {
    config: &'a SchemaConfig,
    project_root: &'a Path,
}

impl<'a> Walker<'a> {
    pub fn new(config: &'a SchemaConfig, project_root: &'a Path) -> Self {
        Self {
            config,
            project_root,
        }
    }

    /// Visit every file matching every corpus entry. Returns a flat list.
    pub fn discover(&self) -> Result<Vec<DiscoveredFile>, WalkerError> {
        let mut files = Vec::new();
        for corpus in &self.config.corpus {
            self.discover_one(corpus, &mut files)?;
        }
        Ok(files)
    }

    fn discover_one(
        &self,
        corpus: &Corpus,
        files: &mut Vec<DiscoveredFile>,
    ) -> Result<(), WalkerError> {
        let absolute = self.project_root.join(&corpus.path);
        if !absolute.exists() {
            return Err(WalkerError::PathNotFound(absolute));
        }

        // Single file → one entry.
        if absolute.is_file() {
            if let Some(file) = self.consider_file(&absolute, corpus.kind)? {
                files.push(file);
            }
            return Ok(());
        }

        let walker = WalkDir::new(&absolute)
            .follow_links(self.config.security.follow_symlinks)
            .into_iter()
            .filter_entry(|e| !self.is_globally_excluded(e.path()));

        for entry in walker {
            let entry = entry.map_err(|e| WalkerError::Io {
                path: absolute.clone(),
                source: e.into(),
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            if self.matches_per_corpus_exclude(entry.path(), corpus) {
                continue;
            }
            if let Some(file) = self.consider_file(entry.path(), corpus.kind)? {
                files.push(file);
            }
        }
        Ok(())
    }

    fn consider_file(
        &self,
        absolute: &Path,
        kind: CorpusKind,
    ) -> Result<Option<DiscoveredFile>, WalkerError> {
        let metadata = std::fs::metadata(absolute).map_err(|e| WalkerError::Io {
            path: absolute.to_path_buf(),
            source: e,
        })?;

        let size_bytes = metadata.len();
        if size_bytes > self.config.retrieval.file_size_max {
            tracing::warn!(
                path = %absolute.display(),
                size_bytes,
                limit = self.config.retrieval.file_size_max,
                "file exceeds file_size_max; skipping",
            );
            return Ok(None);
        }

        let relative_path = absolute
            .strip_prefix(self.project_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| absolute.to_path_buf());

        Ok(Some(DiscoveredFile {
            absolute_path: absolute.to_path_buf(),
            relative_path,
            kind,
            size_bytes,
        }))
    }

    fn is_globally_excluded(&self, path: &Path) -> bool {
        let last_component = path.file_name().and_then(|s| s.to_str());
        if let Some(name) = last_component {
            for excluded in &self.config.security.exclude_default {
                if name == excluded {
                    return true;
                }
            }
        }
        false
    }

    fn matches_per_corpus_exclude(&self, path: &Path, corpus: &Corpus) -> bool {
        // Simple substring match for FASE 1.0; full glob support arrives later.
        for pattern in &corpus.exclude {
            if let Some(name) = path.file_name().and_then(|s| s.to_str())
                && name.contains(pattern.trim_start_matches('*').trim_end_matches('*'))
            {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(p: &Path, contents: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    #[test]
    fn discovers_single_corpus_entry() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("docs/decisions/0001-test.md"), "# ADR-0001");
        write(&root.join("docs/decisions/0002-test.md"), "# ADR-0002");
        write(&root.join("docs/decisions/template.md"), "# template");

        let cfg = SchemaConfig {
            project: crate::config::ProjectMeta {
                name: "demo".into(),
                version: "1".into(),
            },
            corpus: vec![Corpus {
                path: PathBuf::from("docs/decisions"),
                kind: CorpusKind::AdrMadr,
                exclude: vec!["template".into()],
            }],
            embedding: Default::default(),
            retrieval: Default::default(),
            security: Default::default(),
        };

        let walker = Walker::new(&cfg, root);
        let files = walker.discover().unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.kind == CorpusKind::AdrMadr));
        assert!(
            !files
                .iter()
                .any(|f| f.relative_path.to_string_lossy().contains("template"))
        );
    }

    #[test]
    fn skips_files_above_size_limit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("docs/big.md"), &"x".repeat(2_000));

        let mut cfg = SchemaConfig {
            project: crate::config::ProjectMeta {
                name: "demo".into(),
                version: "1".into(),
            },
            corpus: vec![Corpus {
                path: PathBuf::from("docs"),
                kind: CorpusKind::Markdown,
                exclude: vec![],
            }],
            embedding: Default::default(),
            retrieval: Default::default(),
            security: Default::default(),
        };
        cfg.retrieval.file_size_max = 1_000;

        let files = Walker::new(&cfg, root).discover().unwrap();
        assert!(files.is_empty(), "big file should be skipped");
    }

    #[test]
    fn excludes_default_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("docs/a.md"), "ok");
        write(&root.join("docs/node_modules/bad.md"), "skip");

        let cfg = SchemaConfig {
            project: crate::config::ProjectMeta {
                name: "demo".into(),
                version: "1".into(),
            },
            corpus: vec![Corpus {
                path: PathBuf::from("docs"),
                kind: CorpusKind::Markdown,
                exclude: vec![],
            }],
            embedding: Default::default(),
            retrieval: Default::default(),
            security: Default::default(),
        };

        let files = Walker::new(&cfg, root).discover().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].relative_path.ends_with("a.md"));
    }
}
