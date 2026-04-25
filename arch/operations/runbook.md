# Runbook — `schema` operations

Operating handbook for `schema`: install, configure, debug, recover.

## Install

```bash
git clone <repo> ~/dev/mcp-schema
cd ~/dev/mcp-schema
mise install                     # installs Rust 1.95.0 per .mise.toml
mise trust                       # if not already trusted
cargo install --path .           # builds release, installs `schema` to ~/.cargo/bin/
schema --version                 # verify
```

## Configure a consumer project

In the project repo (e.g. `~/dev/lowcow-platform`):

1. Create `schema.toml`:

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
   ```

2. Validate it:

   ```bash
   schema validate --config schema.toml
   ```

   Expected output: `schema.toml is valid.` plus a summary.

3. Wire Claude Code:

   ```json
   // .mcp.json in the project root
   {
     "mcpServers": {
       "schema": {
         "command": "schema",
         "args": ["serve", "--config", "schema.toml"]
       }
     }
   }
   ```

4. Open Claude Code in the project directory; it spawns
   `schema` automatically.

## First-run downloads

On the first `schema serve` for any project:

- `bge-m3` ONNX weights (~2 GB) download to
  `~/.cache/schema/models/`. Subsequent runs (any project) reuse
  the cache.
- LanceDB initialises an empty `chunks` table at
  `~/.cache/schema/projects/<id>/lance/`.
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

- `opening LanceDB` — store initialised.
- `lance table created` — first run, table did not exist.
- `initialising bge-m3 embedder (downloads on first run)` — model
  load.
- `delta-sync complete total=X ...` — startup sync done.

## Cache layout

```text
~/.cache/schema/
├── models/
│   └── bge-m3/                     # ~2 GB ONNX weights
└── projects/
    └── <project-name>-<hash>/
        ├── lance/                  # LanceDB store
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

```bash
rm -rf ~/.cache/schema/projects/<project-name>-<hash>/
# next `schema serve` will rebuild from scratch (~30-60 s)
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

## Tools surface (FASE 1.0)

| Tool                | Purpose                                                  |
| ------------------- | -------------------------------------------------------- |
| `ping`              | Smoke test — returns `"pong"`.                           |
| `query`             | Generic top-K semantic search.                           |
| `find_decisions`    | Top-K search restricted to ADRs.                         |
| `glossary_lookup`   | Term → definition (semantic match).                      |
| `cross_reference`   | Artifact id → defining + referencing chunks.             |
| `list_corpus`       | Debug — list every indexed source path.                  |

Discoverable via Claude Code's `/mcp` listing once schema is
running.

## Updating the binary

```bash
cd ~/dev/mcp-schema
git pull
cargo install --path . --force
```

LanceDB schema migrations (FASE 1.1) run automatically on first
spawn after upgrade. If migration fails, the runbook migration
section will be added.

## Health check (FASE 1.1)

Planned `schema doctor --config schema.toml` will check:

- Model weights present + valid.
- LanceDB store openable.
- Manifest parseable.
- File watcher can watch the corpus paths.

Until then, manual checks:

```bash
schema validate --config schema.toml
ls -lh ~/.cache/schema/models/
ls -lh ~/.cache/schema/projects/<id>/lance/
```

## Uninstall

```bash
cargo uninstall schema
rm -rf ~/.cache/schema/
```
