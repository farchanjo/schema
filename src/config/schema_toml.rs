//! `schema.toml` definition and loader.
//!
//! The on-disk format is the single contract project consumers depend on. Adding
//! or removing fields is a breaking change for them; do it by bumping
//! `[project] version` and shipping a migration note in CLAUDE.md.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Top-level structure of `schema.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaConfig {
    pub project: ProjectMeta,

    #[serde(default)]
    pub corpus: Vec<Corpus>,

    #[serde(default)]
    pub embedding: EmbeddingConfig,

    #[serde(default)]
    pub retrieval: RetrievalConfig,

    #[serde(default)]
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMeta {
    /// Human-readable project name. Combined with the project's canonical path
    /// hash to form `ProjectId`. Must be unique within the user's machine.
    pub name: String,

    /// Schema version of the `schema.toml` file itself. Used for migration
    /// detection if the format ever evolves.
    #[serde(default = "default_config_version")]
    pub version: String,
}

fn default_config_version() -> String {
    "1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corpus {
    /// Path relative to the project root (where `schema.toml` lives).
    /// Path traversal (`..`) is refused at validation time.
    pub path: PathBuf,

    /// Type of content; drives chunking strategy.
    pub kind: CorpusKind,

    /// Optional glob patterns to exclude from this corpus entry.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// FASE 1.0 supports five kinds. `code` (rs/py/ts via tree-sitter) is FASE 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CorpusKind {
    AdrMadr,
    Markdown,
    Glossary,
    Cue,
    Openapi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Embedding model identifier. FASE 1.0 only supports `"bge-m3"`.
    #[serde(default = "default_embedding_model")]
    pub model: String,
}

fn default_embedding_model() -> String {
    "bge-m3".to_string()
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: default_embedding_model(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalConfig {
    /// Default top-K for retrieval queries when the caller does not specify.
    #[serde(default = "default_top_k")]
    pub top_k_default: usize,

    /// Maximum chunk size in bytes. Chunks above this are split by chunker.
    #[serde(default = "default_chunk_size_max")]
    pub chunk_size_max: usize,

    /// Maximum file size in bytes. Files above this are skipped with a warning.
    #[serde(default = "default_file_size_max")]
    pub file_size_max: u64,
}

fn default_top_k() -> usize {
    8
}
fn default_chunk_size_max() -> usize {
    8_192
}
fn default_file_size_max() -> u64 {
    5_242_880 // 5 MiB
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            top_k_default: default_top_k(),
            chunk_size_max: default_chunk_size_max(),
            file_size_max: default_file_size_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// If `true`, follow symlinks during corpus walking. Default: `false`
    /// (safer; symlinks can escape the project root).
    #[serde(default)]
    pub follow_symlinks: bool,

    /// Globs always excluded from any corpus, regardless of per-entry overrides.
    #[serde(default = "default_exclude_default")]
    pub exclude_default: Vec<String>,
}

fn default_exclude_default() -> Vec<String> {
    vec![
        ".git".into(),
        "node_modules".into(),
        "target".into(),
        "dist".into(),
        "build".into(),
        ".venv".into(),
        "__pycache__".into(),
    ]
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            exclude_default: default_exclude_default(),
        }
    }
}

impl SchemaConfig {
    /// Read `schema.toml` from a path and validate it.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading schema.toml at {}", path.display()))?;
        let cfg: SchemaConfig =
            toml::from_str(&raw).with_context(|| "parsing schema.toml as TOML")?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// Resolve the project root directory — the directory containing `schema.toml`.
    pub fn project_root(config_path: &Path) -> Result<PathBuf> {
        let canonical = config_path
            .canonicalize()
            .with_context(|| format!("canonicalising {}", config_path.display()))?;
        let parent = canonical
            .parent()
            .ok_or_else(|| anyhow::anyhow!("schema.toml has no parent directory"))?;
        Ok(parent.to_path_buf())
    }

    /// Validate the loaded config (path traversal, unknown kinds already caught
    /// by serde, name non-empty, etc.).
    fn validate(&self, config_path: &Path) -> Result<()> {
        if self.project.name.is_empty() {
            bail!("[project] name must not be empty");
        }

        let root = SchemaConfig::project_root(config_path)?;
        for (idx, corpus) in self.corpus.iter().enumerate() {
            // Refuse absolute paths or path traversal.
            if corpus.path.is_absolute() {
                bail!(
                    "corpus[{idx}].path must be relative to project root (got absolute: {})",
                    corpus.path.display()
                );
            }
            if corpus
                .path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!(
                    "corpus[{idx}].path must not contain '..' (got {})",
                    corpus.path.display()
                );
            }
            // Sanity check: path resolves within root.
            let resolved = root.join(&corpus.path);
            if let Ok(canon) = resolved.canonicalize()
                && !canon.starts_with(&root)
            {
                bail!(
                    "corpus[{idx}].path escapes project root (resolves to {})",
                    canon.display()
                );
            }
        }

        if self.embedding.model != "bge-m3" {
            bail!(
                "[embedding] model = {:?} is not supported in FASE 1.0 (only \"bge-m3\")",
                self.embedding.model
            );
        }

        Ok(())
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
    fn parses_minimal_config() {
        let toml_str = r#"
[project]
name = "demo"
version = "1"

[[corpus]]
path = "docs/decisions"
kind = "adr-madr"
"#;
        let cfg: SchemaConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.project.name, "demo");
        assert_eq!(cfg.corpus.len(), 1);
        assert_eq!(cfg.corpus[0].kind, CorpusKind::AdrMadr);
        assert_eq!(cfg.embedding.model, "bge-m3");
        assert_eq!(cfg.retrieval.top_k_default, 8);
    }

    #[test]
    fn applies_defaults() {
        let toml_str = r#"
[project]
name = "x"
"#;
        let cfg: SchemaConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.project.version, "1");
        assert_eq!(cfg.embedding.model, "bge-m3");
        assert!(!cfg.security.follow_symlinks);
        assert!(cfg.security.exclude_default.contains(&".git".to_string()));
    }
}
