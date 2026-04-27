# Runbook — `schema` operations

Operating handbook for `schema`: install, configure, debug, recover.

## Install (macOS, per ADR-0014)

```bash
git clone <repo> ~/dev/mcp-schema
cd ~/dev/mcp-schema
mise install                     # installs Rust 1.95.0 per .mise.toml
mise trust                       # if not already trusted

# Build + install + sign per ADR-0014 (revised 2026-04-26 — sign in place):
cargo build --release
sudo install -m 0755 target/release/schema /usr/local/bin/schema
sudo codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
              --options runtime \
              --force \
              /usr/local/bin/schema
codesign --verify --verbose=2 /usr/local/bin/schema   # fitness check
schema --version                                       # smoke
```

**Why install before sign:** signing at the final destination
(`/usr/local/bin/schema`) is atomic — eliminates the
filesystem-attribute / `com.apple.quarantine` flag race conditions
that arise when signing the cargo output and then `install`-copying
the signed binary across volumes or filesystem boundaries.
`sudo codesign` is required because `/usr/local/bin/schema` is
root-owned. Full rationale in ADR-0014.

**Codesign is optional.** Without an Apple Development identity, skip
the `codesign` step — the binary runs unsigned and Gatekeeper warns
once on first launch (clear with `xattr -d com.apple.quarantine
/usr/local/bin/schema` or right-click → Open). Service mode
(`schema install --service`) is more reliable with a codesigned
binary; without codesign, the rendered launchd plist may need the
Hardened Runtime expectation removed (drop `--options runtime`) on
future macOS releases.

If a stale `~/.cargo/bin/schema` exists from prior `cargo install`
runs, remove it once: `rm ~/.cargo/bin/schema`.

## Install (Linux)

```bash
cargo build --release
sudo install -m 0755 target/release/schema /usr/local/bin/schema
schema --version
```

Codesign is a macOS concept and skipped here. ADR-0014 scopes only
macOS.

## Configure a consumer project (post-ADR-0019/0020/0021)

Post-2026-04-26 the binary is **HTTP-only**: `schema serve` listens
on `127.0.0.1:0` with a UUIDv4 bearer token written to
`endpoint.toml`. Per-project lifecycle is owned by **launchd
(macOS)** or **systemd user (Linux)** via `schema install
--service`. Multiple Claude Code sessions on the same project
share **one** server (one embedder, one watcher, one delta-sync) —
the duplicated-process problem of the old stdio transport is gone.

In the project repo (e.g. `~/dev/lowcow-platform`):

1. **Create `schema.toml`** (same shape as before):

   ```toml
   [project]
   name = "lowcow-platform"
   version = "1"

   [[corpus]]
   path = "docs/decisions"
   kind = "adr-madr"

   [[corpus]]
   path = "docs/glossary.md"
   kind = "glossary"

   [[corpus]]
   path = "docs/business-rules"
   kind = "markdown"

   [[corpus]]
   path = "docs/integrations"
   kind = "markdown"

   [[corpus]]
   path = "schemas/integrations"
   kind = "cue"

   # Optional knobs added post-ADR-0018:
   [embedding]
   nice = 5     # OS scheduler nice value 0..=19, default 5
   ```

2. **Validate it** — works from any subdirectory of the project
   (walk-up, ADR-0023):

   ```bash
   schema validate --config schema.toml   # explicit path
   schema validate                         # walk-up finds it
   ```

   Expected output: `schema.toml is valid.` plus a summary.

3. **Install the service** (one-shot, per project):

   ```bash
   schema install --service --config schema.toml
   ```

   Renders the launchd plist (macOS) or systemd user unit (Linux)
   with the absolute path of the project's `schema.toml`, prints
   the OS-specific load command. Run that command to start the
   service. Idempotent — re-running re-renders the unit file.

4. **Generate the consumer-side `mcp-config` snippet** for Claude
   Code's `.mcp.json`:

   ```bash
   schema mcp-config --config schema.toml > /tmp/mcp-fragment.json
   ```

   Output is the `mcpServers.schema` block carrying the URL +
   bearer token from `endpoint.toml`. Paste into the consumer's
   `.mcp.json`:

   ```json
   {
     "mcpServers": {
       "schema": {
         "url": "http://127.0.0.1:48291",
         "headers": {
           "Authorization": "Bearer f47ac10b-58cc-..."
         }
       }
     }
   }
   ```

   **Token rotates on every server restart.** After
   `schema service status` shows a new `started_at` timestamp,
   re-run `schema mcp-config` and re-paste. A `schema mcp-shim`
   long-running proxy that re-reads `endpoint.toml` on `401` is
   the planned follow-up (ADR-0021 §"Shape B").

5. **Open Claude Code** in the project directory. It connects to
   the running HTTP MCP server via the `.mcp.json` fragment.
   Verify with `claude mcp list` (Claude Code's introspection)
   or the new `workspace_context` MCP tool (ADR-0009 amendment).

## Service lifecycle (ADR-0020)

```bash
schema install --service --config <path>     # render + write plist/unit
schema uninstall --service --config <path>   # remove plist/unit
schema service status --config <path>        # endpoint.toml + lifecycle hints
schema mcp-config --config <path>            # ready-to-paste .mcp.json fragment
```

Service identifiers per platform:

- **macOS**: `~/Library/LaunchAgents/com.farchanjo.schema.<project_id>.plist`,
  loaded with `launchctl bootstrap gui/$(id -u) <plist>`.
- **Linux**: `~/.config/systemd/user/schema-<project_id>.service`,
  enabled with `systemctl --user enable --now <unit>`.

Both carry `Nice={nice}` from `[embedding] nice` and stderr →
`~/.cache/schema/projects/<id>/stderr.log`.

## Configuration via ENV (ADR-0023, 12-factor overlay)

`SCHEMA_*` env vars override individual knobs from `schema.toml`.
Precedence: **ENV > `schema.toml` > compiled-in default**. The
server logs the resolved source for every knob on startup
(`source=env(SCHEMA_*)` vs `source=file`).

| ENV                                  | Maps to                       | Default     |
|--------------------------------------|-------------------------------|-------------|
| `SCHEMA_CONFIG`                      | path to `schema.toml`         | walk-up CWD |
| `SCHEMA_EMBEDDING_MODEL`             | `[embedding] model`           | `bge-m3`    |
| `SCHEMA_EMBEDDING_NICE`              | `[embedding] nice`            | `5`         |
| `SCHEMA_RETRIEVAL_TOP_K_DEFAULT`     | `[retrieval] top_k_default`   | `8`         |
| `SCHEMA_RETRIEVAL_CHUNK_SIZE_MAX`    | `[retrieval] chunk_size_max`  | `8192`      |
| `SCHEMA_RETRIEVAL_FILE_SIZE_MAX`     | `[retrieval] file_size_max`   | `5242880`   |
| `SCHEMA_SECURITY_FOLLOW_SYMLINKS`    | `[security] follow_symlinks`  | `false`     |

`RUST_LOG` is the standard `tracing` filter (`info`, `debug`,
`trace`); not prefixed `SCHEMA_*`.

`schema.toml` resolution (path-level, ADR-0023):

```
1. --config <path>     CLI flag wins
2. SCHEMA_CONFIG       env shortcut
3. walk-up CWD → /     ascend until schema.toml is found (cargo-style)
4. error               descriptive: "schema.toml not found ... pass --config or set SCHEMA_CONFIG"
```

Walk-up has **no `$HOME` boundary** — works for projects on
external drives, `/Volumes/...`, `/opt/...`, `/tmp/...` alike.

## First-run downloads

On the first `schema serve` for any project:

- `bge-m3` ONNX weights (~2 GB) download to
  `~/.cache/schema/models/`. Subsequent runs (any project) reuse
  the cache.
- The SQLite + `sqlite-vec` store initialises an empty `chunks`
  table at `~/.cache/schema/projects/<id>/store.db` (with WAL
  companions `store.db-wal` and `store.db-shm`).
- Initial embedding pass embeds every declared file. Expect
  ~20-60 s for ~150 files on M-series.

Subsequent spawns delta-sync only changed files (~1-3 s typical).

## Logs

`schema` logs to stderr via `tracing`. Stdout is reserved for
JSON-RPC. To see logs:

```bash
RUST_LOG=info schema serve --config schema.toml 2>schema.log
```

Common log lines:

- `opening sqlite-vec store` — store initialised.
- `sqlite-vec schema ensured` — first run, tables did not exist.
- `initialising bge-m3 embedder (downloads on first run)` — model
  load.
- `delta-sync complete total=X ...` — startup sync done.
- `legacy LanceDB cache directory detected; safe to delete after
  confirming the sqlite-vec store works` — first spawn after
  upgrading from a pre-ADR-0011 binary; manual cleanup expected.

## Cache layout

```text
~/.cache/schema/
├── models/
│   └── bge-m3/                     # ~2 GB ONNX weights
└── projects/
    └── <project-name>-<hash>/
        ├── store.db                # SQLite + sqlite-vec store
        ├── store.db-wal            # WAL journal
        ├── store.db-shm            # WAL shared memory
        ├── metadata.toml           # delta-sync manifest
        └── lock                    # advisory lock (FASE 1.1)
```

`<project-name>` is the sanitised `[project] name` from
`schema.toml`; `<hash>` is the first 16 hex chars of BLAKE3 of the
project's canonical absolute path. See ADR-0008.

## Common issues

### "schema.toml not found"

`schema` looks for `schema.toml` relative to the current working
directory. Either run from the project root or pass
`--config /abs/path/to/schema.toml`.

### "model download fails"

First run requires network access to Hugging Face Hub for the
bge-m3 weights. Behind a proxy, set `HTTPS_PROXY` /
`HF_HUB_OFFLINE=0` per HF docs.

### "ulimit -n exceeded" on macOS

If the project has > 256 indexable files, macOS's default file
descriptor limit may bite the kqueue watcher (ADR-0010). Bump:

```bash
ulimit -n 4096
schema serve --config schema.toml
```

To make permanent on macOS (zsh):

```bash
echo "ulimit -n 4096" >> ~/.zshrc
```

### "stale index" / "schema doesn't see my edits"

- Within a session: the kqueue watcher should pick up edits
  within ~15 ms. If not, check stderr logs for watcher errors.
- Between sessions: delta-sync runs at startup. If a file's
  hash matches the manifest, it is treated as unchanged.
  Pathological cases (touching mtime without changing content)
  are correctly handled; if you suspect corruption, see
  "Force re-index" below.

### Force re-index

Preferred (ADR-0015) — keeps the project cache directory and only
empties the store + manifest:

```bash
schema reset --config schema.toml --yes
# next `schema serve` rebuilds from scratch (~30-60 s)
```

Or, mid-session, the LLM can call the MCP tool `reset_index` (the
description is prefixed `DESTRUCTIVE` so a well-behaved client
asks the operator first).

To drop a single source path without wiping the whole index:

```bash
schema forget --config schema.toml --path docs/decisions/0042.md
```

The matching MCP tool is `forget_source` (also `DESTRUCTIVE`).

Fallback (still works if the cache itself is corrupt):

```bash
rm -rf ~/.cache/schema/projects/<project-name>-<hash>/
```

A `schema reindex --full` command is planned for FASE 1.1.

### Project moved or renamed

Moving the project to a new path produces a new `project_id`, so
a new cache directory is created on next spawn. The previous
cache becomes orphan. Manual cleanup:

```bash
ls ~/.cache/schema/projects/   # find orphans
rm -rf ~/.cache/schema/projects/<old-name>-<old-hash>/
```

A `schema gc --orphans` command is planned for FASE 1.1.

### Embedding model corruption

If `bge-m3` weights are corrupt (rare; HF Hub serves over TLS
with checksums):

```bash
rm -rf ~/.cache/schema/models/
# next spawn re-downloads (~2 GB)
```

## Tools surface (FASE 1.0, 9 tools post-ADR-0009 amendment)

| Tool                | Purpose                                                  |
| ------------------- | -------------------------------------------------------- |
| `ping`              | Smoke test — returns `"pong"`.                           |
| `workspace_context` | Project + corpus + embedding context. **Useful at session start to confirm `.mcp.json` points at the right project.** (ADR-0009 amendment, ADR-0023.) |
| `query`             | Generic top-K semantic search.                           |
| `find_decisions`    | Top-K search restricted to ADRs.                         |
| `glossary_lookup`   | Term → definition (semantic match).                      |
| `cross_reference`   | Artifact id → defining + referencing chunks.             |
| `list_corpus`       | Debug — list every indexed source path.                  |
| `reset_index`       | DESTRUCTIVE — wipe the whole index + manifest (ADR-0015).|
| `forget_source`     | DESTRUCTIVE — drop one source path from index + manifest (ADR-0015). |

Discoverable via Claude Code's `/mcp` listing once schema is
running.

## Migration from stdio to HTTP (ADR-0019 transition)

Operators upgrading from a pre-ADR-0019 binary that used stdio:

1. Stop any running pre-2026-04-26 `schema serve` processes:

   ```bash
   pkill -f "schema serve"          # kills stdio-spawned daemons
   ```

2. Build + install the new binary (same procedure as fresh install
   above).

3. For each consumer project, install the per-project service:

   ```bash
   cd ~/dev/<project>
   schema install --service --config schema.toml
   # follow the printed launchctl/systemctl command to load
   ```

4. Update the consumer's `.mcp.json` to the HTTP shape (was
   `command + args`, now `url + headers`):

   ```bash
   schema mcp-config --config schema.toml
   # paste output into .mcp.json
   ```

5. Reopen Claude Code; `claude mcp list` confirms `connected`.
   Inside Claude Code, ask the LLM to call the `workspace_context`
   MCP tool — it should report your project name + id, confirming
   the wiring lands at the right server.

## Updating the binary

```bash
cd ~/dev/mcp-schema
git pull
cargo install --path . --force
# OR per ADR-0014 (macOS, codesigned):
cargo build --release
codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
         --options runtime --force \
         target/release/schema
sudo install -m 0755 target/release/schema /usr/local/bin/schema
```

Restart per-project services so they pick up the new binary:

```bash
# macOS
launchctl bootout gui/$(id -u)/com.farchanjo.schema.<project_id>
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.farchanjo.schema.<project_id>.plist

# Linux
systemctl --user restart schema-<project_id>.service
```

**Upgrading across ADR-0011 (LanceDB → sqlite-vec):** the new
binary creates a fresh `store.db` and ignores any pre-existing
`lance/` directory. A `tracing::warn!` line on first spawn names
the legacy path; delete it manually once you have confirmed the
new store works:

```bash
rm -rf ~/.cache/schema/projects/<project-name>-<hash>/lance
```

(Cache migration follow-up tracked in ADR-0011.)

SQLite schema migrations (FASE 1.1+) will run automatically on
first spawn after future upgrades.

## Health check (FASE 1.1)

Planned `schema doctor --config schema.toml` will check:

- Model weights present + valid.
- SQLite + `sqlite-vec` store openable (vec0 + FTS5 virtual
  tables present).
- Manifest parseable.
- File watcher can watch the corpus paths.

Until then, manual checks:

```bash
schema validate --config schema.toml
ls -lh ~/.cache/schema/models/
ls -lh ~/.cache/schema/projects/<id>/store.db
```

## Migration to ADR-0026 (shared daemon)

> **Status (2026-04-26):** ADR-0026 is `accepted` (direction
> locked). Code lands behind a fitness-function gate (two-
> project canary E2E). Per-project units installed under
> ADR-0020 keep running in production until the shared daemon
> ships and that gate is green. **Operators take no action
> yet.** This section is the migration plan, written ahead of
> the code, so the cutover is mechanical when it lands.

### What changes

| Artefact | Before (ADR-0019/0020/0021) | After (ADR-0026) |
|----------|-----------------------------|------------------|
| Process count | N (one per project) | 1 (workstation-level) |
| ONNX session in RAM | N × ~1.7 GB | 1 × ~1.7 GB |
| launchd / systemd unit | N × `com.farchanjo.schema.<project_id>` | 1 × `com.farchanjo.schema.daemon` (macOS) / `schema-daemon.service` (Linux) |
| Project membership | implicit (one `--config` per unit) | explicit (`schema project register --config <path>`) |
| Registry file | none (each unit holds its config path) | `~/Library/Application Support/schema/registry.toml` (macOS) / `~/.local/state/schema/registry.toml` (Linux) — daemon-internal state |
| `endpoint.toml` per project | one URL + one token, daemon listens on a per-project random port | one URL (shared, random port) + one token **per project**, all `endpoint.toml` files point at the same URL |
| `MetadataStore` / `VectorStore` on disk | one `store.db` per project (ADR-0008) | unchanged — one `store.db` per project, opened by the shared daemon |
| `.mcp.json` on consumer side | per-project `url` + per-project `Bearer` | unchanged shape; only the `url` is now shared across projects, `Bearer` still differs per project |
| Bearer validator | `eq` against single token | `Map<TokenHash, ProjectId>`, defence-in-depth against URL/token mismatch |

### What does **not** change

- ADR-0008 (per-project cache directory) — every registered
  project still has its own `~/.cache/schema/projects/
  <project_id>/` with its own `store.db`. No combined index.
- ADR-0014 (install + codesign) — binary still installed
  once at `/usr/local/bin/schema`, signed once.
- ADR-0019 transport — Streamable HTTP via rmcp + axum,
  `LocalSessionManager`, `with_stateful_mode(true)`,
  localhost-only allowed hosts, SIGTERM drain.
- ADR-0021 token secrecy — still UUIDv4 per project, still
  `0600` on `endpoint.toml`, still localhost-bound, still
  rotated on daemon restart.
- Consumer-side `.mcp.json` shape — no client edit needed at
  cutover beyond regenerating with the new `url`/token.
- ADR-0024 E2E test runner (Python pytest + httpx) — the
  ADR-0026 canary fitness function lives in the same suite.

### Cutover sequence (operator, when the code ships)

> Numbered steps are run **in order** to avoid a window
> where neither old nor new daemons serve a project.

1. **Update `schema` to a build that contains the shared-
   daemon code path.**

   ```bash
   cd ~/dev/mcp-schema
   cargo build --release
   sudo install -m 0755 target/release/schema /usr/local/bin/schema
   sudo codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
                 --options runtime --force /usr/local/bin/schema   # macOS only
   schema --version
   ```

2. **Inventory current per-project units (ADR-0020).**

   ```bash
   # macOS:
   launchctl list | grep com.farchanjo.schema
   # Linux:
   systemctl --user list-units 'schema-*' --no-legend
   ```

   Save the list. Each entry maps to a `project_id` you must
   re-register against the new daemon.

3. **Stop and remove the per-project units.**

   ```bash
   # macOS, per project_id:
   launchctl bootout gui/$(id -u)/com.farchanjo.schema.<project_id>
   schema uninstall --service --config <project>/schema.toml

   # Linux, per project_id:
   systemctl --user disable --now schema-<project_id>.service
   schema uninstall --service --config <project>/schema.toml
   ```

   This deletes the plist / unit file but **preserves
   `~/.cache/schema/projects/<project_id>/`** (stores stay on
   disk, ADR-0008).

4. **Install the shared daemon unit (one verb, no per-
   project repetition).**

   ```bash
   schema install --service              # no --config flag
   # macOS:
   launchctl bootout gui/$(id -u)/com.farchanjo.schema.daemon 2>/dev/null
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.farchanjo.schema.daemon.plist
   # Linux:
   systemctl --user daemon-reload
   systemctl --user enable --now schema-daemon.service
   ```

   The daemon boots **with no projects registered**. It binds
   localhost on a single random port and writes one global
   admin endpoint file (admin token + URL) at
   `~/Library/Application Support/schema/admin-endpoint.toml`
   (macOS) / `~/.local/state/schema/admin-endpoint.toml`
   (Linux). It does **not** open any `store.db` until step 5.

5. **Register each project. Order does not matter.**

   ```bash
   for cfg in \
     ~/dev/mcp-schema/schema.toml \
     ~/dev/lowcow-platform/schema.toml \
     ~/dev/alloy-spec2/schema.toml \
     ~/dev/alloy-specs/schema.toml \
   ; do
     schema project register --config "$cfg"
   done
   schema project list   # sanity
   ```

   Each `register` opens that project's existing `store.db`
   (no re-embed; ADR-0008 stores carry over), runs an
   incremental delta-sync (cheap, hashes already in the
   manifest), and adds a row to the daemon's project
   registry. Per-project `endpoint.toml` is rewritten to
   point at the **shared** URL with a freshly minted bearer
   token.

6. **Refresh each consumer's `.mcp.json`.**

   ```bash
   for proj in ~/dev/mcp-schema ~/dev/lowcow-platform ~/dev/alloy-spec2 ~/dev/alloy-specs; do
     ( cd "$proj" && schema mcp-config --config schema.toml > .mcp.json )
   done
   ```

   `.mcp.json` keeps the same shape as today; the `url` is
   now identical across files (one daemon), the `Bearer`
   still differs per project. Open Claude Code in each
   project to confirm the connection is live.

7. **Run the canary fitness check (manual smoke).**

   ```bash
   pytest tests/e2e/test_adr0026_isolation.py -v
   ```

   This is the same fitness function that gates merge in
   CI — confirm it is green on your live workstation
   too. A failure here means the shared daemon is bleeding
   chunks across projects; **roll back immediately**
   (step 8) and file an incident against ADR-0026.

8. **Rollback (if anything in step 5–7 fails).**

   ```bash
   # Stop the shared daemon:
   launchctl bootout gui/$(id -u)/com.farchanjo.schema.daemon          # macOS
   systemctl --user disable --now schema-daemon.service                # Linux
   schema uninstall --service                                          # workstation-level

   # Reinstall per-project units (the old plists/units are
   # already deleted in step 3; re-run install verb per project):
   for cfg in ~/dev/mcp-schema/schema.toml ~/dev/lowcow-platform/schema.toml \
              ~/dev/alloy-spec2/schema.toml ~/dev/alloy-specs/schema.toml; do
     schema install --service --config "$cfg"
   done
   # Then bootstrap each per-project unit (see "Configure a
   # consumer project" above).
   ```

   `~/.cache/schema/projects/*/` is untouched throughout —
   no re-embed required on rollback. Per-project bearer
   tokens regenerate on each daemon restart by ADR-0021,
   so `.mcp.json` files need to be re-rendered with
   `schema mcp-config` after rollback.

### Red flags during migration

- **`schema project register` reports `corpus path X does
  not exist`** — same root cause as the FASE 1 lifecycle:
  consumer `schema.toml` has a stale path. Fix the
  `schema.toml`, re-register. ADR-0019 daemons would
  KeepAlive-loop on this; the shared daemon **must** keep
  running for the other projects (failure containment is
  one of ADR-0026's drivers — file an incident if a single
  project's invalid config takes the whole daemon down).
- **`launchctl print` shows `last exit code = 1` on the
  shared unit** — read
  `~/Library/Caches/schema/daemon/stderr.log`; do **not**
  delete the per-project caches in panic. ADR-0008 paths
  are the source of truth for embedded chunks; losing them
  forces a full re-embed.
- **Canary fitness E2E failure post-cutover** — same as
  step 7 failure path. Roll back via step 8. Cross-project
  bleed is the **one** condition where ADR-0026 says "stop
  the daemon, do not paper over".
- **Memory footprint of the shared daemon higher than
  Σ-of-old-daemons** — unexpected; means per-project
  retention got worse, not better. ADR-0027 (memory budget,
  follow-up to ADR-0026) is the place to track this.

## Uninstall

```bash
cargo uninstall schema
rm -rf ~/.cache/schema/
```
