//! Query orchestration — the read-side of the application.
//!
//! Wraps the [`crate::ports::Persistence`] + [`crate::ports::Embedder`] ports
//! in a thin service that powers the MCP tools (`query`, `find_decisions`,
//! `glossary_lookup`, `cross_reference`, `list_corpus`). The driving adapter
//! (`crate::adapters::mcp_server`) consumes this; the service itself does no
//! protocol-specific serialisation.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::domain::ChunkRecord;
use crate::ports::Persistence;
use schema_core::embedder::Embedder;

/// Read-side service. Cheap to clone; holds `Arc`s.
///
/// `query_passage_prefix` (ADR-0029) and `min_score` (ADR-0028) are
/// captured at wire time from the project's `schema.toml` and threaded
/// into every `embed_query` / `query_nearest` call so the daemon's
/// shared embedder + per-project store honour the project's recipe.
#[derive(Clone)]
#[expect(
    clippy::struct_field_names,
    reason = "field name `query_passage_prefix` mirrors the public \
              schema.toml `[embedding] query_passage_prefix` knob; \
              renaming would diverge from the user-facing config key \
              for no semantic gain — the prefix collision with the \
              struct name is incidental"
)]
pub struct Query {
    persistence: Arc<dyn Persistence>,
    embedder: Arc<Mutex<dyn Embedder>>,
    query_passage_prefix: bool,
    min_score: Option<f32>,
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("persistence", &"<dyn Persistence>")
            .field("embedder", &"<Mutex<dyn Embedder>>")
            .field("query_passage_prefix", &self.query_passage_prefix)
            .field("min_score", &self.min_score)
            .finish()
    }
}

/// Two-sided result returned by [`Query::cross_reference`].
#[derive(Debug, Clone)]
pub struct CrossReference {
    pub definition: Vec<ChunkRecord>,
    pub references: Vec<ChunkRecord>,
}

impl Query {
    #[must_use]
    pub fn new(
        persistence: Arc<dyn Persistence>,
        embedder: Arc<Mutex<dyn Embedder>>,
        query_passage_prefix: bool,
        min_score: Option<f32>,
    ) -> Self {
        Self {
            persistence,
            embedder,
            query_passage_prefix,
            min_score,
        }
    }

    /// Run a top-K nearest-neighbour query, optionally filtered by `kind`.
    ///
    /// # Errors
    /// Returns an error if embedding or persistence fails.
    pub async fn run(
        &self,
        query_text: &str,
        top_k: usize,
        kind_filter: Option<&str>,
    ) -> Result<Vec<ChunkRecord>> {
        let vector = {
            let mut emb = self.embedder.lock().await;
            let v = emb
                .embed_query(query_text.to_string(), self.query_passage_prefix)
                .await?;
            drop(emb);
            v
        };
        let records = self
            .persistence
            .query_nearest(&vector, top_k, kind_filter, self.min_score)
            .await?;
        Ok(records)
    }

    /// Cross-reference an artifact id: definitional chunks (exact `artifact_id`
    /// match) plus referencing chunks (content mentions, excluding the
    /// artifact's own).
    ///
    /// # Errors
    /// Returns an error if either persistence query fails.
    pub async fn cross_reference(
        &self,
        artifact_id: &str,
        definition_limit: usize,
        reference_limit: usize,
    ) -> Result<CrossReference> {
        let definition = self
            .persistence
            .find_by_artifact_id(artifact_id, definition_limit)
            .await?;
        let references = self
            .persistence
            .find_mentioning(artifact_id, reference_limit)
            .await?;
        Ok(CrossReference {
            definition,
            references,
        })
    }

    /// List every distinct `source_path` indexed for this project.
    ///
    /// # Errors
    /// Returns an error if the persistence scan fails.
    pub async fn list_source_paths(&self) -> Result<Vec<String>> {
        let paths = self.persistence.list_source_paths().await?;
        Ok(paths)
    }
}
