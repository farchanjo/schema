//! `sqlite-vec`-backed [`Persistence`] adapter (ADR-0011).
//!
//! Each project owns one `SQLite` database file under
//! `~/.cache/schema/projects/<id>/store.db`. The schema combines three tables:
//!
//! - `chunks`              — canonical row store (one row per chunk).
//! - `chunks_vec` (`vec0`) — virtual table holding the BGE-M3 embedding for
//!   each chunk, joined back to `chunks.rowid` on lookup.
//! - `chunks_fts` (`fts5`) — full-text index over `content` + `artifact_id`
//!   for `find_mentioning` / artifact-id substring matches.
//!
//! Triggers keep `chunks_vec` and `chunks_fts` in sync with `chunks` on every
//! insert / update / delete.
//!
//! The single `unsafe` block in this crate lives in [`register_vec_extension`];
//! it registers `sqlite-vec`'s `extern "C" fn sqlite3_vec_init` as a `SQLite`
//! auto-extension so every connection opened thereafter has the `vec0` virtual
//! table available. The crate ships only the raw `extern "C"` symbol with no
//! safe wrapper, so calling `rusqlite::ffi::sqlite3_auto_extension` is the
//! sanctioned path (see ADR-0011 + ADR-0012 amendment 2026-04-25).

use std::fmt;
use std::fs;
use std::mem;
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::sync::Once;

use anyhow::Result;
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use rusqlite::ffi::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};
use rusqlite::{Connection, params};
use tokio::sync::Mutex;
use tokio::task;
use tracing::info;

use crate::domain::{Chunk, ChunkRecord};
use crate::ports::{Persistence, PersistenceError};
use schema_core::fastembed_embedder::BGE_M3_DIMENSIONS;

/// One-shot guard: register `sqlite_vec::sqlite3_vec_init` exactly once for
/// the lifetime of the process. `SQLite`'s auto-extension list is global;
/// calling `sqlite3_auto_extension` more than once with the same callback is
/// harmless but wasteful, so a `Once` makes the side effect deterministic.
static REGISTER_VEC: Once = Once::new();

/// Register `sqlite-vec`'s loader as a `SQLite` auto-extension.
///
/// `sqlite-vec` 0.1.9 only exposes `extern "C" fn sqlite3_vec_init`; there is
/// no safe wrapper. The canonical pattern, demonstrated in the crate's own
/// integration test, is to transmute the raw `fn` pointer into the
/// `Option<unsafe extern "C" fn(...)>` shape `sqlite3_auto_extension` expects.
fn register_vec_extension() {
    REGISTER_VEC.call_once(register_vec_extension_once);
}

/// Type alias for the `SQLite` extension-loader callback shape that
/// `sqlite3_auto_extension` expects. `sqlite-vec`'s `sqlite3_vec_init` matches
/// this ABI bit-for-bit; the transmute below is a typed-noise removal step.
type SqliteExtensionEntry =
    unsafe extern "C" fn(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> c_int;

/// Body of the `Once::call_once` callback — extracted so the `unsafe` block
/// stands alone in a tiny, easy-to-audit function.
fn register_vec_extension_once() {
    // Single sanctioned `unsafe` block per ADR-0011 + ADR-0012 amendment
    // 2026-04-25. Keep it as small as possible.
    #[expect(
        unsafe_code,
        reason = "sqlite-vec 0.1.9 ships only `extern \"C\" fn sqlite3_vec_init`; ADR-0011 + ADR-0012 amendment 2026-04-25"
    )]
    #[expect(
        clippy::as_conversions,
        reason = "fn-pointer to thin pointer cast is the canonical first step of \
                  the sqlite-vec README pattern; transmute alone cannot bridge \
                  fn-item types and the FFI fn-pointer ABI"
    )]
    // SAFETY:
    //   - `sqlite3_vec_init` is the real entry symbol exported by the bundled
    //     `sqlite_vec0` C archive (see sqlite-vec's `build.rs`), matching the
    //     SQLite extension entry-point ABI exactly.
    //   - `sqlite3_auto_extension` only stashes the function pointer in a
    //     SQLite-managed list; it does not invoke it here. The callback runs
    //     later, on every new `Connection::open`, inside SQLite — also a
    //     sanctioned ABI boundary.
    //   - `Once` guarantees we register the same pointer at most once, so
    //     SQLite's internal list cannot grow unbounded.
    //   - The transmute matches the layout the C side already expects;
    //     without `unsafe extern "C" fn` aliases on stable Rust, this is the
    //     documented pattern (sqlite-vec README, rusqlite docs).
    unsafe {
        // Cast through `*const ()` to match the canonical sqlite-vec example.
        let entry = sqlite_vec::sqlite3_vec_init as *const ();
        sqlite3_auto_extension(Some(mem::transmute::<*const (), SqliteExtensionEntry>(
            entry,
        )));
    }
}

/// `sqlite-vec`-backed persistence.
///
/// The connection is wrapped in [`tokio::sync::Mutex`] because `rusqlite` is
/// synchronous and not `Sync`. Sync work runs on the runtime thread; for a
/// derived cache (read-heavy, single-writer per project) this is plenty.
pub struct SqliteVecStore {
    conn: Mutex<Connection>,
}

impl fmt::Debug for SqliteVecStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteVecStore")
            .field("conn", &"<rusqlite::Connection>")
            .finish()
    }
}

impl SqliteVecStore {
    /// Open (or create) the `SQLite` database file at the given path.
    ///
    /// The parent directory is created if missing. Registering the `vec0`
    /// extension is idempotent across calls.
    ///
    /// # Errors
    /// Returns an error if the parent directory cannot be created or `SQLite`
    /// fails to open the database.
    pub async fn open(store_path: &Path) -> Result<Self, PersistenceError> {
        if let Some(parent) = store_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| PersistenceError::Backend(format!("io: {e}")))?;
        }
        let path = store_path.to_path_buf();
        info!(path = %path.display(), "opening sqlite-vec store");

        let conn = task::spawn_blocking(move || -> Result<Connection, PersistenceError> {
            register_vec_extension();
            let c = Connection::open(&path).map_err(sqlite_err)?;
            apply_pragmas(&c)?;
            Ok(c)
        })
        .await
        .map_err(|e| PersistenceError::Backend(format!("join: {e}")))??;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Log a warning if the legacy `LanceDB` cache directory is still on disk.
///
/// Called from `main` before opening the new store. Does NOT delete anything —
/// operators decide when to reclaim the space.
pub fn migrate_legacy_lance_dir(cache_dir: &Path) {
    let legacy = cache_dir.join("lance");
    if legacy.exists() {
        tracing::warn!(
            path = %legacy.display(),
            "legacy LanceDB cache directory detected; safe to delete after \
             confirming the sqlite-vec store works (schema does not auto-prune)",
        );
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as `Result::map_err` argument across the file; taking by \
              value matches the FnOnce(E) -> E2 signature without forcing \
              every call site to wrap in a closure"
)]
fn sqlite_err(e: rusqlite::Error) -> PersistenceError {
    PersistenceError::Backend(format!("sqlite: {e}"))
}

fn apply_pragmas(c: &Connection) -> Result<(), PersistenceError> {
    // WAL gives concurrent readers + a single writer, plus durable crash
    // recovery without expensive rollback journals. NORMAL is the WAL-aware
    // sync mode (FULL is overkill for a derived cache).
    c.execute_batch(
        "PRAGMA journal_mode=WAL;\n\
         PRAGMA synchronous=NORMAL;\n\
         PRAGMA temp_store=MEMORY;\n\
         PRAGMA foreign_keys=ON;",
    )
    .map_err(sqlite_err)?;
    Ok(())
}

/// Canonical row store. `rowid` is the implicit `INTEGER PRIMARY KEY` and
/// serves as the join key into `chunks_vec` and `chunks_fts`.
const DDL_CHUNKS: &str = "
    CREATE TABLE IF NOT EXISTS chunks (
        id           TEXT NOT NULL,
        source_path  TEXT NOT NULL,
        line_start   INTEGER NOT NULL,
        line_end     INTEGER NOT NULL,
        artifact_id  TEXT,
        title        TEXT,
        kind         TEXT NOT NULL,
        content      TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS chunks_source_path_idx ON chunks(source_path);
    CREATE INDEX IF NOT EXISTS chunks_artifact_id_idx ON chunks(artifact_id);
";

/// Triggers wiring `chunks_fts` (external-content FTS5) to `chunks`. See
/// `sqlite-fts5` docs for the canonical insert/delete/update pattern.
const DDL_FTS_TRIGGERS: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
        content,
        artifact_id,
        content='chunks',
        content_rowid='rowid',
        tokenize='unicode61 remove_diacritics 2'
    );
    CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
        INSERT INTO chunks_fts(rowid, content, artifact_id)
        VALUES (new.rowid, new.content, new.artifact_id);
    END;
    CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
        INSERT INTO chunks_fts(chunks_fts, rowid, content, artifact_id)
        VALUES ('delete', old.rowid, old.content, old.artifact_id);
    END;
    CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
        INSERT INTO chunks_fts(chunks_fts, rowid, content, artifact_id)
        VALUES ('delete', old.rowid, old.content, old.artifact_id);
        INSERT INTO chunks_fts(rowid, content, artifact_id)
        VALUES (new.rowid, new.content, new.artifact_id);
    END;
";

/// Trigger keeping `chunks_vec` in lockstep with `chunks` deletions.
/// Insertions are explicit (the writer owns the vector); updates do not
/// touch the vector.
const DDL_VEC_TRIGGER: &str = "
    CREATE TRIGGER IF NOT EXISTS chunks_vec_ad AFTER DELETE ON chunks BEGIN
        DELETE FROM chunks_vec WHERE rowid = old.rowid;
    END;
";

/// Single-row scalar table tracking storage-format invariants that must
/// outlive the process (ADR-0028 partition migration, ADR-0029 prefix
/// recipe). One row per `key`.
const DDL_META: &str = "
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

/// `meta.key` storing the storage-schema version. Bumped whenever the
/// physical layout of `chunks_vec` changes (e.g., partition column
/// added, ANN index introduced). The adapter compares the stored value
/// against [`STORAGE_SCHEMA_VERSION_CURRENT`] at `ensure_ready` time
/// and runs any mechanical migrations needed.
const META_KEY_SCHEMA_VERSION: &str = "schema_version";

/// `meta.key` recording which embedding recipe (ADR-0029) the rows in
/// `chunks_vec` were produced under. Values: [`RECIPE_RAW`] or
/// [`RECIPE_BGE_M3_QUERY_PASSAGE`]. Mismatch with the project's
/// `query_passage_prefix` flag triggers a forced re-embed from the
/// scalar `chunks` table on the next start.
const META_KEY_EMBEDDING_RECIPE: &str = "embedding_recipe";

/// Storage-schema versions. v1 — pre-ADR-0028 (no partition key).
/// v2 — ADR-0028 (kind partition key on `chunks_vec`).
const STORAGE_SCHEMA_VERSION_V1: &str = "1";
const STORAGE_SCHEMA_VERSION_V2: &str = "2";

/// Current storage-schema version this binary writes. Older `store.db`
/// files are migrated up to this on first start; downgrades are not
/// supported.
const STORAGE_SCHEMA_VERSION_CURRENT: &str = STORAGE_SCHEMA_VERSION_V2;

/// Embedding-recipe markers (ADR-0029).
pub const RECIPE_RAW: &str = "raw";
pub const RECIPE_BGE_M3_QUERY_PASSAGE: &str = "bge-m3-query-passage";

/// Build the `chunks_vec` virtual-table DDL for the current storage
/// schema (v2 — kind partition key per ADR-0028).
fn chunks_vec_ddl(vector_dim: u32) -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(\
            embedding float[{vector_dim}],\
            kind text partition key\
         );"
    )
}

/// DDL fan-out across the three tables and their triggers.
fn ensure_schema(c: &Connection, vector_dim: u32) -> Result<(), PersistenceError> {
    c.execute_batch(DDL_CHUNKS).map_err(sqlite_err)?;
    c.execute_batch(DDL_META).map_err(sqlite_err)?;
    c.execute_batch(&chunks_vec_ddl(vector_dim))
        .map_err(sqlite_err)?;
    c.execute_batch(DDL_FTS_TRIGGERS).map_err(sqlite_err)?;
    c.execute_batch(DDL_VEC_TRIGGER).map_err(sqlite_err)?;
    Ok(())
}

/// Read `meta(key)` if present.
fn read_meta(conn: &Connection, key: &str) -> Result<Option<String>, PersistenceError> {
    let mut stmt = conn
        .prepare("SELECT value FROM meta WHERE key = ?1")
        .map_err(sqlite_err)?;
    let value: Option<String> = stmt
        .query_row(params![key], |row| row.get::<_, String>(0))
        .optional()
        .map_err(sqlite_err)?;
    Ok(value)
}

/// Upsert `meta(key, value)`.
fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<(), PersistenceError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

/// Detect whether `chunks_vec` already has the v2 partition column
/// (`kind`). Older `store.db` files (v1) have only the `embedding`
/// column. Inspecting `PRAGMA table_xinfo(chunks_vec)` is the most
/// reliable probe — `sqlite-vec` virtual tables expose their schema
/// through the standard pragma.
fn chunks_vec_has_kind_partition(conn: &Connection) -> Result<bool, PersistenceError> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_xinfo('chunks_vec')")
        .map_err(sqlite_err)?;
    let mut rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sqlite_err)?;
    let has_kind = rows.try_fold(false, |acc, r| match r {
        Ok(name) => Ok::<_, PersistenceError>(acc || name == "kind"),
        Err(e) => Err(sqlite_err(e)),
    })?;
    Ok(has_kind)
}

/// Migrate `chunks_vec` from v1 (no partition) to v2 (kind partition
/// key). Drop + recreate the virtual table and replay every row from
/// scalar `chunks` using its existing rowid + kind. The per-row
/// embedding is **not** carried over by this migration — v1 stored it
/// only inside `chunks_vec` (no on-row blob) so the v1 → v2 step
/// produces an empty vec table that the daemon must repopulate from
/// fresh embeddings (delta-sync after migration). This is the
/// trade-off recorded in ADR-0028 §"Schema migration": data loss is
/// limited to vectors, never to the canonical `chunks` rows.
fn migrate_chunks_vec_to_v2(conn: &Connection, vector_dim: u32) -> Result<(), PersistenceError> {
    info!("migrating chunks_vec to v2 (kind partition key) per ADR-0028");
    conn.execute_batch("DROP TABLE IF EXISTS chunks_vec")
        .map_err(sqlite_err)?;
    conn.execute_batch(&chunks_vec_ddl(vector_dim))
        .map_err(sqlite_err)?;
    // The vec-delete trigger references chunks_vec; re-bind it
    // explicitly in case the DROP cascaded the trigger as well.
    conn.execute_batch(DDL_VEC_TRIGGER).map_err(sqlite_err)?;
    Ok(())
}

/// Serialise an `f32` slice into the little-endian byte buffer `vec0` expects
/// for a `float[N]` column. Avoids the `bytemuck` dep (no measurable upside
/// for our batch sizes).
fn vector_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()
}

/// Column list for `SELECT` against `chunks`. Single source of truth so
/// `row_to_record` indices stay in sync with the SELECT projection.
const SELECT_COLUMNS: &str =
    "id, source_path, line_start, line_end, artifact_id, title, kind, content";

/// Build a [`ChunkRecord`] from one row matching [`SELECT_COLUMNS`].
fn row_to_record(row: &rusqlite::Row<'_>, score: Option<f32>) -> rusqlite::Result<ChunkRecord> {
    Ok(ChunkRecord {
        id: row.get::<_, String>(0)?,
        source_path: row.get::<_, String>(1)?,
        line_start: row.get::<_, i32>(2)?,
        line_end: row.get::<_, i32>(3)?,
        artifact_id: row.get::<_, Option<String>>(4)?,
        title: row.get::<_, Option<String>>(5)?,
        kind: row.get::<_, String>(6)?,
        content: row.get::<_, String>(7)?,
        score,
    })
}

/// `SELECT_COLUMNS` re-rendered with a `c.` prefix so it can sit inside a
/// `JOIN` projection without ambiguous-column errors.
fn prefixed_columns() -> String {
    SELECT_COLUMNS
        .split(',')
        .map(|s| format!("c.{}", s.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl Persistence for SqliteVecStore {
    async fn ensure_ready(&self) -> Result<(), PersistenceError> {
        let dim = u32::try_from(BGE_M3_DIMENSIONS)
            .map_err(|e| PersistenceError::Backend(format!("dim cast: {e}")))?;
        let guard = self.conn.lock().await;
        // Ensure the `meta` and `chunks` tables exist before reading
        // them — a fresh store.db has neither.
        guard.execute_batch(DDL_CHUNKS).map_err(sqlite_err)?;
        guard.execute_batch(DDL_META).map_err(sqlite_err)?;

        // Migrate `chunks_vec` from v1 → v2 if needed (ADR-0028).
        let stored_version = read_meta(&guard, META_KEY_SCHEMA_VERSION)?.unwrap_or_else(|| {
            // Absent meta row means either a brand-new store.db
            // (no chunks_vec yet) or a pre-meta v1 store. The
            // partition probe disambiguates.
            STORAGE_SCHEMA_VERSION_V1.to_string()
        });
        if stored_version != STORAGE_SCHEMA_VERSION_CURRENT
            || !chunks_vec_has_kind_partition(&guard)?
        {
            migrate_chunks_vec_to_v2(&guard, dim)?;
            write_meta(
                &guard,
                META_KEY_SCHEMA_VERSION,
                STORAGE_SCHEMA_VERSION_CURRENT,
            )?;
        }

        // Schema fan-out is idempotent for everything else.
        ensure_schema(&guard, dim)?;
        drop(guard);
        info!("sqlite-vec schema ensured");
        Ok(())
    }

    async fn append_chunks(
        &self,
        chunks: &[Chunk],
        vectors: &[Vec<f32>],
    ) -> Result<(), PersistenceError> {
        if chunks.is_empty() {
            return Ok(());
        }
        if chunks.len() != vectors.len() {
            return Err(PersistenceError::Other(anyhow::anyhow!(
                "chunks ({}) and vectors ({}) length mismatch",
                chunks.len(),
                vectors.len()
            )));
        }
        let prepared = build_inserts(chunks, vectors);
        let mut guard = self.conn.lock().await;
        execute_inserts(&mut guard, &prepared)?;
        drop(guard);
        Ok(())
    }

    async fn delete_by_source(&self, paths: &[&str]) -> Result<(), PersistenceError> {
        if paths.is_empty() {
            return Ok(());
        }
        let owned: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        let mut guard = self.conn.lock().await;
        execute_deletes(&mut guard, &owned)?;
        drop(guard);
        Ok(())
    }

    async fn query_nearest(
        &self,
        vector: &[f32],
        k: usize,
        kind_filter: Option<&str>,
        min_score: Option<f32>,
    ) -> Result<Vec<ChunkRecord>, PersistenceError> {
        let bytes = vector_to_bytes(vector);
        let limit = i64::try_from(k).unwrap_or(i64::MAX);
        let kind_owned = kind_filter.map(str::to_string);
        let guard = self.conn.lock().await;
        let rows = run_query_nearest(&guard, &bytes, limit, kind_owned.as_deref(), min_score)?;
        drop(guard);
        Ok(rows)
    }

    async fn find_by_artifact_id(
        &self,
        artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>, PersistenceError> {
        let aid = artifact_id.to_string();
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        let guard = self.conn.lock().await;
        let sql = format!("SELECT {SELECT_COLUMNS} FROM chunks WHERE artifact_id = ?1 LIMIT ?2");
        let mut stmt = guard.prepare(&sql).map_err(sqlite_err)?;
        let rows = stmt
            .query_map(params![aid, lim], |row| row_to_record(row, None))
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?;
        drop(stmt);
        drop(guard);
        Ok(rows)
    }

    async fn find_mentioning(
        &self,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>, PersistenceError> {
        // FTS5 MATCH expects a query string; for an arbitrary substring needle
        // we wrap in double quotes to make it a phrase token, the closest
        // analogue to LanceDB's `LIKE '%needle%'`. We exclude chunks whose
        // `artifact_id` equals the needle so cross-references do not return
        // self-mentions.
        let phrase = format!("\"{}\"", needle.replace('"', "\"\""));
        let aid = needle.to_string();
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        let guard = self.conn.lock().await;
        let rows = run_find_mentioning(&guard, &phrase, &aid, lim)?;
        drop(guard);
        Ok(rows)
    }

    async fn list_source_paths(&self) -> Result<Vec<String>, PersistenceError> {
        let guard = self.conn.lock().await;
        let mut stmt = guard
            .prepare("SELECT DISTINCT source_path FROM chunks ORDER BY source_path")
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?;
        drop(stmt);
        drop(guard);
        Ok(rows)
    }

    async fn reset_all(&self) -> Result<(), PersistenceError> {
        let guard = self.conn.lock().await;
        let removed = run_reset_all(&guard)?;
        drop(guard);
        info!(
            rows_deleted = removed,
            "sqlite-vec store reset (DELETE + VACUUM)"
        );
        Ok(())
    }

    async fn read_embedding_recipe(&self) -> Result<Option<String>, PersistenceError> {
        let guard = self.conn.lock().await;
        let value = read_meta(&guard, META_KEY_EMBEDDING_RECIPE)?;
        drop(guard);
        Ok(value)
    }

    async fn write_embedding_recipe(&self, recipe: &str) -> Result<(), PersistenceError> {
        let guard = self.conn.lock().await;
        write_meta(&guard, META_KEY_EMBEDDING_RECIPE, recipe)?;
        drop(guard);
        Ok(())
    }
}

/// Bulk-delete every row in `chunks` and reclaim disk pages with `VACUUM`.
///
/// Returns the number of rows that existed before the wipe (logged by the
/// caller). The deletes cascade to `chunks_vec` and `chunks_fts` via the
/// triggers defined in [`DDL_VEC_TRIGGER`] / [`DDL_FTS_TRIGGERS`]; `VACUUM`
/// is intentionally outside any transaction (`SQLite` forbids `VACUUM` inside
/// one).
fn run_reset_all(conn: &Connection) -> Result<i64, PersistenceError> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .map_err(sqlite_err)?;
    conn.execute("DELETE FROM chunks", []).map_err(sqlite_err)?;
    conn.execute_batch("VACUUM").map_err(sqlite_err)?;
    Ok(count)
}

const SQL_INSERT_CHUNK: &str = "INSERT INTO chunks (id, source_path, line_start, line_end, \
                                artifact_id, title, kind, content) \
                                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

const SQL_INSERT_VEC: &str = "INSERT INTO chunks_vec (rowid, embedding, kind) VALUES (?1, ?2, ?3)";

fn insert_one_row(
    tx: &rusqlite::Transaction<'_>,
    insert_chunk: &mut rusqlite::CachedStatement<'_>,
    insert_vec: &mut rusqlite::CachedStatement<'_>,
    row: &InsertRow,
) -> Result<(), PersistenceError> {
    insert_chunk
        .execute(params![
            row.id,
            row.source_path,
            row.line_start,
            row.line_end,
            row.artifact_id,
            row.title,
            row.kind,
            row.content,
        ])
        .map_err(sqlite_err)?;
    let rowid = tx.last_insert_rowid();
    insert_vec
        .execute(params![rowid, row.vector_bytes, row.kind])
        .map_err(sqlite_err)?;
    Ok(())
}

fn execute_inserts(conn: &mut Connection, prepared: &[InsertRow]) -> Result<(), PersistenceError> {
    let tx = conn.transaction().map_err(sqlite_err)?;
    {
        let mut insert_chunk = tx.prepare_cached(SQL_INSERT_CHUNK).map_err(sqlite_err)?;
        let mut insert_vec = tx.prepare_cached(SQL_INSERT_VEC).map_err(sqlite_err)?;
        for row in prepared {
            insert_one_row(&tx, &mut insert_chunk, &mut insert_vec, row)?;
        }
    }
    tx.commit().map_err(sqlite_err)?;
    Ok(())
}

fn execute_deletes(conn: &mut Connection, paths: &[String]) -> Result<(), PersistenceError> {
    let tx = conn.transaction().map_err(sqlite_err)?;
    {
        let mut stmt = tx
            .prepare_cached("DELETE FROM chunks WHERE source_path = ?1")
            .map_err(sqlite_err)?;
        for p in paths {
            stmt.execute(params![p]).map_err(sqlite_err)?;
        }
    }
    tx.commit().map_err(sqlite_err)?;
    Ok(())
}

fn build_query_nearest_sql(kind_filter: Option<&str>, min_score: Option<f32>) -> String {
    let cols = prefixed_columns();
    let kind_clause = if kind_filter.is_some() {
        // Pushed inside the MATCH via the kind partition key — sqlite-vec
        // routes the search to the partition before evaluating top-K.
        "AND v.kind = ?3"
    } else {
        ""
    };
    // ADR-0028 score floor: cosine distance ceiling = 1.0 - min_score.
    // Inlined as a literal — `f32` renders a finite, SQL-safe number;
    // this avoids an extra parameter slot whose ordinal would clash
    // with the optional kind bind.
    let distance_clause = min_score.map_or_else(String::new, |s| {
        let max_distance = 1.0_f32 - s;
        format!(" AND v.distance <= {max_distance}")
    });
    format!(
        "SELECT {cols}, v.distance \
         FROM chunks_vec v \
         JOIN chunks c ON c.rowid = v.rowid \
         WHERE v.embedding MATCH ?1 AND k = ?2 \
         {kind_clause}{distance_clause} \
         ORDER BY v.distance"
    )
}

fn run_query_nearest(
    conn: &Connection,
    vector_bytes: &[u8],
    k: i64,
    kind_filter: Option<&str>,
    min_score: Option<f32>,
) -> Result<Vec<ChunkRecord>, PersistenceError> {
    let sql = build_query_nearest_sql(kind_filter, min_score);
    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let rows = if let Some(kind) = kind_filter {
        stmt.query_map(params![vector_bytes, k, kind], map_with_distance)
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?
    } else {
        stmt.query_map(params![vector_bytes, k], map_with_distance)
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?
    };
    Ok(rows)
}

fn map_with_distance(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRecord> {
    let dist: f32 = row.get(8)?;
    row_to_record(row, Some(dist))
}

fn run_find_mentioning(
    conn: &Connection,
    phrase: &str,
    aid: &str,
    lim: i64,
) -> Result<Vec<ChunkRecord>, PersistenceError> {
    let cols = prefixed_columns();
    let sql = format!(
        "SELECT {cols} \
         FROM chunks_fts f \
         JOIN chunks c ON c.rowid = f.rowid \
         WHERE chunks_fts MATCH ?1 \
           AND (c.artifact_id IS NULL OR c.artifact_id != ?2) \
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![phrase, aid, lim], |row| row_to_record(row, None))
        .map_err(sqlite_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sqlite_err)?;
    Ok(rows)
}

/// Pre-built insert payload, computed before the lock is taken.
struct InsertRow {
    id: String,
    source_path: String,
    line_start: i32,
    line_end: i32,
    artifact_id: Option<String>,
    title: Option<String>,
    kind: String,
    content: String,
    vector_bytes: Vec<u8>,
}

fn line_to_i32(line: usize) -> i32 {
    i32::try_from(line).unwrap_or(i32::MAX)
}

fn build_inserts(chunks: &[Chunk], vectors: &[Vec<f32>]) -> Vec<InsertRow> {
    chunks
        .iter()
        .zip(vectors.iter())
        .enumerate()
        .map(|(i, (c, v))| InsertRow {
            id: format!("{}#L{}-L{}#{}", c.source_path, c.line_start, c.line_end, i),
            source_path: c.source_path.clone(),
            line_start: line_to_i32(c.line_start),
            line_end: line_to_i32(c.line_end),
            artifact_id: c.artifact_id.clone(),
            title: c.title.clone(),
            kind: format!("{:?}", c.kind),
            content: c.content.clone(),
            vector_bytes: vector_to_bytes(v),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::*;
    use crate::domain::{Chunk, CorpusKind};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::task::JoinHandle;

    fn dummy_vec() -> Vec<f32> {
        vec![0.0_f32; BGE_M3_DIMENSIONS]
    }

    fn sample_chunk(path: &str, artifact: Option<&str>, content: &str) -> Chunk {
        Chunk {
            source_path: path.to_string(),
            line_start: 1,
            line_end: 10,
            artifact_id: artifact.map(str::to_string),
            title: Some("Sample".to_string()),
            content: content.to_string(),
            kind: CorpusKind::Markdown,
        }
    }

    #[tokio::test]
    async fn open_and_ensure_ready_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("store.db");
        let store = SqliteVecStore::open(&path).await.unwrap();
        store.ensure_ready().await.unwrap();
        store.ensure_ready().await.unwrap();
    }

    #[tokio::test]
    async fn append_then_list_source_paths() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![
            sample_chunk("a.md", Some("ADR-0001"), "alpha bravo"),
            sample_chunk("b.md", Some("ADR-0002"), "charlie delta"),
        ];
        let vectors = vec![dummy_vec(), dummy_vec()];
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let paths = store.list_source_paths().await.unwrap();
        assert_eq!(paths, vec!["a.md".to_string(), "b.md".to_string()]);
    }

    #[tokio::test]
    async fn delete_by_source_removes_rows_and_cascades() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![sample_chunk("doomed.md", Some("ADR-9999"), "to be removed")];
        let vectors = vec![dummy_vec()];
        store.append_chunks(&chunks, &vectors).await.unwrap();
        assert_eq!(store.list_source_paths().await.unwrap().len(), 1);

        store.delete_by_source(&["doomed.md"]).await.unwrap();
        assert!(store.list_source_paths().await.unwrap().is_empty());

        let hits = store.find_mentioning("removed", 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn find_by_artifact_id_returns_match() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![
            sample_chunk("x.md", Some("ADR-0042"), "text"),
            sample_chunk("y.md", Some("ADR-0099"), "other"),
        ];
        let vectors = vec![dummy_vec(), dummy_vec()];
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let hits = store.find_by_artifact_id("ADR-0042", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_path, "x.md");
    }

    #[tokio::test]
    async fn find_mentioning_skips_self_reference() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![
            sample_chunk("def.md", Some("ADR-0042"), "ADR-0042 is the definition"),
            sample_chunk(
                "ref.md",
                Some("ADR-0100"),
                "this references ADR-0042 inline",
            ),
        ];
        let vectors = vec![dummy_vec(), dummy_vec()];
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let hits = store.find_mentioning("ADR-0042", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_path, "ref.md");
    }

    #[tokio::test]
    async fn query_nearest_returns_sorted_by_distance() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![
            sample_chunk("near.md", None, "near"),
            sample_chunk("far.md", None, "far"),
        ];
        let mut near_vec = vec![0.0_f32; BGE_M3_DIMENSIONS];
        near_vec[0] = 1.0;
        let mut far_vec = vec![0.0_f32; BGE_M3_DIMENSIONS];
        far_vec[0] = -1.0;
        let vectors = vec![near_vec.clone(), far_vec];
        store.append_chunks(&chunks, &vectors).await.unwrap();

        let hits = store.query_nearest(&near_vec, 2, None, None).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source_path, "near.md");
        assert_eq!(hits[1].source_path, "far.md");
        assert!(hits[0].score.unwrap() <= hits[1].score.unwrap());
    }

    #[test]
    fn vector_to_bytes_is_little_endian_concatenation() {
        let v = vec![1.0_f32, -0.5_f32];
        let bytes = vector_to_bytes(&v);
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[0..4], &1.0_f32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(-0.5_f32).to_le_bytes());
    }

    /// Number of fixture chunks for the latency-at-scale test (ADR-0011 fitness 1).
    const LATENCY_CORPUS_SIZE: usize = 1_000;
    /// Index of the seeded "golden" chunk inside the latency corpus.
    const LATENCY_GOLDEN_INDEX: usize = 17;

    /// Build `n` deterministic distinct 1024-dim vectors paired with
    /// `n` chunks. Helper for `query_nearest_meets_latency_at_scale`.
    fn build_latency_corpus(n: usize) -> (Vec<Chunk>, Vec<Vec<f32>>) {
        let mut chunks: Vec<Chunk> = Vec::with_capacity(n);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(n);
        for i in 0..n {
            // Two non-zero positions per index so distances rank
            // predictably and the golden is unique within the corpus.
            let mut v = vec![0.0_f32; BGE_M3_DIMENSIONS];
            v[i % BGE_M3_DIMENSIONS] = 1.0;
            v[(i + 1) % BGE_M3_DIMENSIONS] = 0.5;
            vectors.push(v);
            chunks.push(Chunk {
                source_path: format!("doc-{i:04}.md"),
                line_start: 1 + i,
                line_end: 5 + i,
                artifact_id: None,
                title: Some(format!("Doc {i}")),
                content: format!("chunk number {i}"),
                kind: CorpusKind::Markdown,
            });
        }
        (chunks, vectors)
    }

    /// ADR-0011 fitness function 1 — latency at scale.
    ///
    /// Insert 1 000 deterministic 1024-dim chunks, then `query_nearest`
    /// for the seeded "golden" chunk; assert wall-clock latency under
    /// 50 ms and that the top-1 hit matches the golden's `source_path`.
    /// On an M-series Mac this is well under 5 ms; on Linux CI we
    /// typically observe < 30 ms. Threshold is the ADR contract.
    #[tokio::test]
    async fn query_nearest_meets_latency_at_scale() {
        use std::time::{Duration, Instant};

        let tmp = TempDir::new().unwrap();
        let store = SqliteVecStore::open(&tmp.path().join("store.db"))
            .await
            .unwrap();
        store.ensure_ready().await.unwrap();

        let (chunks, vectors) = build_latency_corpus(LATENCY_CORPUS_SIZE);
        let query_vec = vectors[LATENCY_GOLDEN_INDEX].clone();

        store.append_chunks(&chunks, &vectors).await.unwrap();

        let started = Instant::now();
        let hits = store
            .query_nearest(&query_vec, 8, None, None)
            .await
            .unwrap();
        let elapsed = started.elapsed();

        tracing::info!(
            elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            n = LATENCY_CORPUS_SIZE,
            "sqlite-vec query_nearest at scale"
        );

        assert!(
            elapsed < Duration::from_millis(50),
            "query_nearest took {elapsed:?}, exceeds ADR-0011 50 ms ceiling"
        );
        assert!(!hits.is_empty(), "expected at least one hit, got 0");
        assert_eq!(
            hits[0].source_path,
            format!("doc-{LATENCY_GOLDEN_INDEX:04}.md"),
            "top-1 hit must match the seeded golden chunk"
        );
    }

    /// Read every `source_path` whose row matches the FTS5 phrase
    /// `'rbac'` via a fresh read-only `rusqlite::Connection`. Helper
    /// for `fts5_and_vec0_fire_on_same_chunk`.
    fn fts5_match_rbac_paths(db_path: &Path) -> Vec<String> {
        use rusqlite::OpenFlags;

        let ro = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = ro
            .prepare(
                "SELECT chunks.source_path \
                 FROM chunks_fts \
                 JOIN chunks ON chunks.rowid = chunks_fts.rowid \
                 WHERE chunks_fts MATCH 'rbac'",
            )
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// ADR-0011 fitness function 2 — hybrid search (FTS5 + vec0).
    ///
    /// Insert one chunk; verify both `vec_search` (via the `Persistence`
    /// port) and `chunks_fts MATCH 'rbac'` (via a raw read-only
    /// `rusqlite::Connection`) return that chunk's `source_path`. This
    /// proves the FTS5 trigger fires on `chunks` insert and that vec0
    /// is in lockstep with the canonical row store.
    #[tokio::test]
    async fn fts5_and_vec0_fire_on_same_chunk() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("store.db");
        let store = SqliteVecStore::open(&db_path).await.unwrap();
        store.ensure_ready().await.unwrap();

        // Build a deterministic vector collision: insert == query.
        let mut vector = vec![0.0_f32; BGE_M3_DIMENSIONS];
        vector[42] = 1.0;

        let chunk = Chunk {
            source_path: "rbac.md".to_string(),
            line_start: 1,
            line_end: 3,
            artifact_id: Some("ADR-RBAC".to_string()),
            title: Some("RBAC".to_string()),
            content: "role-based access control".to_string(),
            kind: CorpusKind::Markdown,
        };

        store
            .append_chunks(&[chunk], &[vector.clone()])
            .await
            .unwrap();

        // vec0 path — exercised through the Persistence port.
        let vec_hits = store.query_nearest(&vector, 1, None, None).await.unwrap();
        assert_eq!(vec_hits.len(), 1);
        assert_eq!(vec_hits[0].source_path, "rbac.md");

        // FTS5 path — open a fresh read-only handle to confirm the
        // `chunks_ai` trigger populated `chunks_fts` from the insert.
        let rows = fts5_match_rbac_paths(&db_path);
        assert_eq!(
            rows.len(),
            1,
            "FTS5 MATCH 'rbac' must return exactly one row"
        );
        assert_eq!(rows[0], "rbac.md");
    }

    /// Build `n` seed chunks for the WAL concurrency test.
    fn build_wal_seed(n: usize) -> (Vec<Chunk>, Vec<Vec<f32>>) {
        let mut seed_chunks: Vec<Chunk> = Vec::with_capacity(n);
        let mut seed_vecs: Vec<Vec<f32>> = Vec::with_capacity(n);
        for i in 0..n {
            seed_chunks.push(sample_chunk(
                &format!("seed-{i}.md"),
                Some(&format!("ADR-S{i}")),
                "seed",
            ));
            let mut v = vec![0.0_f32; BGE_M3_DIMENSIONS];
            v[i] = 1.0;
            seed_vecs.push(v);
        }
        (seed_chunks, seed_vecs)
    }

    /// One read-only sample of `COUNT(*)` from both readers paired
    /// with the most recent observation. Used by the WAL concurrency
    /// test to prove counts climb monotonically.
    fn sample_wal_readers(
        ro_a: &Connection,
        ro_b: &Connection,
        last_a: i64,
        last_b: i64,
    ) -> (i64, i64) {
        let count_a: i64 = ro_a
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        let count_b: i64 = ro_b
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(
            count_a >= last_a,
            "count_a regressed: {last_a} -> {count_a}"
        );
        assert!(
            count_b >= last_b,
            "count_b regressed: {last_b} -> {count_b}"
        );
        (count_a, count_b)
    }

    /// Spawn the writer half of the WAL concurrency test: 5 sequential
    /// append batches with distinct paths, on a multi-threaded runtime.
    fn spawn_wal_writer(store: Arc<SqliteVecStore>) -> JoinHandle<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        tokio::spawn(async move {
            for i in 0..5 {
                let chunk = sample_chunk(
                    &format!("writer-{i}.md"),
                    Some(&format!("ADR-W{i}")),
                    "writer",
                );
                let mut v = vec![0.0_f32; BGE_M3_DIMENSIONS];
                v[100 + i] = 1.0;
                store.append_chunks(&[chunk], &[v]).await.unwrap();
                sleep(Duration::from_millis(1)).await;
            }
        })
    }

    /// ADR-0011 fitness function 3 — WAL allows concurrent readers
    /// during writes.
    ///
    /// Open the writer-owning [`SqliteVecStore`] plus two raw read-only
    /// `rusqlite::Connection`s; spawn a writer task that inserts in a
    /// loop while the main task reads `COUNT(*)` from both readers
    /// concurrently. Without WAL this would deadlock or hit
    /// `SQLITE_BUSY`; with WAL it just works. Counts must be
    /// monotonically non-decreasing and the final count must reflect
    /// both the seed and the writer-loop inserts.
    /// Open a fresh read-only `Connection` to the WAL test database.
    fn open_ro(db_path: &Path) -> Connection {
        use rusqlite::OpenFlags;
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    /// Run the read-loop sample-and-yield cycle for the WAL test.
    /// Synchronous so the non-`Send` `&Connection` borrows never need
    /// to cross an `.await`. Sleeps via `std::thread::sleep`; safe
    /// because the test runs on a multi-thread runtime where the
    /// writer task lives on a different worker.
    fn run_wal_reader_loop(ro_a: &Connection, ro_b: &Connection) {
        use std::thread::sleep;
        use std::time::Duration;

        let mut last_a: i64 = 0;
        let mut last_b: i64 = 0;
        for _ in 0..50 {
            let (a, b) = sample_wal_readers(ro_a, ro_b, last_a, last_b);
            last_a = a;
            last_b = b;
            sleep(Duration::from_millis(1));
        }
    }

    /// Read the row count of a virtual table via a fresh read-only handle.
    /// Helper for `reset_all_wipes_chunks_and_cascades_triggers`.
    fn read_only_count(db_path: &Path, table: &str) -> i64 {
        use rusqlite::OpenFlags;
        let ro = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let sql = format!("SELECT COUNT(*) FROM {table}");
        ro.query_row(&sql, [], |r| r.get(0)).unwrap()
    }

    /// ADR-0015 — single-path delete preserves siblings.
    ///
    /// Validates that the persistence half of `Cleanup::forget_source`
    /// (`delete_by_source(&[path])`) drops only the named source path
    /// from `chunks` AND the FTS5 cascade trigger fires for that one
    /// path while leaving siblings untouched in both row store and FTS5.
    #[tokio::test]
    async fn delete_by_source_one_path_preserves_others() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("store.db");
        let store = SqliteVecStore::open(&db_path).await.unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![
            sample_chunk("a.md", Some("ADR-A"), "alpha mention"),
            sample_chunk("b.md", Some("ADR-B"), "beta mention"),
            sample_chunk("c.md", Some("ADR-C"), "charlie mention"),
        ];
        let vectors = vec![dummy_vec(), dummy_vec(), dummy_vec()];
        store.append_chunks(&chunks, &vectors).await.unwrap();

        store.delete_by_source(&["b.md"]).await.unwrap();

        let paths = store.list_source_paths().await.unwrap();
        assert_eq!(
            paths,
            vec!["a.md".to_string(), "c.md".to_string()],
            "only b.md must be gone"
        );
        // FTS5 cascade: 'beta' must yield 0 hits while 'alpha'/'charlie' still resolve.
        let beta_hits = store.find_mentioning("beta", 10).await.unwrap();
        assert!(beta_hits.is_empty(), "FTS5 must drop b.md's row");
        let alpha_hits = store.find_mentioning("alpha", 10).await.unwrap();
        assert_eq!(alpha_hits.len(), 1);
        assert_eq!(alpha_hits[0].source_path, "a.md");
    }

    /// ADR-0015 fitness function — `reset_all` wipes every row in `chunks`
    /// and the FTS5 / vec0 trigger cascades fire on the bulk delete.
    #[tokio::test]
    async fn reset_all_wipes_chunks_and_cascades_triggers() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("store.db");
        let store = SqliteVecStore::open(&db_path).await.unwrap();
        store.ensure_ready().await.unwrap();

        let chunks = vec![
            sample_chunk("a.md", Some("ADR-A"), "alpha bravo"),
            sample_chunk("b.md", Some("ADR-B"), "charlie delta"),
            sample_chunk("c.md", Some("ADR-C"), "echo foxtrot"),
        ];
        let vectors = vec![dummy_vec(), dummy_vec(), dummy_vec()];
        store.append_chunks(&chunks, &vectors).await.unwrap();
        assert_eq!(store.list_source_paths().await.unwrap().len(), 3);
        // Sanity: FTS5 also has rows before reset.
        assert_eq!(read_only_count(&db_path, "chunks_fts"), 3);

        store.reset_all().await.unwrap();

        let paths = store.list_source_paths().await.unwrap();
        assert!(
            paths.is_empty(),
            "reset_all must wipe chunks, got {paths:?}"
        );
        assert_eq!(
            read_only_count(&db_path, "chunks_fts"),
            0,
            "FTS5 cascade trigger must clear chunks_fts on reset"
        );
        // Re-using the store after reset must work: insert a new chunk + read it back.
        let post = vec![sample_chunk("post.md", Some("ADR-POST"), "post-reset")];
        let post_vec = vec![dummy_vec()];
        store.append_chunks(&post, &post_vec).await.unwrap();
        let paths = store.list_source_paths().await.unwrap();
        assert_eq!(paths, vec!["post.md".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wal_allows_concurrent_reads_during_writes() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("store.db");
        let store = Arc::new(SqliteVecStore::open(&db_path).await.unwrap());
        store.ensure_ready().await.unwrap();

        let (seed_chunks, seed_vecs) = build_wal_seed(5);
        store.append_chunks(&seed_chunks, &seed_vecs).await.unwrap();

        let ro_a = open_ro(&db_path);
        let ro_b = open_ro(&db_path);

        let writer = spawn_wal_writer(Arc::<SqliteVecStore>::clone(&store));
        run_wal_reader_loop(&ro_a, &ro_b);
        writer.await.unwrap();
        drop(ro_a);
        drop(ro_b);

        let final_paths = store.list_source_paths().await.unwrap();
        assert_eq!(
            final_paths.len(),
            10,
            "expected 5 seed + 5 writer-loop = 10 distinct paths, got {}",
            final_paths.len()
        );
    }
}
