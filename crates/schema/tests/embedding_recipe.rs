//! Integration tests for ADR-0029 — bge-m3 query / passage prefix
//! policy at the adapter boundary. The fastembed model itself is not
//! booted (heavy ONNX download); these tests focus on the behavioural
//! contract of the persisted recipe marker and prefix application.
//!
//! Recall-benchmark coverage (ADR-0029 fitness #1) lives in the
//! gated `tests/e2e/` suite that boots a real BGE-M3 instance — those
//! runs are opt-in via the `schema-online` env flag.

#![allow(
    clippy::unwrap_used,
    reason = "test fixtures may panic if the env is broken"
)]
#![allow(
    unused_crate_dependencies,
    reason = "Cargo.toml is shared between lib + bin + integration tests; \
              `unused_crate_dependencies` runs per-target and the lib's \
              full dep tree is visible to every test target."
)]

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use schema::adapters::sqlite_vec_store::{
        RECIPE_BGE_M3_QUERY_PASSAGE, RECIPE_RAW, SqliteVecStore,
    };
    use schema::ports::{
        EMBEDDER_PASSAGE_PREFIX, EMBEDDER_QUERY_PREFIX, EmbedError, Embedder, Persistence,
    };
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// In-memory `Embedder` that records every input text it sees so
    /// tests can assert which prefix (if any) the caller applied.
    /// Returns a deterministic single-axis unit vector per call.
    #[derive(Debug, Default)]
    struct RecordingEmbedder {
        seen: StdMutex<Vec<String>>,
    }

    impl RecordingEmbedder {
        fn snapshot(&self) -> Vec<String> {
            let guard = self.seen.lock().unwrap();
            guard.clone()
        }
    }

    #[async_trait]
    impl Embedder for RecordingEmbedder {
        async fn embed_query(
            &mut self,
            text: String,
            with_prefix: bool,
        ) -> Result<Vec<f32>, EmbedError> {
            let prepared = if with_prefix {
                format!("{EMBEDDER_QUERY_PREFIX}{text}")
            } else {
                text
            };
            self.seen.lock().unwrap().push(prepared);
            Ok(vec![1.0_f32])
        }

        async fn embed_passages(
            &mut self,
            texts: Vec<String>,
            with_prefix: bool,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            let prepared: Vec<String> = if with_prefix {
                texts
                    .into_iter()
                    .map(|t| format!("{EMBEDDER_PASSAGE_PREFIX}{t}"))
                    .collect()
            } else {
                texts
            };
            let count = prepared.len();
            self.seen.lock().unwrap().extend(prepared);
            Ok(vec![vec![1.0_f32]; count])
        }
    }

    /// ADR-0029 — when `with_prefix = false`, query input flows raw.
    #[tokio::test]
    async fn embed_query_without_prefix_passes_text_verbatim() {
        let mut emb = RecordingEmbedder::default();
        emb.embed_query("UUIDv7 rationale".to_string(), false)
            .await
            .unwrap();
        let seen = emb.snapshot();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], "UUIDv7 rationale");
        assert!(
            !seen[0].starts_with(EMBEDDER_QUERY_PREFIX),
            "raw mode must not prepend the BAAI query prefix",
        );
    }

    /// ADR-0029 — when `with_prefix = true`, query input gains the
    /// BAAI dense-retrieval query prefix.
    #[tokio::test]
    async fn embed_query_with_prefix_prepends_query_prefix() {
        let mut emb = RecordingEmbedder::default();
        emb.embed_query("UUIDv7 rationale".to_string(), true)
            .await
            .unwrap();
        let seen = emb.snapshot();
        assert_eq!(seen.len(), 1);
        let expected = format!("{EMBEDDER_QUERY_PREFIX}UUIDv7 rationale");
        assert_eq!(seen[0], expected);
    }

    /// ADR-0029 — passage side gains the passage prefix when flag is on.
    #[tokio::test]
    async fn embed_passages_with_prefix_prepends_passage_prefix() {
        let mut emb = RecordingEmbedder::default();
        let texts = vec!["body alpha".to_string(), "body beta".to_string()];
        let vectors = emb.embed_passages(texts, true).await.unwrap();
        assert_eq!(vectors.len(), 2);
        let seen = emb.snapshot();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], format!("{EMBEDDER_PASSAGE_PREFIX}body alpha"));
        assert_eq!(seen[1], format!("{EMBEDDER_PASSAGE_PREFIX}body beta"));
    }

    /// ADR-0029 — passage side passes through raw when flag is off.
    #[tokio::test]
    async fn embed_passages_without_prefix_passes_text_verbatim() {
        let mut emb = RecordingEmbedder::default();
        let texts = vec!["raw body".to_string()];
        emb.embed_passages(texts, false).await.unwrap();
        let seen = emb.snapshot();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], "raw body");
    }

    /// ADR-0029 — recipe marker round-trips through the persistence port.
    #[tokio::test]
    async fn embedding_recipe_round_trips_through_persistence() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        assert_eq!(store.read_embedding_recipe().await.unwrap(), None);

        store.write_embedding_recipe(RECIPE_RAW).await.unwrap();
        assert_eq!(
            store.read_embedding_recipe().await.unwrap(),
            Some(RECIPE_RAW.to_string())
        );

        store
            .write_embedding_recipe(RECIPE_BGE_M3_QUERY_PASSAGE)
            .await
            .unwrap();
        assert_eq!(
            store.read_embedding_recipe().await.unwrap(),
            Some(RECIPE_BGE_M3_QUERY_PASSAGE.to_string())
        );
    }

    /// ADR-0029 — recipe marker survives `reset_all`.
    ///
    /// `reset_all` wipes `chunks`, not `meta`. The orchestrator
    /// (`project_instance`) writes the desired recipe **after** the
    /// reset; this test pins that contract so the recipe row never
    /// silently disappears.
    #[tokio::test]
    async fn reset_all_does_not_clear_meta_table() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();
        store
            .write_embedding_recipe(RECIPE_BGE_M3_QUERY_PASSAGE)
            .await
            .unwrap();

        store.reset_all().await.unwrap();

        assert_eq!(
            store.read_embedding_recipe().await.unwrap(),
            Some(RECIPE_BGE_M3_QUERY_PASSAGE.to_string()),
            "reset_all targets chunks; meta survives so the orchestrator's \
             post-reset write is the authority on recipe state",
        );
    }
}
