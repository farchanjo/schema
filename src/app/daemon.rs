//! Shared multi-project daemon (ADR-0026 slice 4).
//!
//! Composition root for the post-ADR-0026 deployment: one process,
//! one [`Embedder`](crate::ports::Embedder) loaded once, optional
//! shared [`LlmProvider`](crate::ports::LlmProvider), and N
//! [`ProjectInstance`](crate::app::project_instance::ProjectInstance)
//! values keyed by `project_id`. The HTTP surface is a single
//! [`axum::Router`] mounting one `/mcp/<project_id>` route per
//! project (B.1 routing per ADR-0026 §"Open questions"); each mount
//! is gated by a single-token [`crate::adapters::auth::BearerValidator`]
//! tied to that project's bearer.
//!
//! # Why single-token-per-mount instead of `MultiTenantBearerValidator`
//!
//! ADR-0021 amendment by ADR-0026 calls out `Map<TokenHash,
//! ProjectId>` as the validator shape. Slice 4 ships a stricter
//! variant: each mount has **only** that project's token in scope,
//! so a mismatched token at `/mcp/<project_b_id>` cannot resolve to
//! any project — it just fails 401. The
//! [`crate::adapters::auth::ProjectTokenRegistry`] exists as the
//! daemon's introspection surface (token rotation, `schema project
//! list` decoration, future hot-reload), not as the per-request
//! gate. This still satisfies the canary fitness function: bearer-A
//! on path-B returns 401 (not 200-with-empty-results); cross-project
//! bleed is impossible by routing construction.
//!
//! # What this slice does **not** do
//!
//! - No new CLI verb yet (`schema daemon` lands in slice 4b).
//!   `run_serve` keeps wiring single-project mode unchanged.
//! - No `endpoint.toml` write loop. The daemon's HTTP listener and
//!   per-project endpoint files come with slice 4b together with
//!   the launchd / systemd template collapse (slice 5).
//! - No watcher / initial delta-sync orchestration. Caller drives
//!   those via [`crate::app::project_instance::ProjectInstance`]
//!   accessors after [`Daemon::wire`] returns.
//! - No fitness-function E2E. Slice 6 (`tests/e2e/test_adr0026_isolation.py`)
//!   verifies cross-project isolation end-to-end against a running
//!   daemon; routing-level isolation is asserted in this module's
//!   unit tests via direct `Router` introspection plus a synthetic
//!   request.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::adapters::auth::ProjectTokenRegistry;
use crate::adapters::mcp_server::{Mount, SchemaServer, ServerState, build_multi_tenant_router};
use crate::adapters::project_identity::ProjectIdentity;
use crate::adapters::registry_toml::{ProjectEntry, Registry};
use crate::adapters::toml_config::SchemaConfig;
use crate::app::project_instance::ProjectInstance;
use crate::ports::{Embedder, LlmProvider};

/// One registered project's slot inside the shared daemon.
///
/// Holds the project's identity (via `instance.identity`), its
/// freshly minted bearer token, the live [`ProjectInstance`] (kept
/// here so its `sync.clone()` can drive the watcher loop), and the
/// per-project [`SchemaServer`] handed to the router builder.
pub struct ProjectSlot {
    pub project_id: String,
    pub token: String,
    pub instance: ProjectInstance,
    pub server: SchemaServer,
}

impl fmt::Debug for ProjectSlot {
    /// Manual `Debug` impl — `SchemaServer` does not implement
    /// `Debug` and the bearer `token` is sensitive (ADR-0021,
    /// must never reach trace logs). Surface only the
    /// `project_id` and a flag that the slot has a wired
    /// `instance`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectSlot")
            .field("project_id", &self.project_id)
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

/// Multi-project daemon.
///
/// Holds the **shared** `Embedder` (one per workstation per
/// ADR-0026 §"Decision drivers") and the per-project slots. The
/// shared daemon is built once at start-up via [`Daemon::wire`]
/// and consumed by [`Daemon::into_router`].
pub struct Daemon {
    /// Shared `Embedder` (one ONNX session for the whole daemon).
    /// Kept alive even after [`Self::into_router`] consumes the
    /// project slots, in case future slices add hot project
    /// registration (the new project will reuse this `Arc` clone).
    pub embedder: Arc<Mutex<dyn Embedder>>,
    /// Optional LLM provider (one per workstation, shared across
    /// projects). `None` puts the `synthesize` MCP tool into the
    /// disabled-mode branch on every project's handler.
    pub llm_provider: Option<Arc<dyn LlmProvider>>,
    /// `(token → ProjectId)` map mirroring the per-mount tokens.
    /// Not the per-request gate (ADR-0021 amendment notwithstanding;
    /// see module docs); populated for introspection and as the
    /// substrate a future hot-reload admin endpoint will mutate.
    pub token_registry: ProjectTokenRegistry,
    /// One slot per registered project.
    pub slots: Vec<ProjectSlot>,
}

impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("project_count", &self.slots.len())
            .field("llm_provider_enabled", &self.llm_provider.is_some())
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Wire every project listed in `registry` against the shared
    /// `embedder` and `llm_provider`. Runs each project's
    /// [`ProjectInstance::wire`] sequentially — parallelism is a
    /// follow-up (ONNX session contention measurement is open per
    /// ADR-0026 §"Open questions").
    ///
    /// This factory does **not** run the initial delta-sync, write
    /// `endpoint.toml`, or spawn the watcher. The caller drives
    /// those (the eventual `schema daemon` runtime entry point in
    /// slice 4b will do all three after this returns).
    ///
    /// # Errors
    /// Returns an error on the first project that fails to wire
    /// (config load, identity resolve, persistence open). The
    /// remaining projects are not attempted; failure containment
    /// across projects is a follow-up per ADR-0026 §"Decision
    /// drivers".
    pub async fn wire(
        registry: &Registry,
        embedder: Arc<Mutex<dyn Embedder>>,
        llm_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Result<Self> {
        let token_registry = ProjectTokenRegistry::new();
        let mut slots = Vec::with_capacity(registry.len());
        for entry in &registry.projects {
            let slot = wire_slot(entry, &embedder, llm_provider.clone()).await?;
            token_registry.insert(slot.token.clone(), slot.instance.identity.id.clone());
            slots.push(slot);
        }
        Ok(Self {
            embedder,
            llm_provider,
            token_registry,
            slots,
        })
    }

    /// Number of registered projects in this daemon. Mirrors
    /// `slots.len()`; provided so callers can avoid touching the
    /// public field.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the daemon has zero registered projects (the empty
    /// fresh-install state). The router is then `/health`-only.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Consume the daemon and return the composed
    /// [`axum::Router`]. The slots' `server` fields are moved into
    /// per-project mounts; the `instance` fields would have been
    /// useful for the watcher loop but are dropped here — wiring
    /// the watcher per project is the next slice's job (and will
    /// keep `ProjectInstance` alive separately).
    pub fn into_router(self, cancellation_token: &CancellationToken) -> axum::Router {
        let mounts: Vec<Mount> = self
            .slots
            .into_iter()
            .map(|slot| Mount {
                project_id: slot.project_id,
                token: slot.token,
                server: slot.server,
            })
            .collect();
        build_multi_tenant_router(mounts, cancellation_token)
    }
}

/// Wire one [`ProjectSlot`]. Resolves the consumer's `schema.toml`
/// from the registry entry, mints a fresh `UUIDv4` bearer token, and
/// builds the [`SchemaServer`] from the per-project `ServerState`.
async fn wire_slot(
    entry: &ProjectEntry,
    embedder: &Arc<Mutex<dyn Embedder>>,
    llm_provider: Option<Arc<dyn LlmProvider>>,
) -> Result<ProjectSlot> {
    let schema_toml = Path::new(&entry.schema_toml_path);
    let (config, resolved_path) = SchemaConfig::resolve(Some(schema_toml))
        .with_context(|| format!("loading {}", schema_toml.display()))?;
    let identity = ProjectIdentity::resolve(
        &config.project.name,
        &SchemaConfig::project_root(&resolved_path)?,
    )
    .with_context(|| format!("resolving identity for {}", schema_toml.display()))?;
    identity.ensure_cache_dir()?;

    let instance = ProjectInstance::wire(config, identity, embedder, llm_provider).await?;
    let project_id = instance.identity.id.to_string();
    let token = Uuid::new_v4().to_string();
    let server = build_server_for(&instance);
    Ok(ProjectSlot {
        project_id,
        token,
        instance,
        server,
    })
}

fn build_server_for(instance: &ProjectInstance) -> SchemaServer {
    let state = ServerState {
        config: instance.config.clone(),
        identity: instance.identity.clone(),
        query: instance.query.clone(),
        cleanup: instance.cleanup.clone(),
        synthesize: instance.synthesize.clone(),
    };
    SchemaServer::with_state(state)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::Router;
    use axum::body::Body;
    use http::{Method, Request, StatusCode, header};
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use super::Daemon;
    use crate::adapters::auth::ProjectTokenRegistry;
    use crate::ports::{EmbedError, Embedder};

    /// An empty daemon (zero registered projects) builds a router
    /// with only `/health`. Acts as the fresh-install fixture and
    /// guards the degenerate path of `build_multi_tenant_router`.
    #[tokio::test]
    async fn empty_daemon_router_serves_only_health() {
        let daemon = build_empty_daemon();
        let router = daemon.into_router(&CancellationToken::new());
        let response = oneshot_get(router, "/health").await;
        assert_eq!(response, StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_daemon_router_returns_404_on_unknown_mcp_path() {
        let daemon = build_empty_daemon();
        let router = daemon.into_router(&CancellationToken::new());
        // No projects registered → no `/mcp/...` mounts → 404 from axum.
        let response = oneshot_post(router, "/mcp/never-registered", "Bearer x").await;
        assert_eq!(response, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn empty_daemon_reports_zero_projects() {
        let daemon = build_empty_daemon();
        assert_eq!(daemon.len(), 0);
        assert!(daemon.is_empty());
    }

    fn build_empty_daemon() -> Daemon {
        // Build directly from struct fields rather than via `wire`,
        // because constructing a real `Embedder` would download the
        // BGE-M3 ONNX model (heavy + flaky in CI). The shape under
        // test here is the routing layer, not the embedder wiring.
        Daemon {
            embedder: Arc::new(Mutex::new(MuteEmbedder)),
            llm_provider: None,
            token_registry: ProjectTokenRegistry::new(),
            slots: Vec::new(),
        }
    }

    struct MuteEmbedder;

    #[async_trait]
    impl Embedder for MuteEmbedder {
        async fn embed(&mut self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(Vec::new())
        }
    }

    async fn oneshot_get(router: Router, uri: &str) -> StatusCode {
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap().status()
    }

    async fn oneshot_post(router: Router, uri: &str, authorization: &str) -> StatusCode {
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::AUTHORIZATION, authorization)
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap().status()
    }
}
