//! LanceDB-backed vector store for embedded chunks.
//!
//! Each project owns one LanceDB instance under
//! `~/.cache/schema/projects/<id>/lance/`. A single table named `chunks`
//! holds every embedded chunk.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int32Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::Connection;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

use crate::corpus::Chunk;
use crate::embeddings::BGE_M3_DIMENSIONS;

const TABLE_NAME: &str = "chunks";

#[derive(Debug, Error)]
pub enum VectorStoreError {
    #[error("lancedb error: {0}")]
    Lance(#[from] lancedb::Error),
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// A row materialised from the LanceDB store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub id: String,
    pub source_path: String,
    pub line_start: i32,
    pub line_end: i32,
    pub artifact_id: Option<String>,
    pub title: Option<String>,
    pub kind: String,
    pub content: String,
    /// Distance returned by the nearest-neighbour query (lower = closer).
    pub score: Option<f32>,
}

pub struct VectorStore {
    conn: Connection,
}

impl std::fmt::Debug for VectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorStore")
            .field("conn", &"<lancedb::Connection>")
            .finish()
    }
}

impl VectorStore {
    /// Open (or create) the LanceDB at the given path.
    pub async fn open(lance_dir: &Path) -> Result<Self, VectorStoreError> {
        std::fs::create_dir_all(lance_dir)?;
        let uri = lance_dir.to_string_lossy().to_string();
        info!(uri, "opening LanceDB");
        let conn = lancedb::connect(&uri).execute().await?;
        Ok(Self { conn })
    }

    /// Ensure the `chunks` table exists. Idempotent.
    pub async fn ensure_table(&self) -> Result<(), VectorStoreError> {
        let names = self.conn.table_names().execute().await?;
        if names.contains(&TABLE_NAME.to_string()) {
            return Ok(());
        }
        let schema = chunks_schema();
        let empty: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(std::iter::empty(), schema.clone()));
        self.conn.create_table(TABLE_NAME, empty).execute().await?;
        info!(table = TABLE_NAME, "lance table created");
        Ok(())
    }

    /// Append rows for a batch of chunks paired with their embeddings.
    pub async fn append_chunks(
        &self,
        chunks: &[Chunk],
        vectors: &[Vec<f32>],
    ) -> Result<(), VectorStoreError> {
        if chunks.is_empty() {
            return Ok(());
        }
        if chunks.len() != vectors.len() {
            return Err(VectorStoreError::Other(anyhow::anyhow!(
                "chunks ({}) and vectors ({}) length mismatch",
                chunks.len(),
                vectors.len()
            )));
        }

        let schema = chunks_schema();
        let batch = build_batch(chunks, vectors, schema.clone())?;
        let table = self.conn.open_table(TABLE_NAME).execute().await?;
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(std::iter::once(Ok(batch)), schema));
        table.add(reader).execute().await?;
        Ok(())
    }

    /// Delete every chunk whose `source_path` matches one of `paths`.
    pub async fn delete_by_source(&self, paths: &[&str]) -> Result<(), VectorStoreError> {
        if paths.is_empty() {
            return Ok(());
        }
        let table = self.conn.open_table(TABLE_NAME).execute().await?;
        let in_clause: String = paths
            .iter()
            .map(|p| format!("'{}'", p.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let predicate = format!("source_path IN ({in_clause})");
        table.delete(&predicate).await?;
        Ok(())
    }

    /// Top-K nearest-neighbour query. Optionally filtered by `kind`.
    pub async fn query_nearest(
        &self,
        vector: &[f32],
        k: usize,
        kind_filter: Option<&str>,
    ) -> Result<Vec<ChunkRecord>, VectorStoreError> {
        let table = self.conn.open_table(TABLE_NAME).execute().await?;
        let mut q = table.vector_search(vector)?.limit(k);
        if let Some(kind) = kind_filter {
            q = q.only_if(format!("kind = '{}'", kind.replace('\'', "''")));
        }
        let mut stream = q.execute().await?;

        let mut out = Vec::new();
        while let Some(batch) = stream.try_next().await? {
            out.extend(batch_to_records(&batch)?);
        }
        Ok(out)
    }

    /// List every distinct `source_path` currently in the index.
    pub async fn list_source_paths(&self) -> Result<Vec<String>, VectorStoreError> {
        let table = self.conn.open_table(TABLE_NAME).execute().await?;
        let mut stream = table
            .query()
            .select(lancedb::query::Select::columns(&["source_path"]))
            .execute()
            .await?;
        let mut paths = std::collections::BTreeSet::new();
        while let Some(batch) = stream.try_next().await? {
            if let Some(arr) = batch
                .column_by_name("source_path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            {
                for i in 0..arr.len() {
                    if !arr.is_null(i) {
                        paths.insert(arr.value(i).to_string());
                    }
                }
            }
        }
        Ok(paths.into_iter().collect())
    }
}

/// The canonical Arrow schema for the `chunks` table.
pub fn chunks_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("source_path", DataType::Utf8, false),
        Field::new("line_start", DataType::Int32, false),
        Field::new("line_end", DataType::Int32, false),
        Field::new("artifact_id", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
        Field::new("kind", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                BGE_M3_DIMENSIONS as i32,
            ),
            false,
        ),
    ]))
}

fn build_batch(
    chunks: &[Chunk],
    vectors: &[Vec<f32>],
    schema: Arc<Schema>,
) -> Result<RecordBatch, VectorStoreError> {
    let ids: Vec<String> = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}#L{}-L{}#{}", c.source_path, c.line_start, c.line_end, i))
        .collect();
    let kinds: Vec<String> = chunks.iter().map(|c| format!("{:?}", c.kind)).collect();

    let id_arr = StringArray::from(ids);
    let path_arr = StringArray::from(
        chunks
            .iter()
            .map(|c| c.source_path.clone())
            .collect::<Vec<_>>(),
    );
    let line_start_arr = Int32Array::from(
        chunks
            .iter()
            .map(|c| c.line_start as i32)
            .collect::<Vec<_>>(),
    );
    let line_end_arr =
        Int32Array::from(chunks.iter().map(|c| c.line_end as i32).collect::<Vec<_>>());
    let artifact_arr = StringArray::from(
        chunks
            .iter()
            .map(|c| c.artifact_id.clone())
            .collect::<Vec<Option<String>>>(),
    );
    let title_arr = StringArray::from(
        chunks
            .iter()
            .map(|c| c.title.clone())
            .collect::<Vec<Option<String>>>(),
    );
    let kind_arr = StringArray::from(kinds);
    let content_arr =
        StringArray::from(chunks.iter().map(|c| c.content.clone()).collect::<Vec<_>>());

    // Build the FixedSizeList<Float32, 1024> column.
    let vector_arr = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors
            .iter()
            .map(|v| Some(v.iter().map(|f| Some(*f)).collect::<Vec<_>>())),
        BGE_M3_DIMENSIONS as i32,
    );

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(id_arr),
            Arc::new(path_arr),
            Arc::new(line_start_arr),
            Arc::new(line_end_arr),
            Arc::new(artifact_arr),
            Arc::new(title_arr),
            Arc::new(kind_arr),
            Arc::new(content_arr),
            Arc::new(vector_arr),
        ],
    )?;
    Ok(batch)
}

fn batch_to_records(batch: &RecordBatch) -> Result<Vec<ChunkRecord>, VectorStoreError> {
    let id = batch
        .column_by_name("id")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| VectorStoreError::Other(anyhow::anyhow!("id column missing")))?;
    let source_path = batch
        .column_by_name("source_path")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| VectorStoreError::Other(anyhow::anyhow!("source_path column missing")))?;
    let line_start = batch
        .column_by_name("line_start")
        .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
        .ok_or_else(|| VectorStoreError::Other(anyhow::anyhow!("line_start column missing")))?;
    let line_end = batch
        .column_by_name("line_end")
        .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
        .ok_or_else(|| VectorStoreError::Other(anyhow::anyhow!("line_end column missing")))?;
    let artifact_id = batch
        .column_by_name("artifact_id")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let title = batch
        .column_by_name("title")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let kind = batch
        .column_by_name("kind")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| VectorStoreError::Other(anyhow::anyhow!("kind column missing")))?;
    let content = batch
        .column_by_name("content")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| VectorStoreError::Other(anyhow::anyhow!("content column missing")))?;

    // LanceDB exposes the cosine/L2 distance under the column "_distance"
    // when results come from a vector search.
    let distance = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(ChunkRecord {
            id: id.value(i).to_string(),
            source_path: source_path.value(i).to_string(),
            line_start: line_start.value(i),
            line_end: line_end.value(i),
            artifact_id: artifact_id.and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            title: title.and_then(|t| (!t.is_null(i)).then(|| t.value(i).to_string())),
            kind: kind.value(i).to_string(),
            content: content.value(i).to_string(),
            score: distance.map(|d| d.value(i)),
        });
    }
    Ok(out)
}
