//! Shared multi-project daemon (ADR-0026 + ADR-0027).
//!
//! Composition root for the post-ADR-0027 deployment:
//!
//! - **One** [`Embedder`](crate::ports::Embedder) per workstation
//!   (the ~1.7 GB BGE-M3 ONNX baseline that ADR-0026 was built
//!   around). The daemon owns the `Arc<Mutex<dyn Embedder>>`; every
//!   resolved [`ProjectInstance`] receives an `Arc::clone` of it.
//! - **Optional shared** [`LlmProvider`](crate::ports::LlmProvider)
//!   (Anthropic / `OpenAI` per ADR-0025). Workstation-level — every
//!   resolved project inherits the same provider.
//! - **Lazy** `Map<ProjectId, Arc<ProjectInstance>>` populated on
//!   the first request that names each project. ADR-0008's per-
//!   `project_id` cache directory remains the on-disk source of
//!   truth; the daemon's map is just a runtime cache of wired
//!   instances keyed by the same id.
//! - **One single-token** [`crate::adapters::auth::BearerValidator`]
//!   gates the daemon's HTTP surface (ADR-0021); project membership
//!   is the LLM's job at tool-call time, not the auth layer's
//!   (ADR-0027 reverted the multi-tenant validator from ADR-0026
//!   slice 1).
//!
//! ## Resolution
//!
//! [`Daemon::resolve_or_wire`] takes a `working_directory: &Path`
//! supplied by the LLM (Claude Code) on every project-scoped tool
//! call. It walks up from `working_directory` looking for a
//! `schema.toml`, resolves the project via
//! [`crate::adapters::project_identity::ProjectIdentity::resolve`]
//!
//! ## Walk-up resolution
//! (ADR-0008 path-keyed identity), and either returns the cached
//! [`ProjectInstance`] or wires a new one (initial delta-sync
//! inline; subsequent calls hit the warm cache). Cross-project
//! bleed is impossible because each `working_directory` resolves
//! to exactly one `project_id` and that id selects exactly one
//! `store.db` (ADR-0008 §"Per-project cache directory" stays the
//! physical isolation guarantee).
//!
//! ## Single-project compatibility
//!
//! ADR-0019's per-project `schema serve --config schema.toml` path
//! reuses this daemon shape: `main::run_serve` builds a `Daemon`
//! via [`Daemon::new_pre_wired`] with the one project pre-resolved
//! and inserted into the map. Tool handlers still call
//! `resolve_or_wire(working_directory)`; the LLM in single-project
//! mode passes its CWD which walks up to that project's
//! `schema.toml`. One code path serves both deployment modes.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::sync::{Mutex, RwLock};

use crate::adapters::project_identity::{ProjectId, ProjectIdentity};
use crate::adapters::toml_config::SchemaConfig;
use crate::app::project_instance::{ProjectInstance, spawn_project_watcher};
use crate::ports::{Embedder, LlmProvider};

/// Per-workstation daemon. See module docs.
pub struct Daemon {
    /// Shared `Embedder` (one ONNX session for the whole daemon).
    pub embedder: Arc<Mutex<dyn Embedder>>,
    /// Optional shared LLM provider.
    pub llm_provider: Option<Arc<dyn LlmProvider>>,
    /// Lazy cache of resolved projects keyed by `project_id`. `Arc`
    /// on the value side so handlers can borrow a `ProjectInstance`
    /// out of the read lock and drop the lock before doing actual
    /// work (the use cases are themselves `Clone`-via-`Arc` so the
    /// caller pays no copy cost).
    projects: Arc<RwLock<HashMap<ProjectId, Arc<ProjectInstance>>>>,
}

impl Daemon {
    /// Empty daemon. Used by `schema daemon` (ADR-0027) — the map
    /// fills in lazily on the first project-scoped tool call.
    #[must_use]
    pub fn new(
        embedder: Arc<Mutex<dyn Embedder>>,
        llm_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self {
            embedder,
            llm_provider,
            projects: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Daemon pre-wired with one project. Used by
    /// `schema serve --config schema.toml` (ADR-0019) so the
    /// single-project deployment shares the same dispatch path.
    #[must_use]
    pub fn new_pre_wired(
        embedder: Arc<Mutex<dyn Embedder>>,
        llm_provider: Option<Arc<dyn LlmProvider>>,
        project: ProjectInstance,
    ) -> Self {
        let mut map = HashMap::with_capacity(1);
        map.insert(project.identity.id.clone(), Arc::new(project));
        Self {
            embedder,
            llm_provider,
            projects: Arc::new(RwLock::new(map)),
        }
    }

    /// Resolve `working_directory` to a wired [`ProjectInstance`].
    ///
    /// Walks up from `working_directory` looking for a
    /// `schema.toml`. On hit, resolves the project identity per
    /// ADR-0008 and returns the cached instance, wiring a new one
    /// if absent. On miss (no `schema.toml` found up to the
    /// filesystem root), returns a structured error.
    ///
    /// # Errors
    /// Returns an error when:
    /// - No `schema.toml` is found walking up from `working_directory`.
    /// - The found `schema.toml` cannot be loaded
    ///   ([`SchemaConfig::resolve`] failure).
    /// - The project identity cannot be resolved.
    /// - The persistence store cannot be opened on first wire.
    pub async fn resolve_or_wire(&self, working_directory: &Path) -> Result<Arc<ProjectInstance>> {
        let schema_toml = walk_up_for_schema_toml(working_directory).ok_or_else(|| {
            anyhow!(
                "no `schema.toml` found walking up from {}; create one at the project root before \
                 querying",
                working_directory.display(),
            )
        })?;
        let (config, resolved_path) = SchemaConfig::resolve(Some(&schema_toml))
            .with_context(|| format!("loading {}", schema_toml.display()))?;
        let identity = ProjectIdentity::resolve(
            &config.project.name,
            &SchemaConfig::project_root(&resolved_path)?,
        )
        .with_context(|| format!("resolving identity for {}", schema_toml.display()))?;

        if let Some(existing) = self.lookup(&identity.id).await {
            return Ok(existing);
        }
        self.wire_and_insert(config, identity).await
    }

    async fn lookup(&self, id: &ProjectId) -> Option<Arc<ProjectInstance>> {
        let guard = self.projects.read().await;
        guard.get(id).cloned()
    }

    async fn wire_and_insert(
        &self,
        config: SchemaConfig,
        identity: ProjectIdentity,
    ) -> Result<Arc<ProjectInstance>> {
        identity.ensure_cache_dir()?;
        let instance = ProjectInstance::wire(
            config,
            identity.clone(),
            &self.embedder,
            self.llm_provider.clone(),
        )
        .await?;
        // Initial delta-sync inline so the warm cache is in place by
        // the time the tool call's response goes out (ADR-0027 §"Open
        // questions" — initial-sync timeout). Cheap on warm restart
        // per ADR-0017's `mtime + size` short-circuit; slow only on a
        // truly fresh project.
        let initial = instance.sync.clone().run().await?;
        tracing::info!(
            project = %instance.identity.id,
            ?initial,
            "daemon: initial delta-sync complete",
        );
        // ADR-0010 watcher recovery for daemon mode (ADR-0027 PR 4/4
        // Evidence). Without this the lazy-wired projects would not
        // pick up edits while the daemon is running — only a daemon
        // restart would refresh them.
        spawn_project_watcher(&instance)?;

        let arc = Arc::new(instance);
        // Double-check pattern: another concurrent request may have
        // wired the same project between our `lookup` read-lock
        // dropping and this write-lock acquisition. Honour the
        // existing entry — wiring is idempotent on the persisted
        // `store.db`, but the existing `Arc` is the canonical one.
        let mut guard = self.projects.write().await;
        if let Some(existing) = guard.get(&identity.id) {
            let canonical = Arc::clone(existing);
            drop(guard);
            return Ok(canonical);
        }
        guard.insert(identity.id, Arc::clone(&arc));
        drop(guard);
        Ok(arc)
    }

    /// Number of currently wired projects. Diagnostics only.
    pub async fn len(&self) -> usize {
        self.projects.read().await.len()
    }

    /// Whether the daemon has zero wired projects.
    pub async fn is_empty(&self) -> bool {
        self.projects.read().await.is_empty()
    }
}

impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("llm_provider_enabled", &self.llm_provider.is_some())
            .finish_non_exhaustive()
    }
}

/// Walk up from `start` looking for a `schema.toml`. Returns the
/// path to the file on hit, `None` on miss (filesystem root reached
/// without finding one).
fn walk_up_for_schema_toml(start: &Path) -> Option<PathBuf> {
    let canonical = start.canonicalize().ok()?;
    let mut current = canonical.as_path();
    loop {
        let candidate = current.join("schema.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use super::Daemon;
    use crate::ports::{EmbedError, Embedder};

    fn empty_embedder() -> Arc<Mutex<dyn Embedder>> {
        Arc::new(Mutex::new(MuteEmbedder))
    }

    struct MuteEmbedder;

    #[async_trait]
    impl Embedder for MuteEmbedder {
        async fn embed(&mut self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn empty_daemon_reports_zero_projects() {
        let daemon = Daemon::new(empty_embedder(), None);
        assert_eq!(daemon.len().await, 0);
        assert!(daemon.is_empty().await);
    }

    #[tokio::test]
    async fn resolve_errors_when_no_schema_toml_walks_up() {
        let dir = TempDir::new().unwrap();
        let daemon = Daemon::new(empty_embedder(), None);
        let err = daemon.resolve_or_wire(dir.path()).await.unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("no `schema.toml` found"),
            "expected walk-up miss message, got: {message}",
        );
    }

    #[tokio::test]
    async fn resolve_errors_on_missing_directory() {
        use std::path::Path;
        let daemon = Daemon::new(empty_embedder(), None);
        let err = daemon
            .resolve_or_wire(Path::new("/definitely/does/not/exist"))
            .await
            .unwrap_err();
        // `canonicalize` fails before walk-up runs; the error
        // surfaces through the `ok_or_else` branch.
        let message = format!("{err}");
        assert!(
            message.contains("no `schema.toml` found"),
            "expected walk-up miss message even on missing dir, got: {message}",
        );
    }
}
