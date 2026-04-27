---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
---

# 0023 — Config resolution: walk-up `schema.toml` + 12-factor ENV overlay

> **Y-statement** — In the context of operators running `schema` CLI verbs
> from arbitrary subdirectories of a consumer project (where the root
> `schema.toml` lives many levels up) and operators wanting to tune
> per-knob values without editing the canonical `schema.toml` (e.g., a
> CI run with `[embedding] nice = 0` or a local test with a different
> bind address), facing the choice between (a) **strict
> `--config <path>` requirement** (annoying day-to-day; breaks "run from
> anywhere"), (b) **`schema.toml` lookup via `dirs::config_dir()` or
> some user-global location** (anti-multi-project; conflicts with
> ADR-0003), (c) **walk-up CWD looking for `schema.toml`, plus
> 12-factor `SCHEMA_<SECTION>_<KEY>` ENV overlay applied on top of
> the parsed file**, or (d) **schema.toml adjacent to the binary**
> (anti-multi-project; one binary at `/usr/local/bin/schema` can't
> serve N consumer projects), we decided for **(c)**, against
> (a) (ergonomics), (b) (architectural conflict with ADR-0003),
> and (d) (architectural conflict with ADR-0003 + ADR-0014), to
> achieve **a config layer that honors `schema.toml` as the canonical
> per-project source while letting ENV vars override individual knobs
> at process-start without re-editing the file, and that auto-finds
> the `schema.toml` regardless of which subdirectory the operator
> is in**, accepting **the ambiguity of nested projects (walk-up
> stops at the closest `schema.toml`, which is the well-known
> `cargo` / `git` / `mise` semantic) and the cost of a startup log
> line per knob naming the source (file / env / default).**

## Context and Problem Statement

ADR-0004 fixed `schema.toml` as the canonical config file. ADR-0019/0020
made the `schema serve` daemon a launchd / systemd service that
captures the **absolute** config path at install time, so the daemon
itself does not need any config-resolution logic at runtime — its
service unit's `ExecStart` already contains `--config <abs path>`.

The problem is the **CLI verbs** the operator runs interactively:
`schema validate`, `schema reset`, `schema forget`, `schema install
--service`, `schema uninstall --service`, `schema service status`,
`schema mcp-config`. Today every one of those takes `--config` with
a `default_value("schema.toml")`, meaning:

- If the operator is at the project root, `schema validate` works.
- If the operator is at any subdirectory, `schema validate` fails:
  `default_value` resolves against CWD, and CWD has no `schema.toml`.

That is friction. Tools like `cargo`, `git`, and `mise` solve it by
**walking up** from CWD until they find their canonical file
(`Cargo.toml`, `.git/`, `mise.toml`).

Separately, the operator wants to tune individual knobs without
editing the project's checked-in `schema.toml` — for transient
overrides (CI, debug, ad-hoc port choice). The 12-factor pattern is
ENV vars with explicit precedence: `ENV > config-file > compiled-in
default`. `RUST_LOG` is already honored by `tracing-subscriber` in
this binary; no other knob is.

## Decision Drivers

- **Honor ADR-0003** (multi-project): one binary, N consumer projects;
  config can not be binary-adjacent.
- **Honor ADR-0004** (config-driven): `schema.toml` remains canonical.
- **12-factor compliance**: ENV is the universal override channel —
  works inside containers, systemd `Environment=`, launchd
  `EnvironmentVariables`, CI runners, dev shells.
- **Cargo-shaped ergonomics**: walk-up CWD until `schema.toml`,
  same algorithm as `cargo` for `Cargo.toml`.
- **Daemon mode unaffected**: the launchd / systemd service unit
  carries an absolute path; the daemon never needs walk-up.
- **Source visibility**: every knob's resolved value should log its
  source on startup. Hidden overrides cause confusion ("why does
  the server say nice=0 when my schema.toml says nice=5?" → because
  `SCHEMA_EMBEDDING_NICE=0` was set). Log line eliminates the
  mystery.

## Considered Options

### Option A — Strict `--config <path>` (rejected)

Operator must always pass `--config`. Friction every time. Easy to
typo. Doesn't fit `cargo`-shaped tools the operator already uses.

### Option B — `dirs::config_dir()` global lookup (rejected)

Lookup at `~/.config/schema/schema.toml` (Linux XDG) or
`~/Library/Application Support/schema/schema.toml` (macOS). One
config for all projects. Anti-multi-project: ADR-0003 is built on
the assumption that each project owns its own `schema.toml` because
the corpus is project-specific. A global config would either index
nothing (no `[corpus]`) or leak cross-project references.

### Option C — Walk-up CWD + 12-factor ENV (chosen)

Two layers, each independent:

**Layer 1 — config-path resolution** (which `schema.toml` to load):

```
1. --config <path>          CLI flag wins (any path, absolute or relative)
2. SCHEMA_CONFIG env         shortcut (skip walk-up if set)
3. walk-up CWD → /           ascend until schema.toml is found
4. ~/.schema.toml            HOME-dotfile fallback (per-user, hidden)
5. error: "schema.toml not found in CWD or any parent directory,
           and no ~/.schema.toml fallback present.
           Pass --config <path> or set SCHEMA_CONFIG=<path>."
```

The HOME-dotfile fallback (`~/.schema.toml`) is the **last resort** —
walk-up still wins when a project-local `schema.toml` exists. It
exists so that CLI verbs (`schema validate`, `schema mcp-config`, etc.)
run from a directory outside any project tree still resolve to a
sane config without forcing the operator to pass `--config`. The
per-project model (one `schema.toml` per consumer repo) is
preserved — `~/.schema.toml` does **not** become a multi-project
registry; it is at most a single-project user-default. ADR-0003
(multi-project) is honored because the operator's day-to-day
project work continues to walk-up to the per-repo file.

**Layer 2 — per-knob value resolution** (after the file is loaded):

```
1. SCHEMA_<SECTION>_<KEY>    ENV override (12-factor)
2. value in [section] of schema.toml
3. compiled-in default
```

Example: `SCHEMA_EMBEDDING_NICE=10` overrides `[embedding] nice = 5`
in the file, which itself overrides the compiled default of `5`.

Naming: `SCHEMA_<SECTION>_<KEY>` in SCREAMING_SNAKE_CASE; nested
keys join with `_` (no double-underscore).

### Option D — `schema.toml` adjacent to the binary (rejected)

`/usr/local/bin/schema` looks for `/usr/local/bin/schema.toml`.
Conflicts with multi-project (ADR-0003) and with codesign-protected
install path (ADR-0014). Single binary serves N projects; binary-
adjacent file would mean either one config for all projects (anti-
multi-project) or N binaries (anti-codesign).

## Decision Outcome

**Option C — walk-up + ENV overlay.** The two-layer resolution
described above lands in `src/adapters/toml_config.rs::SchemaConfig`
as `SchemaConfig::resolve(cli_arg: Option<&Path>)`, replacing the
existing `load(&Path)` constructor at the call sites. Old `load`
stays available for tests that pass a fixed path.

### Implementation crate choice

`figment` was considered as the multi-source merging library. With
the project's ~10 ENV-relevant knobs, the cost of a new dep
(figment + serde-spanned + uncased + transitive churn) outweighs
the ~80 lines of manual `std::env::var` + parse + assign code.
**Manual env overlay** is the chosen path; revisit if the knob
count exceeds ~30 or nested struct overrides become awkward.

### ENV vars in scope (initial set)

| ENV                                    | maps to                                          | default    |
|----------------------------------------|--------------------------------------------------|------------|
| `SCHEMA_CONFIG`                        | path to `schema.toml`                            | walk-up    |
| `SCHEMA_EMBEDDING_MODEL`               | `[embedding] model`                              | bge-m3     |
| `SCHEMA_EMBEDDING_NICE`                | `[embedding] nice`                               | 5          |
| `SCHEMA_RETRIEVAL_TOP_K_DEFAULT`       | `[retrieval] top_k_default`                      | 8          |
| `SCHEMA_RETRIEVAL_CHUNK_SIZE_MAX`      | `[retrieval] chunk_size_max`                     | 8192       |
| `SCHEMA_RETRIEVAL_FILE_SIZE_MAX`       | `[retrieval] file_size_max`                      | 5242880    |
| `SCHEMA_SECURITY_FOLLOW_SYMLINKS`      | `[security] follow_symlinks`                     | false      |

`RUST_LOG` is **not** prefixed `SCHEMA_*` — it is the `tracing`
ecosystem standard.

The following are intentionally **not** ENV-controlled (FASE 1.0):

- `[project]` name/version: identity is not a runtime knob.
- `[corpus]` paths/kinds: structured list, not flat scalar.
- `[security] exclude_default`: list, hardly tuned per-deploy.

If those need ENV overrides later, this ADR is amended (not
superseded) with the JSON-encoded `SCHEMA_CORPUS_JSON=[...]`
pattern or similar.

### Walk-up boundary

**No boundary.** Walk-up ascends from CWD all the way to FS root.
First `schema.toml` found wins. Same semantic as `cargo` walking up
for `Cargo.toml`; in practice the operator's project root is the
first match and walk-up stops there.

This was reconsidered against an earlier `$HOME` boundary proposal:
the operator works on projects in `~/dev/`, `~/Projects/`, but also
external drives (`/Volumes/...`), shared paths (`/opt/...`), and
ad-hoc scratch (`/tmp/...`). A `$HOME` boundary excludes the last
three. No boundary keeps everything working at the cost of
theoretically picking up a stray `schema.toml` somewhere up the
tree — a non-issue because the operator does not strew `schema.toml`
files at random ancestors.

### Source-of-truth log line

On every server / CLI start that loads a config, the resolver emits
one tracing event per knob:

```
INFO config: source path=/Users/op/dev/proj/schema.toml resolution=walk-up
INFO config: knob [embedding].nice          = 10    source=env(SCHEMA_EMBEDDING_NICE)
INFO config: knob [embedding].model         = bge-m3 source=file
INFO config: knob [retrieval].top_k_default = 8     source=default
```

Operator running `RUST_LOG=info schema validate` sees exactly which
fork of resolution applied. Eliminates "why is my override
ignored?" questions.

## Consequences

- **Good:** CLI verbs work from any subdirectory of a project.
- **Good:** ENV layer enables CI / container / launchd-Environment
  override without editing the checked-in file.
- **Good:** No new runtime dep (manual env overlay).
- **Good:** Source-log makes resolution debuggable.
- **Good:** Daemon mode unaffected (path is absolute, captured at
  install-time).
- **Neutral:** Walk-up adds one O(depth) `stat` chain on each CLI
  invocation. Trivially fast for any realistic project depth.
- **Bad:** Operator with two `schema.toml` files in nested ancestors
  gets the closest one. Acceptable, matches `cargo`. If operator
  hits this they pass `--config` explicitly.
- **Bad:** ENV layer means a process-wide var changes config under
  the operator's nose. Mitigated by the source-log line.

## Fitness function

- **Unit test (walk-up — found):** with a temp dir tree
  `<tmp>/a/b/c/`, write `schema.toml` at `<tmp>/a/`, set CWD to
  `<tmp>/a/b/c/`, call resolver — assert it returns
  `<tmp>/a/schema.toml`.
- **Unit test (walk-up — not found):** with empty temp dir tree,
  set CWD into it, resolver returns descriptive error containing
  `"schema.toml not found"`, `"--config"`, and `"SCHEMA_CONFIG"`.
- **Unit test (ENV layer — overrides file):** load a fixture
  `schema.toml` with `nice = 5`; set `SCHEMA_EMBEDDING_NICE=10`;
  resolved config has `nice = 10`. Source-log carries
  `source=env(SCHEMA_EMBEDDING_NICE)`.
- **Unit test (ENV layer — invalid value):** set
  `SCHEMA_EMBEDDING_NICE=99` (out-of-range per ADR-0018 0..=19);
  resolver returns parse error mentioning the env var, not silently
  fall back.
- **Unit test (precedence):** `--config` flag beats `SCHEMA_CONFIG`
  env var which beats walk-up.
- **Integration test (ENV-only)**: `unset --config`; only
  `SCHEMA_CONFIG=/abs/path` is set; resolver returns that path.

## Cross-references and follow-ups

- **ADR-0003 — multi-project architecture.** Honored: walk-up finds
  the per-project `schema.toml` next to the consumer's repo, not
  next to the binary, not in `~/.config/`.
- **ADR-0004 — config-driven projects.** Amended: `schema.toml`
  remains canonical, ENV is overlay on top. ADR-0004 frontmatter
  receives an `Evidence and amendments` entry.
- **ADR-0014 — install + codesign.** Honored: binary at
  `/usr/local/bin/schema` is unchanged; the launchd plist /
  systemd unit installed by ADR-0020 carries the absolute config
  path, so the daemon path resolution is install-time, not runtime.
- **ADR-0018 — embedder cap.** `SCHEMA_EMBEDDING_NICE` honored;
  validation (0..=19) re-applied after env overlay.
- **ADR-0019 — HTTP transport.** `SCHEMA_HTTP_BIND` is **not**
  added in FASE 1.0 — bind is fixed at `127.0.0.1:0` (kernel-
  assigned port, written to `endpoint.toml`). If a future use-case
  needs a fixed port (Docker port-publish, reverse proxy), open a
  new ADR.
- **ADR-0020 — service permanent.** Service unit's
  `Environment=`/`EnvironmentVariables` slot is the canonical way
  to set `SCHEMA_*` for the daemon — already documented in the
  rendered templates as the natural extension point.
- **ADR-0009 — generic MCP tools.** Receives an amendment adding
  `workspace_context` (a tool the LLM calls to learn its bound
  project), motivated alongside this ADR (the ENV-overlay decision
  raises questions like "which schema.toml is loaded? which
  overrides? which corpus is indexed?" — `workspace_context`
  answers those at the MCP layer).

## Evidence and amendments

- **2026-04-26 — implemented.** `src/adapters/toml_config.rs` gains
  `SchemaConfig::resolve(cli_arg)` returning `(SchemaConfig, PathBuf)`,
  composing four layers: CLI flag (with default-marker recognition),
  `SCHEMA_CONFIG` env, walk-up CWD → FS root, descriptive error.
  Walk-up extracted into a pure `walk_up_from(start: &Path)` helper so
  tests exercise the algorithm against `TempDir` without touching
  process-global CWD. ENV overlay implemented manually (no `figment`
  dep — ~80 lines for ~6 knobs is below the dep threshold) via a
  test-friendly seam: `apply_overrides_from(lookup: &dyn Fn)` lets
  tests inject a `HashMap`-backed lookup, avoiding `unsafe`
  `env::set_var` (process-global, racy under `cargo test`). All
  six knobs from this ADR's table land
  (`SCHEMA_EMBEDDING_MODEL/NICE`,
  `SCHEMA_RETRIEVAL_TOP_K_DEFAULT/CHUNK_SIZE_MAX/FILE_SIZE_MAX`,
  `SCHEMA_SECURITY_FOLLOW_SYMLINKS`). Source-of-truth log line
  emitted as one `tracing` `info` event per knob. Validation
  (`validate()`) re-applied after overlay so out-of-range env
  values (e.g., `SCHEMA_EMBEDDING_NICE=99`) fail descriptively
  via ADR-0018's nice-range check. Eight new unit tests cover
  walk-up found / not-found, default-marker recognition,
  resolution-branch label, single-knob overlay, multi-knob
  overlay, parse-failure error, empty-string env semantics.
  Validation gate green: `cargo fmt --check`, `cargo clippy
  --all-features --all-targets --workspace -- -D warnings`,
  `cargo test --all-features` all exit 0; **61 tests passing
  (was 53)**.
- **2026-04-26 — HOME-dotfile fallback added (`~/.schema.toml`).**
  Layer 1 resolution gains a fourth tier between walk-up and
  error: `~/.schema.toml`. Triggered by operator running CLI verbs
  outside any project tree where walk-up returns no match — the
  fallback gives `schema validate` / `schema mcp-config` a sane
  default without forcing `--config`. Walk-up still wins when a
  per-repo `schema.toml` exists, so ADR-0003 (multi-project) is
  preserved. `dirs::home_dir()` resolves the HOME root on all
  three platforms; `env::var("HOME")` is **not** used directly so
  Windows (where `HOME` is unset) still works via the `dirs` crate's
  `USERPROFILE` resolution. Implementation in
  `src/adapters/toml_config.rs::resolve_config_path` chains
  `walk_up_for_schema_toml` → `home_dotfile_fallback` → error.
  `describe_resolution` gains a `"home-dotfile"` branch label so
  the source-of-truth log line shows when the fallback fired. Two
  new unit tests: walk-up beats home-dotfile when both exist;
  home-dotfile fires when walk-up returns nothing.
- **2026-04-26 — companion `workspace_context` MCP tool landed.**
  ADR-0009 receives an amendment registering `workspace_context`
  as the LLM-facing answer to "which `schema.toml` is loaded,
  with which overrides, indexing what corpus" — the safety-net
  motivated by this ADR's ENV-overlay surface. Tool schema
  declared in `src/adapters/mcp_server.rs` (`WorkspaceContext`,
  `ProjectContext`, `CorpusEntry`, `EmbeddingContext`).
