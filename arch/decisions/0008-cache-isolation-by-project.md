---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0008 — Per-project cache isolation in `~/.cache/schema/`

> **Y-statement** — In the context of ADR-0003 making `schema`
> multi-project (one binary serves any number of consumer repos),
> facing the question of **where per-project state lives** (LanceDB
> store, delta-sync manifest, optional lock file) — given the risks
> that two projects with the same name in different paths must not
> share a cache, two daemons on the same project must read
> consistently while one writes, and renaming/moving a project
> should not corrupt the previous index — we decided for **isolation
> at `~/.cache/schema/projects/<project_id>/`** where `project_id =
> sanitise(name) + "-" + blake3(canonical_absolute_path)[:16hex]`,
> against (a) **per-project state inside the consumer repo** (would
> bloat repos and conflict with `.gitignore`), (b) **a shared
> namespace with `project_name` discriminator** (collisions when two
> projects share a name), or (c) **a global LanceDB instance with a
> `project_id` column** (cross-project reads possible by accident),
> to achieve a layout where every project sees only its own state
> and where the same project at the same path always resolves to
> the same cache directory, accepting that renaming or moving a
> consumer repo produces a fresh cache directory (one-time
> re-index) and that the OS cache base dir (`dirs::cache_dir()`)
> determines portability across platforms.

## Context and Problem Statement

Per ADR-0003 the binary is multi-project. Per ADR-0006 each project
has a LanceDB instance. Per ADR-0007 each project has a manifest.
Where does this state live?

Three concerns:

1. **Collision** — two projects with the same name (e.g. two
   different repos called `docs`) must not write to the same cache.
2. **Concurrency** — two `schema` instances on the same project
   (operator opens Claude Code in two terminals) must coordinate.
3. **Portability** — moving/renaming a project should not corrupt
   anything; ideally produce a fresh cache or follow the rename.

## Decision Drivers

- **Strong isolation.** Cross-project leakage is an information
  hazard (private docs from one project surfacing in another's
  retrieval).
- **Stable identity.** Same project at same path → same cache.
  Multiple sessions reuse the work. Renaming = re-do is acceptable;
  it should not corrupt anything.
- **OS conventions.** `~/.cache/` on Linux/macOS, `%LOCALAPPDATA%`
  on Windows. The `dirs` crate's `cache_dir()` returns the right
  path per platform.

## Considered Options

### Option A — state inside the consumer repo (rejected)

`<repo>/.schema-cache/lance/`. Pro: trivial isolation. Con: bloats
repos with binary blobs; `.gitignore` discipline required from every
consumer; backups balloon. Operators dislike "tools writing into my
repo".

### Option B — shared namespace by `project_name` only (rejected)

`~/.cache/schema/lowcow-platform/lance/`. Pro: short, readable.
Con: two repos named `lowcow-platform` (one in `~/dev/`, one in
`~/work/`) collide. Operationally we may never hit this, but the
risk is asymmetric: collision = data corruption.

### Option C — global LanceDB with `project_id` discriminator (rejected)

One LanceDB instance with a `project_id` column on every chunk;
queries filter by project. Pro: one set of files. Con: a missing
filter on any code path leaks one project's chunks into another's
results. Information-hazard surface > zero.

### Option D — `~/.cache/schema/projects/<project_id>/` (chosen)

`project_id = sanitise(name) + "-" + blake3(canonical_path)[:16hex]`.

Example for `~/dev/lowcow-platform`:
`~/.cache/schema/projects/lowcow-platform-a3f9c2d1/`.

Pro: collision-proof (path hash distinguishes same-name projects).
Pro: stable while the path is stable. Pro: each project sees only
its own dir; no cross-project leakage possible by code path.

Con: rename or move the project = new `project_id` = re-index.
Acceptable; it's a rare event and the rebuild is bounded.

## Decision Outcome

Chosen option: **D — per-project directory keyed by `(name,
canonical-path-hash)`**.

### Layout

```text
~/.cache/schema/                  # cache_root() — global
├── models/
│   └── bge-m3.onnx              # ADR-0005, shared across projects
└── projects/
    ├── lowcow-platform-a3f9c2d1/
    │   ├── lance/               # LanceDB store
    │   ├── metadata.toml        # delta-sync manifest
    │   └── lock                 # advisory lock (FASE 1.1)
    ├── lowcow-site-b2e7d501/
    │   └── ...
    └── ...
```

### Identity computation

```rust
// src/config/project.rs
let canonical = project_root.canonicalize()?;     // resolves symlinks
let hash = blake3::hash(canonical.as_os_str().as_encoded_bytes());
let prefix = hash.to_hex().chars().take(16).collect::<String>();
let sanitised_name = sanitise(name);              // [a-z0-9-] only
let project_id = format!("{sanitised_name}-{prefix}");
```

### Concurrency

- LanceDB MVCC handles multi-reader-single-writer at the store
  level.
- A future `lock` file (FASE 1.1) wraps `fd_lock::RwLock` around
  delta-sync write phases so two daemons can't trip over each
  other during a re-index. Read-only queries don't take the lock.

### Renames / moves

Project moved → `canonicalize()` returns a different path → new
`project_id` → fresh cache. The previous cache is orphaned. A
future `schema gc --orphans` (FASE 1.1) cleans them up.

## Consequences

- **Good:** strong cross-project isolation; no leakage possible
  by code path.
- **Good:** moving the repo doesn't corrupt the previous cache;
  worst case is a one-off re-index (~30-60 s per project at
  Lowcow-platform scale).
- **Good:** `dirs::cache_dir()` is portable; works the same on
  macOS / Linux (Windows would need a smoke test before claiming
  support).
- **Bad:** orphan caches accumulate when projects are moved/
  deleted. Mitigated by `schema gc` (FASE 1.1).
- **Bad:** symlinked repos have an interesting failure mode —
  `canonicalize` resolves the symlink, so caching follows the
  real path, not the symlink. Documented in the runbook.

## Fitness function

- `project_id_is_deterministic` and `project_id_changes_with_path`
  unit tests in `src/config/project.rs::tests` exercise the
  identity invariants.
- `sanitises_unicode_and_special_chars` ensures the directory
  name stays filesystem-safe.

## More information

- `src/adapters/project_identity.rs` — `ProjectId`, `ProjectIdentity`
  (was `src/config/project.rs` before ADR-0013 hex restructure).
- ADR-0003 — multi-project architecture (the why).
- ADR-0005 — model cache lives outside per-project state.
- ADR-0006 — LanceDB store shape (superseded by ADR-0011).
- ADR-0007 — manifest lives in the same per-project dir.
- ADR-0011 — replaces LanceDB with SQLite + `sqlite-vec`;
  cache layout updated (see Evidence amendment 2026-04-25).
- ADR-0013 — hexagonal restructure moved `ProjectIdentity`
  from `src/config/` into `src/adapters/`.

## Evidence and amendments

- _2026-04-25 — Cache layout amended by ADR-0011 implementation.
  The per-project store changed from a directory `lance/`
  (LanceDB) to a single file `store.db` (SQLite +
  `sqlite-vec`), with WAL companions `store.db-wal` and
  `store.db-shm`. The diagram in **Resulting structure**
  remains correct in shape (per-project cache directory under
  `~/.cache/schema/projects/<id>/`); only the inner artifact
  changed from a directory to a file. Cleanup, copy, and
  inspection are now filesystem operations on a single file.
  Legacy `lance/` directories from prior versions are detected
  and reported via `tracing::warn!` on first `schema serve`
  (ADR-0011 Follow-ups: manual migration policy). The
  `ProjectIdentity` field was renamed `lance_dir` → `store_path`._
- _2026-04-25 — Source location moved by ADR-0013. The struct
  now lives at `src/adapters/project_identity.rs` (was
  `src/config/project.rs`); the cache-path resolution is an
  adapter concern under the new hex layout._
