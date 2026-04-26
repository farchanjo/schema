---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0015 — Cleanup tools at both MCP and CLI surfaces

> **Y-statement** — In the context of the operator wanting to wipe a
> stale corpus or a single source path without manually `rm -rf`-ing
> `~/.cache/schema/projects/<id>/`, facing the choice between (a)
> CLI-only cleanup (safer — the LLM cannot accidentally trigger
> destructive ops), (b) MCP-tools-only cleanup (the LLM can self-heal
> but no off-session option exists), or (c) **both layers sharing one
> library function**, we decided for **(c)**, against (a) (no
> LLM-driven self-cleanup; the operator must drop the session) and
> (b) (no off-session cleanup; manual `rm` re-emerges), to achieve
> symmetric cleanup at both interaction surfaces, accepting that
> destructive operations exposed to the LLM require explicit operator
> confirmation discipline (encoded by `DESTRUCTIVE` prefix in the
> tool description and by an `--yes` flag on the CLI).

## Context and Problem Statement

ADR-0011 (SQLite + `sqlite-vec`) closed with **Cleanup tools (PR2)**
as a follow-up: surface `reset_all` and `forget_source` to operators
without requiring `rm -rf` on the cache directory. Two concrete
operator scenarios drove the work:

1. **Mid-session self-heal.** While Claude Code holds an MCP session
   open, the operator notices stale chunks (e.g. an ADR was edited
   but the watcher missed it; the embedder's chunker boundaries
   changed and old rows look duplicated). They want the LLM to drop
   one path or wipe the index, then re-index — without dropping the
   session and reopening Claude Code.
2. **Between-session maintenance.** The operator runs a one-shot
   `schema reset` from the terminal, e.g. before publishing a new
   release that ships a different chunker, or to recover from a
   disk-corruption incident. The MCP server is not running.

A single layer covers only one of these. Two surfaces sharing one
library function covers both, with the same semantics (idempotent
wipe + manifest reset; per-path delete + manifest update).

## Decision Drivers

- **Symmetry.** Same library use case behind both surfaces; same
  semantics; one place to test.
- **Off-session reachability.** Cleanup must not require the MCP
  server to be running.
- **Confirmation discipline.** Destructive operations exposed to an
  LLM need a deliberately ugly description (`DESTRUCTIVE` prefix);
  destructive CLI commands need an `--yes` flag.
- **No new dependencies.** Wraps existing port traits
  (`Persistence::delete_by_source`, `MetadataStore::save`) plus two
  new methods (`Persistence::reset_all`, `MetadataStore::reset`).

## Considered Options

### Option A — CLI-only cleanup (rejected)

Pros: the LLM can never accidentally drop a corpus. Cons: no
mid-session self-heal; the operator has to drop the Claude Code
session, run `schema reset`, restart. The friction pushes operators
back to manual `rm -rf` instead.

### Option B — MCP-tools-only cleanup (rejected)

Pros: smallest surface change. Cons: when the MCP server itself is
broken (e.g. cache file corrupt → server cannot open the store),
there is no recovery path short of `rm -rf`. Operator scenarios 2
above does not work at all.

### Option C — Both layers sharing one library function (chosen)

```text
              ┌─────────────────────────┐
              │  app::cleanup::Cleanup  │  ← single use case
              └────────────┬────────────┘
                           │
            ┌──────────────┼──────────────┐
            │                             │
   adapters::mcp_server          src/main.rs subcommands
   (reset_index, forget_source)  (schema reset, schema forget)
```

Pros: covers both operator scenarios; the use case is testable in
isolation; future surfaces (HTTP API, watcher-driven auto-clean)
plug into the same use case. Cons: two surface implementations to
keep in sync — addressed by the fitness function below.

## Decision Outcome

Chosen option: **C — both layers sharing one
`crate::app::cleanup::Cleanup`**.

### Use case API

```rust
pub struct Cleanup { /* Arc<dyn Persistence> + Arc<dyn MetadataStore> */ }

impl Cleanup {
    pub async fn reset_index(&self) -> anyhow::Result<()> {
        // 1. Persistence::reset_all       → DELETE FROM chunks; VACUUM;
        // 2. MetadataStore::reset         → save(&Metadata::default())
    }

    pub async fn forget_source(&self, path: &str) -> anyhow::Result<()> {
        // 1. Persistence::delete_by_source(&[path])
        // 2. metadata.files.remove(path); MetadataStore::save(&metadata)
    }
}
```

Both methods are idempotent (calling `reset_index` twice is fine;
`forget_source` on a path that does not exist returns `Ok(())`).

### MCP surface

Two new tools in `crate::adapters::mcp_server`:

| Tool             | Description                                                              |
| ---------------- | ------------------------------------------------------------------------ |
| `reset_index`    | `DESTRUCTIVE — wipe every chunk and the manifest for this project. Disk file kept open; contents emptied + VACUUM. Requires explicit operator confirmation.` |
| `forget_source`  | `DESTRUCTIVE — drop chunks for one source path and remove it from the manifest. Source file on disk is NOT touched.` |

The `DESTRUCTIVE` prefix is the LLM-side guard rail: any reasonable
prompt template encourages the model to ask the operator before
calling a tool whose description begins with that literal.

### CLI surface

Two new subcommands in `src/main.rs`:

| Command                                | Behaviour                                                            |
| -------------------------------------- | -------------------------------------------------------------------- |
| `schema reset --config X --yes`        | Calls `Cleanup::reset_index`. Without `--yes`, aborts with a friendly message. |
| `schema forget --config X --path PATH` | Calls `Cleanup::forget_source(PATH)`. Both `--config` and `--path` required. |

Both subcommands open the same wiring as `serve` (load config,
resolve identity, open store, build cleanup) but **DO NOT** start
the MCP server, **DO NOT** spawn the watcher, and **DO NOT** run
delta-sync first. They invoke the use case and exit. User-facing
output goes through `tracing::info!` (Layer A `forbid` blocks
`println!`/`eprintln!` outside test modules — see ADR-0012).

### Cache layout impact

None. `store.db` stays open; `DELETE FROM chunks` cascades to
`chunks_vec` and `chunks_fts` via the existing triggers; `VACUUM`
reclaims disk pages without recreating the file. `metadata.toml` is
overwritten in place with `Metadata::default()`.

## Consequences

- **Good:** Operators stop reaching for `rm -rf` on the cache
  directory; the documented path is now `schema reset` /
  `schema forget` or the matching MCP tools.
- **Good:** ADR-0011's "Cleanup tools (PR2)" follow-up is closed.
- **Good:** The watcher's blast radius is bounded — if a watcher
  bug pushes garbage into the index, the LLM can self-heal in the
  same session via `reset_index`.
- **Bad:** Two surfaces to keep in sync. Addressed by the fitness
  function (single use-case test + greppable `DESTRUCTIVE` prefix).
- **Neutral:** No new dependencies. Two new port methods
  (`Persistence::reset_all`, `MetadataStore::reset`) extend existing
  traits; both adapters already have all the primitives.

## Fitness function

- **Greppable LLM guard rail.** Every cleanup tool description
  begins with the literal `DESTRUCTIVE` —
  `grep '#\[tool(description = "DESTRUCTIVE'` lists exactly the
  destructive tools.
- **CLI confirmation guard rail.** `schema reset --config X` (no
  `--yes`) exits non-zero with a message instructing the operator
  to add `--yes`. Verified by integration test in `src/main.rs`'s
  test module if/when one lands; the smoke test today is the
  `schema reset --help` output.
- **Use-case unit test (full reset).** After
  `Cleanup::reset_index`, `Persistence::list_source_paths` returns
  empty AND `MetadataStore::load` returns `Metadata::default()`.
- **Use-case unit test (single forget).** After
  `Cleanup::forget_source("a.md")`, the named path is gone from
  both `Persistence` and `MetadataStore`; other paths are
  untouched.
- **Adapter test (reset_all cascades).** After
  `SqliteVecStore::reset_all`, `chunks_fts` is also empty
  (proves the `chunks_ad` trigger fired on the bulk delete).

## More information

- ADR-0011 — predecessor; closes its "Cleanup tools (PR2)"
  follow-up.
- ADR-0012 — strict lint baseline; `print_stdout`/`print_stderr`
  at `forbid` is why CLI output goes through `tracing::info!`.
- ADR-0013 — hexagonal architecture; the new `app/cleanup.rs`
  service depends only on `domain` + `ports`.
- `src/app/cleanup.rs` — implementation scope for the use case.
- `src/adapters/mcp_server.rs` — MCP surface.
- `src/main.rs` — CLI surface.

## Evidence and amendments

- _2026-04-25 — Initial recording. Cleanup gap surfaced when
  upgrading from the LanceDB-backed binary to the sqlite-vec
  binary (ADR-0011): the legacy `lance/` directory was easy to
  recognise and `rm -rf`, but per-path cleanup of a stale chunk
  set within the live `store.db` had no documented path short of
  re-running `cargo build --release` on a different chunker
  config. ADR-0015 records the fix._
- _2026-04-25 — Accepted and implemented. Two new port methods
  (`Persistence::reset_all`, `MetadataStore::reset`); one new
  app service (`app::cleanup::Cleanup`); two new MCP tools
  (`reset_index`, `forget_source`); two new CLI subcommands
  (`schema reset --yes`, `schema forget --path`). Test count
  rose from 31 to 36. All three validation gates (`cargo fmt`,
  `cargo clippy --all-features --all-targets --workspace -- -D
  warnings`, `cargo test --all-features`) exit 0._
