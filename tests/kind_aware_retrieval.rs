//! Integration tests for ADR-0028 — `kind`-partition retrieval and
//! score-floor filtering against a real `sqlite-vec` store.
//!
//! Fixture-only tests; no embedder, no MCP server, no daemon. Each test
//! seeds a deterministic corpus into a temp `store.db` and asserts the
//! adapter routes the SQL through the partition key + distance ceiling
//! per the ADR's fitness function.

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
    use schema::adapters::sqlite_vec_store::SqliteVecStore;
    use schema::domain::{Chunk, CorpusKind};
    use schema::ports::Persistence;
    use tempfile::TempDir;

    const DIM: usize = 1024;

    fn unit_vec(slot: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; DIM];
        v[slot % DIM] = 1.0;
        v
    }

    fn make_chunk(path: &str, kind: CorpusKind, content: &str) -> Chunk {
        Chunk {
            source_path: path.to_string(),
            line_start: 1,
            line_end: 5,
            artifact_id: None,
            title: None,
            content: content.to_string(),
            kind,
        }
    }

    fn make_adr_chunk(slot: usize) -> Chunk {
        Chunk {
            source_path: format!("docs/adr/{slot:04}-fixture.md"),
            line_start: 1,
            line_end: 5,
            artifact_id: Some(format!("ADR-{slot:04}")),
            title: Some(format!("ADR {slot:04}")),
            content: format!("ADR fixture {slot}"),
            kind: CorpusKind::AdrMadr,
        }
    }

    async fn open_fresh_store() -> (TempDir, SqliteVecStore) {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();
        (tmp, store)
    }

    use std::ops::Range;

    fn build_skewed_corpus(
        markdown_count: usize,
        glossary_slots: Range<usize>,
    ) -> (Vec<Chunk>, Vec<Vec<f32>>) {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        for i in 0..markdown_count {
            chunks.push(make_chunk(
                &format!("md-{i}.md"),
                CorpusKind::Markdown,
                &format!("markdown {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        for i in glossary_slots {
            chunks.push(make_chunk(
                &format!("g-{i}.md"),
                CorpusKind::Glossary,
                &format!("glossary {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        (chunks, vectors)
    }

    /// ADR-0028 fitness #1 — kind-skewed corpus.
    ///
    /// Without the partition key the engine's top-K could return only
    /// `Markdown` rows; with the partition pushed inside MATCH the
    /// top-K is computed *within* the requested kind.
    #[tokio::test]
    async fn glossary_lookup_returns_glossary_only_under_kind_skew() {
        let (_tmp, store) = open_fresh_store().await;
        let (chunks, vectors) = build_skewed_corpus(80, 80..100);
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let q = unit_vec(95);
        let hits = store
            .query_nearest(&q, 8, Some("Glossary"), None)
            .await
            .unwrap();

        assert!(!hits.is_empty(), "kind=Glossary must produce hits");
        assert!(
            hits.iter().all(|h| h.kind == "Glossary"),
            "every hit must carry the requested kind",
        );
        assert_eq!(
            hits[0].source_path, "g-95.md",
            "top-1 must be the seeded golden glossary chunk",
        );
    }

    fn build_extreme_skew_corpus() -> (Vec<Chunk>, Vec<Vec<f32>>) {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        for i in 0..95 {
            chunks.push(make_chunk(
                &format!("noise-{i}.md"),
                CorpusKind::Markdown,
                &format!("noise {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        for i in 90..95 {
            chunks.push(make_adr_chunk(i));
            vectors.push(unit_vec(i));
        }
        (chunks, vectors)
    }

    /// ADR-0028 fitness #2 — small target kind survives extreme skew.
    #[tokio::test]
    async fn find_decisions_returns_seeded_adr_under_extreme_skew() {
        let (_tmp, store) = open_fresh_store().await;
        let (chunks, vectors) = build_extreme_skew_corpus();
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let q = unit_vec(92);
        let hits = store
            .query_nearest(&q, 8, Some("AdrMadr"), None)
            .await
            .unwrap();

        assert!(!hits.is_empty(), "AdrMadr must produce hits");
        assert!(
            hits.iter().all(|h| h.kind == "AdrMadr"),
            "every hit must be AdrMadr",
        );
        assert!(
            hits.iter()
                .any(|h| h.source_path == "docs/adr/0092-fixture.md"),
            "expected golden ADR-0092 fixture in hits",
        );
    }

    fn build_mixed_corpus() -> (Vec<Chunk>, Vec<Vec<f32>>) {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        for i in 0..20 {
            chunks.push(make_chunk(
                &format!("md-{i}.md"),
                CorpusKind::Markdown,
                &format!("md {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        for i in 20..40 {
            chunks.push(make_chunk(
                &format!("g-{i}.md"),
                CorpusKind::Glossary,
                &format!("g {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        (chunks, vectors)
    }

    /// ADR-0028 fitness #3 — cross-kind path unchanged.
    #[tokio::test]
    async fn cross_kind_query_still_returns_global_top_k() {
        let (_tmp, store) = open_fresh_store().await;
        let (chunks, vectors) = build_mixed_corpus();
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let q = unit_vec(25);
        let hits = store.query_nearest(&q, 8, None, None).await.unwrap();

        assert_eq!(hits.len(), 8, "cross-kind must still return top-K");
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
        assert!(
            kinds.contains(&"Glossary"),
            "top-8 across kinds must include the glossary chunk closest \
             to the query, got {kinds:?}",
        );
    }

    /// ADR-0028 fitness #4 — score floor rejects orthogonal hits.
    #[tokio::test]
    async fn score_floor_rejects_orthogonal_query() {
        let (_tmp, store) = open_fresh_store().await;

        let mut chunks: Vec<Chunk> = Vec::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        for i in 0..16 {
            chunks.push(make_chunk(
                &format!("md-{i}.md"),
                CorpusKind::Markdown,
                &format!("md {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        store.append_chunks(&chunks, &vectors).await.unwrap();

        // Slot 999 is orthogonal to every seeded vector.
        let q = unit_vec(999);

        let hits_no_floor = store.query_nearest(&q, 8, None, None).await.unwrap();
        assert_eq!(hits_no_floor.len(), 8, "raw top-K should fill");

        let hits_floor = store.query_nearest(&q, 8, None, Some(0.95)).await.unwrap();
        assert!(
            hits_floor.is_empty(),
            "score floor 0.95 must reject orthogonal hits, got {} rows",
            hits_floor.len(),
        );
    }

    /// ADR-0028 fitness #5 — score floor admits perfectly-aligned hits.
    #[tokio::test]
    async fn score_floor_admits_perfectly_aligned_hit() {
        let (_tmp, store) = open_fresh_store().await;

        let mut chunks: Vec<Chunk> = Vec::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        for i in 0..8 {
            chunks.push(make_chunk(
                &format!("md-{i}.md"),
                CorpusKind::Markdown,
                &format!("md {i}"),
            ));
            vectors.push(unit_vec(i));
        }
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let q = unit_vec(3);
        let hits = store.query_nearest(&q, 8, None, Some(0.5)).await.unwrap();
        assert_eq!(
            hits.len(),
            1,
            "only the perfectly-aligned hit (slot 3) clears the floor",
        );
        assert_eq!(hits[0].source_path, "md-3.md");
    }
}
