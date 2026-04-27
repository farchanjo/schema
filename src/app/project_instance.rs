//! Per-project runtime bundle for the shared daemon (ADR-0026).
//!
//! Replaces the FASE 1.0 pair `Wiring + Services` (private to
//! `main.rs`) with a single, named, file-resident concept. A
//! [`ProjectInstance`] owns the per-project ports
//! (`Persistence`, `MetadataStore`, `Walker`, `Chunker`) and the
//! per-project app-layer use cases (`DeltaSync`, `Query`,
//! `Cleanup`, `Synthesize`) for **one** registered project.
//!
//! The `Embedder` is **not** owned by `ProjectInstance` — it is
//! supplied by the caller. ADR-0026 §"Decision drivers" requires
//! one `Embedder` total per workstation (BGE-M3 ONNX baseline
//! ~1.7 GB), which the shared daemon constructs once and `Arc`-clones
//! to every project. Single-project [`crate::main`] builds an
//! `Embedder` and passes it in the same way, so the wiring path is
//! uniform across the per-project (ADR-0019) and shared-daemon
//! (ADR-0026) deployments — what changes is **how many** projects
//! share each `Embedder`.
//!
//! `LlmProvider` is also caller-supplied (`Option<Arc<dyn
//! LlmProvider>>`). The shared daemon resolves the provider once at
//! startup; per-project paths in single-project mode resolve from
//! their own `[llm]` config block. ADR-0025 selection rules are
//! orthogonal to ADR-0026 and stay in the composition root.
//!
//! # Hexagonal placement
//!
//! `ProjectInstance` is **application-layer**, not domain — it
//! aggregates ports (interfaces declared by domain/application
//! boundaries) into a runtime value. The composition root in
//! `main.rs` (single-project) and the eventual `Daemon` (slice 4,
//! shared) are the only places that construct it; everything else
//! consumes it through the use cases it exposes.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::adapters::filesystem::WalkdirWalker;
use crate::adapters::markdown_chunker::MarkdownChunker;
use crate::adapters::metadata_store::TomlMetadataStore;
use crate::adapters::project_identity::ProjectIdentity;
use crate::adapters::sqlite_vec_store::{SqliteVecStore, migrate_legacy_lance_dir};
use crate::adapters::toml_config::SchemaConfig;
use crate::app::cleanup::Cleanup;
use crate::app::delta_sync::DeltaSync;
use crate::app::query::Query;
use crate::app::synthesize::Synthesize;
use crate::ports::{Chunker, Embedder, LlmProvider, MetadataStore, Persistence, Walker};

/// One registered project's complete runtime state.
///
/// Every field is public so the composition root can borrow them
/// piecewise into the MCP `ServerState`; future internal-only knobs
/// should still be added through accessor methods to leave the public
/// surface flat.
pub struct ProjectInstance {
    pub identity: ProjectIdentity,
    pub config: SchemaConfig,
    pub persistence: Arc<dyn Persistence>,
    pub metadata: Arc<dyn MetadataStore>,
    pub walker: Arc<dyn Walker>,
    pub chunker: Arc<dyn Chunker>,
    pub sync: DeltaSync,
    pub query: Query,
    pub cleanup: Cleanup,
    /// `Some` when an LLM provider was configured (ADR-0025); `None`
    /// puts the `synthesize` MCP tool in the disabled-mode branch.
    pub synthesize: Option<Synthesize>,
}

impl ProjectInstance {
    /// Wire one project against caller-supplied `embedder` and
    /// `llm_provider`.
    ///
    /// Sharing the embedder across projects is the point of ADR-0026;
    /// sharing the LLM provider is incidental (one provider per
    /// workstation is the common case).
    ///
    /// This factory does **not** run the initial delta-sync, write
    /// `endpoint.toml`, or spawn the watcher. Those side effects
    /// happen in the composition root after the instance is wired
    /// (so a `Daemon` can run them in parallel across projects, or
    /// fan them out behind a registration progress UI).
    ///
    /// # Errors
    /// Returns an error if the persistence store cannot be opened or
    /// initialised. Walker / chunker / metadata wiring is infallible.
    pub async fn wire(
        config: SchemaConfig,
        identity: ProjectIdentity,
        embedder: &Arc<Mutex<dyn Embedder>>,
        llm_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Result<Self> {
        let ports = WiredPorts::open(&config, &identity).await?;
        let use_cases = WiredUseCases::build(&config, &ports, embedder, llm_provider);
        Ok(Self::assemble(config, identity, ports, use_cases))
    }

    fn assemble(
        config: SchemaConfig,
        identity: ProjectIdentity,
        ports: WiredPorts,
        use_cases: WiredUseCases,
    ) -> Self {
        Self {
            identity,
            config,
            persistence: ports.persistence,
            metadata: ports.metadata,
            walker: ports.walker,
            chunker: ports.chunker,
            sync: use_cases.sync,
            query: use_cases.query,
            cleanup: use_cases.cleanup,
            synthesize: use_cases.synthesize,
        }
    }
}

impl fmt::Debug for ProjectInstance {
    /// Manual `Debug` impl — the field types include `Arc<dyn Trait>`
    /// objects whose trait does not require `Debug`, so a `derive`
    /// would not type-check. Surface the identity and a
    /// has-LLM-provider flag, which is the bulk of what a
    /// `tracing::debug!` call wants to log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectInstance")
            .field("identity", &self.identity.id)
            .field("project_root", &self.identity.root.display())
            .field("synthesize_enabled", &self.synthesize.is_some())
            .finish_non_exhaustive()
    }
}

/// Per-project ports (the "outbound" half of hexagonal). Built once
/// in [`WiredPorts::open`] and consumed by [`WiredUseCases::build`].
struct WiredPorts {
    persistence: Arc<dyn Persistence>,
    metadata: Arc<dyn MetadataStore>,
    walker: Arc<dyn Walker>,
    chunker: Arc<dyn Chunker>,
}

impl WiredPorts {
    async fn open(config: &SchemaConfig, identity: &ProjectIdentity) -> Result<Self> {
        migrate_legacy_lance_dir(&identity.cache_dir);
        let persistence: Arc<dyn Persistence> =
            Arc::new(SqliteVecStore::open(&identity.store_path).await?);
        persistence.ensure_ready().await?;

        let cfg_arc = Arc::new(config.clone());
        let walker: Arc<dyn Walker> = Arc::new(WalkdirWalker::new(cfg_arc, identity.root.clone()));
        let chunker: Arc<dyn Chunker> =
            Arc::new(MarkdownChunker::new(config.retrieval.chunk_size_max));
        let metadata: Arc<dyn MetadataStore> =
            Arc::new(TomlMetadataStore::new(identity.metadata_path.clone()));

        Ok(Self {
            persistence,
            metadata,
            walker,
            chunker,
        })
    }
}

/// Per-project app-layer use cases (the inbound side of the
/// composition).  All four use cases share the per-project ports;
/// `Embedder` and the optional `LlmProvider` are caller-supplied so
/// they can be shared across [`ProjectInstance`]s in a daemon.
struct WiredUseCases {
    sync: DeltaSync,
    query: Query,
    cleanup: Cleanup,
    synthesize: Option<Synthesize>,
}

impl WiredUseCases {
    fn build(
        config: &SchemaConfig,
        ports: &WiredPorts,
        embedder: &Arc<Mutex<dyn Embedder>>,
        llm_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        let sync = DeltaSync::new(
            Arc::clone(&ports.persistence),
            Arc::clone(embedder),
            Arc::clone(&ports.walker),
            Arc::clone(&ports.chunker),
            Arc::clone(&ports.metadata),
        );
        let query = Query::new(Arc::clone(&ports.persistence), Arc::clone(embedder));
        let cleanup = Cleanup::new(Arc::clone(&ports.persistence), Arc::clone(&ports.metadata));
        let synthesize = llm_provider.map(|provider| {
            Synthesize::new(
                Arc::clone(&ports.persistence),
                Arc::clone(embedder),
                provider,
                config.llm.max_tokens,
                config.llm.temperature,
            )
        });
        Self {
            sync,
            query,
            cleanup,
            synthesize,
        }
    }
}
