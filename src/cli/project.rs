//! `schema project register / unregister / list` verbs (ADR-0026 slice 3).
//!
//! These verbs are the operator interface to the daemon's project
//! [`crate::adapters::registry_toml::Registry`]. Per ADR-0026 §"Decision",
//! the shared daemon does **not** infer project membership from `--config`
//! flags on its unit's `ExecStart`; the operator declares membership
//! explicitly through these verbs.
//!
//! All three verbs are filesystem-only — they do **not** dispatch a
//! request to a running daemon. The daemon picks up registry changes by
//! reloading [`crate::adapters::registry_toml::Registry`] on startup; a
//! follow-up slice will add a localhost `POST /admin/projects/refresh`
//! call so changes apply hot. For now, restart the daemon (or rely on
//! launchd / systemd `KeepAlive`) after a register / unregister.
//!
//! No dependency on the daemon's runtime makes the verbs trivially
//! testable: each one operates on an injected `registry_path: &Path`
//! (overridden in tests, defaults to
//! [`crate::adapters::registry_toml::Registry::default_path`] at the
//! top-level CLI dispatch in `main.rs`).

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::adapters::project_identity::{ProjectId, ProjectIdentity};
use crate::adapters::registry_toml::{ProjectEntry, Registry};
use crate::adapters::toml_config::SchemaConfig;

/// `schema project register --config <path>`.
///
/// Resolves the project's identity from the supplied `schema.toml`,
/// upserts an entry in the registry at `registry_path`, and prints a
/// one-line confirmation. Re-registering an existing project rotates
/// the `registered_at` timestamp and preserves the ordering slot.
///
/// # Errors
/// Returns an error if the config cannot be loaded, identity cannot be
/// resolved, or the registry cannot be loaded / saved.
pub fn register(schema_toml: &Path, registry_path: &Path) -> Result<()> {
    let identity = resolve_identity(schema_toml)?;
    let absolute = canonicalise_schema_toml(schema_toml)?;

    let mut registry = load_registry(registry_path)?;
    let entry = ProjectEntry {
        project_id: identity.id.to_string(),
        name: identity.name,
        schema_toml_path: absolute.to_string_lossy().into_owned(),
        registered_at: Utc::now().to_rfc3339(),
    };
    let was_replace = registry.upsert(entry);
    save_registry(&registry, registry_path)?;

    print_register_summary(
        &identity.id,
        &absolute,
        registry_path,
        registry.len(),
        was_replace,
    )
}

fn print_register_summary(
    project_id: &ProjectId,
    schema_toml_absolute: &Path,
    registry_path: &Path,
    total: usize,
    was_replace: bool,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let verb = if was_replace {
        "re-registered"
    } else {
        "registered"
    };
    let count_change = if was_replace {
        "unchanged"
    } else {
        "incremented"
    };
    writeln!(
        out,
        "{verb} project {project_id} ({})",
        schema_toml_absolute.display()
    )?;
    writeln!(out, "  registry: {}", registry_path.display())?;
    writeln!(
        out,
        "  total registered: {total} ({count_change} after this change)"
    )?;
    Ok(())
}

/// `schema project unregister --project-id <id>`.
///
/// Removes the entry from the registry. Returns a non-fatal warning
/// (still exit 0) if the id was not present, since `unregister` is
/// idempotent from the operator's perspective.
///
/// # Errors
/// Returns an error if the registry cannot be loaded or saved. Missing
/// entry is **not** an error — see the rationale above.
pub fn unregister(project_id: &str, registry_path: &Path) -> Result<()> {
    let mut registry = load_registry(registry_path)?;
    let removed = registry.remove(project_id);
    save_registry(&registry, registry_path)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if let Some(entry) = removed {
        writeln!(
            out,
            "unregistered project {} ({})",
            entry.project_id, entry.name
        )?;
    } else {
        writeln!(
            out,
            "no entry for project_id {project_id}; registry left unchanged"
        )?;
    }
    let suffix = if registry.len() == 1 { "" } else { "s" };
    writeln!(
        out,
        "  registry: {} ({} entry{suffix} remain)",
        registry_path.display(),
        registry.len(),
    )?;
    Ok(())
}

/// `schema project list`.
///
/// Prints every registered project as one row per entry. Output is
/// stable across calls (registry preserves insertion order).
///
/// # Errors
/// Returns an error if the registry cannot be loaded.
pub fn list(registry_path: &Path) -> Result<()> {
    let registry = load_registry(registry_path)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if registry.is_empty() {
        writeln!(out, "no projects registered ({})", registry_path.display())?;
        return Ok(());
    }

    let suffix = if registry.len() == 1 { "" } else { "s" };
    writeln!(
        out,
        "{} project{suffix} registered ({})",
        registry.len(),
        registry_path.display(),
    )?;
    for entry in &registry.projects {
        write_entry(&mut out, entry)?;
    }
    Ok(())
}

fn write_entry<W: Write>(out: &mut W, entry: &ProjectEntry) -> Result<()> {
    writeln!(out)?;
    writeln!(out, "- project_id  : {}", entry.project_id)?;
    writeln!(out, "  name        : {}", entry.name)?;
    writeln!(out, "  schema.toml : {}", entry.schema_toml_path)?;
    writeln!(out, "  registered  : {}", entry.registered_at)?;
    Ok(())
}

// ─── Internals ───────────────────────────────────────────────────────────────

/// Project metadata extracted from the consumer's `schema.toml`. Kept in
/// the module so the integration with `SchemaConfig` / `ProjectIdentity`
/// lives in one place.
struct ResolvedIdentity {
    id: ProjectId,
    name: String,
}

impl fmt::Display for ResolvedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)
    }
}

fn resolve_identity(schema_toml: &Path) -> Result<ResolvedIdentity> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(schema_toml))
        .with_context(|| format!("loading {}", schema_toml.display()))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;
    Ok(ResolvedIdentity {
        id: identity.id,
        name: cfg.project.name,
    })
}

fn canonicalise_schema_toml(schema_toml: &Path) -> Result<PathBuf> {
    schema_toml
        .canonicalize()
        .with_context(|| format!("canonicalising {}", schema_toml.display()))
}

fn load_registry(registry_path: &Path) -> Result<Registry> {
    Registry::load(registry_path).with_context(|| {
        format!(
            "loading registry at {} (consider deleting if corrupt)",
            registry_path.display()
        )
    })
}

fn save_registry(registry: &Registry, registry_path: &Path) -> Result<()> {
    registry
        .save_atomic(registry_path)
        .with_context(|| format!("saving registry to {}", registry_path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::fs;
    use std::path::Path as StdPath;

    use tempfile::TempDir;

    use super::{Registry, list, register, unregister};

    /// Build a minimal consumer project tree (one `schema.toml` + one ADR
    /// dir) so `SchemaConfig::resolve` and `ProjectIdentity::resolve`
    /// both succeed against it.
    fn fixture_project(root: &StdPath, name: &str) {
        fs::create_dir_all(root.join("docs/decisions")).unwrap();
        fs::write(
            root.join("docs/decisions/0001-x.md"),
            "---\nstatus: accepted\n---\n# 0001 — x\n",
        )
        .unwrap();
        fs::write(
            root.join("schema.toml"),
            format!(
                "[project]\nname = \"{name}\"\nversion = \"1\"\n\n[[corpus]]\npath = \"docs/decisions\"\nkind = \"adr-madr\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn register_creates_registry_with_one_entry() {
        let project_dir = TempDir::new().unwrap();
        fixture_project(project_dir.path(), "alpha");
        let schema_toml = project_dir.path().join("schema.toml");

        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");

        register(&schema_toml, &registry_path).unwrap();

        let registry = Registry::load(&registry_path).unwrap();
        assert_eq!(registry.len(), 1);
        let entry = &registry.projects[0];
        assert_eq!(entry.name, "alpha");
        assert!(entry.project_id.starts_with("alpha-"));
        assert!(entry.registered_at.contains('T'));
        assert!(entry.schema_toml_path.ends_with("schema.toml"));
    }

    #[test]
    fn re_registering_replaces_in_place_and_keeps_count() {
        let project_dir = TempDir::new().unwrap();
        fixture_project(project_dir.path(), "alpha");
        let schema_toml = project_dir.path().join("schema.toml");

        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");

        register(&schema_toml, &registry_path).unwrap();
        let first = Registry::load(&registry_path).unwrap();
        let first_ts = first.projects[0].registered_at.clone();

        // Sleep-free deterministic re-register: timestamps may match if
        // the clock granularity is coarse. The contract is "in-place
        // replace, count unchanged" — assert that, not the timestamp.
        register(&schema_toml, &registry_path).unwrap();
        let second = Registry::load(&registry_path).unwrap();

        assert_eq!(second.len(), 1, "re-register must not append");
        assert_eq!(
            second.projects[0].project_id, first.projects[0].project_id,
            "project_id must match across re-registers of the same root"
        );
        // The timestamp may or may not advance depending on clock
        // resolution; assert it's well-formed and present in both.
        assert!(!first_ts.is_empty());
        assert!(!second.projects[0].registered_at.is_empty());
    }

    #[test]
    fn registering_two_projects_appends_in_order() {
        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");

        let alpha_dir = TempDir::new().unwrap();
        fixture_project(alpha_dir.path(), "alpha");
        let beta_dir = TempDir::new().unwrap();
        fixture_project(beta_dir.path(), "beta");

        register(&alpha_dir.path().join("schema.toml"), &registry_path).unwrap();
        register(&beta_dir.path().join("schema.toml"), &registry_path).unwrap();

        let registry = Registry::load(&registry_path).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.projects[0].name, "alpha");
        assert_eq!(registry.projects[1].name, "beta");
    }

    #[test]
    fn unregister_removes_entry_and_persists_change() {
        let project_dir = TempDir::new().unwrap();
        fixture_project(project_dir.path(), "alpha");
        let schema_toml = project_dir.path().join("schema.toml");

        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");

        register(&schema_toml, &registry_path).unwrap();
        let registered = Registry::load(&registry_path).unwrap();
        let project_id = registered.projects[0].project_id.clone();

        unregister(&project_id, &registry_path).unwrap();
        let after = Registry::load(&registry_path).unwrap();
        assert!(after.is_empty());
    }

    #[test]
    fn unregister_unknown_id_is_idempotent() {
        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");

        // Even on a missing-file registry, unregister must succeed (no-op).
        unregister("never-existed", &registry_path).unwrap();
        // The save_atomic path created the registry file (empty).
        let registry = Registry::load(&registry_path).unwrap();
        assert!(registry.is_empty());
    }

    #[test]
    fn list_on_missing_registry_does_not_error() {
        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");
        list(&registry_path).unwrap();
    }

    #[test]
    fn list_after_two_registers_walks_in_order() {
        let registry_dir = TempDir::new().unwrap();
        let registry_path = registry_dir.path().join("registry.toml");

        let alpha_dir = TempDir::new().unwrap();
        fixture_project(alpha_dir.path(), "alpha");
        let beta_dir = TempDir::new().unwrap();
        fixture_project(beta_dir.path(), "beta");

        register(&alpha_dir.path().join("schema.toml"), &registry_path).unwrap();
        register(&beta_dir.path().join("schema.toml"), &registry_path).unwrap();

        // No assertion on stdout content here — `list` writes through
        // the locked stdout handle, not capturable from the test
        // process easily without redirection plumbing. Round-trip via
        // `Registry::load` already covers the data side; this test
        // just confirms `list` exits 0 against the persisted state.
        list(&registry_path).unwrap();
    }
}
