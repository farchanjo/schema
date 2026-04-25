---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0004 — Config-driven projects via `schema.toml`

> **Y-statement** — In the context of ADR-0003 establishing that
> `schema` reads per-project configuration at startup, facing the
> design choice between (a) **a CLI-flag-only configuration** (every
> spawn gets paths via `--corpus docs/decisions=adr-madr`...), (b) **a
> JSON config** in a hidden directory like `.schema/config.json`, or
> (c) **a TOML config (`schema.toml`) at the repo root**, we decided
> for **TOML at the repo root with a stable schema** — the
> `[project]`, `[[corpus]]` (one entry per indexable directory or
> file), `[embedding]`, `[retrieval]`, and `[security]` sections —
> and against CLI-only (no place for projects to record their own
> conventions; reinvented per spawn) or JSON+hidden dir (less
> human-friendly than TOML; hiding implies "internal" but the file
> is documentation), to achieve a versioned, human-readable, lint-
> able configuration that lives next to the project's other root
> manifests (`Cargo.toml`, `package.json`, `pyproject.toml`),
> accepting that adding new fields is a soft breaking change for
> consumers and that path-traversal validation must enforce that
> every `corpus.path` resolves within the project root.

## Context and Problem Statement

ADR-0003 established that `schema` is config-driven multi-project.
The shape of that config drives consumer ergonomics:

- Every project author must read and edit it.
- Every project's CI pipeline must validate it.
- Future fields must be addable without breaking existing consumers.

Three reasonable shapes:

1. CLI flags only.
2. JSON config (hidden or visible).
3. TOML config at the repo root.

## Decision Drivers

- **Familiarity.** Rust + Python + JS projects all use TOML for
  project manifests. Operators read it without thinking.
- **Lint-ability.** TOML has stable parsers in every language.
  `serde + toml = 1.x` parses + validates with `Deserialize`
  derives.
- **Visibility.** A config at the repo root is *the* place to look.
  Hidden config (`.schema/`) implies something operators should not
  touch — wrong message for a domain-relevant file.
- **Versioning surface.** A `[project] version = "1"` field lets us
  evolve the schema in a controlled way later.

## Considered Options

### Option A — CLI flags only (rejected)

```bash
schema serve \
  --corpus docs/decisions=adr-madr \
  --corpus docs/glossary.md=glossary \
  --top-k 8
```

Implication: every spawn must reconstruct the entire project shape
on the command line. No place for the project to *record* its
conventions. Editor integrations (`.mcp.json`) become unwieldy. CI
validation cannot run against a single file.

### Option B — JSON in `.schema/config.json` (rejected)

JSON doesn't comment-tolerate (JSONC needed) and reads worse than
TOML for the kind of config we want (multiple `[[corpus]]`
sections). Hiding the file in `.schema/` further suggests internal-
only; but `schema.toml` is genuinely a project manifest.

### Option C — TOML at the repo root (chosen)

```toml
[project]
name = "lowcow-platform"
version = "1"

[[corpus]]
path = "docs/decisions"
kind = "adr-madr"
exclude = ["template*.md"]

[[corpus]]
path = "docs/glossary.md"
kind = "glossary"

[embedding]
model = "bge-m3"

[retrieval]
top_k_default = 8
chunk_size_max = 8192
file_size_max = 5242880

[security]
follow_symlinks = false
exclude_default = [".git", "node_modules", "target", "dist", "build"]
```

Implication: TOML parsing handled by `toml = "1.1"`; schema validated
via `serde::Deserialize`. Every consumer commits this file. Editors
have first-class TOML support.

## Decision Outcome

Chosen option: **C — `schema.toml` at the repo root**.

### Schema sections

| Section          | Purpose                                                                    |
| ---------------- | -------------------------------------------------------------------------- |
| `[project]`      | Identity (`name`) + `version` of the schema.toml format itself.            |
| `[[corpus]]`     | One per indexable directory or file. Fields: `path`, `kind`, `exclude`.    |
| `[embedding]`    | `model` (FASE 1.0: only `"bge-m3"`).                                       |
| `[retrieval]`    | `top_k_default`, `chunk_size_max`, `file_size_max`.                        |
| `[security]`     | `follow_symlinks`, `exclude_default` (always-ignored basenames).           |

### Validation rules (loader-time)

- `project.name` must be non-empty.
- Every `corpus.path` must be **relative** to the project root and
  must not contain `..` components.
- After resolving, the canonical path must remain inside the project
  root (no symlink-based escape).
- `embedding.model` must be `"bge-m3"` (FASE 1.0 only).
- `kind` enum is closed (`adr-madr`, `markdown`, `glossary`, `cue`,
  `openapi`); future kinds bump `[project] version`.

### Forward compatibility

- New fields added with `#[serde(default = "...")]` so older
  `schema.toml` files keep parsing.
- Removed fields deprecated for one major version before removal.
- Breaking changes bump `[project] version`.

## Consequences

- **Good:** TOML is universal; no consumer needs to learn a custom
  format.
- **Good:** validation runs offline (`schema validate --config
  schema.toml`), cheap to wire into any CI.
- **Good:** path-traversal protection prevents accidentally
  indexing parent directories or `/etc`.
- **Bad:** consumers must keep `schema.toml` in sync with their
  directory layout. Manual but bounded.
- **Bad:** the `kind` enum is closed today. Adding code chunking
  (FASE 2) requires a new variant.

## Fitness function

- `cargo test --lib` runs `parses_minimal_config` and
  `applies_defaults` to check the loader.
- `schema validate --config schema.toml` is the consumer-side gate.
- Path-traversal is unit-tested by trying `../../etc` paths and
  asserting validation rejects them (test coverage in FASE 1.1).

## More information

- `src/config/schema_toml.rs` — full schema + Deserialize impl.
- `src/config/project.rs` — project identity derivation.
- ADR-0003 — multi-project architecture (the why).
- ADR-0008 — cache isolation per project identity.
