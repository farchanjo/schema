---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0012 — Strict lint baseline (Layer A `forbid` + Layer B activation)

> **Y-statement** — In the context of preventing regressions in code
> quality (unwraps slipping into production paths, `dbg!()` left
> behind, design smells like raw casts and clone-on-Arc-by-method
> form, `#[allow(...)]` without rationale rotting into permanent
> escapes) on a Rust crate that is going to grow with hexagonal
> restructuring (ADR-0013) and a persistence-adapter swap
> (ADR-0011), facing the choice between (a) **status quo** —
> Layer A at `deny` only, no Layer B, Layer C complete (current
> state), (b) **full strict baseline** — Layer A at `forbid`
> for non-test lints (with `unwrap_used` / `expect_used` kept
> at `deny` for test-fixture ergonomics), Layer B groups
> (`clippy::all`, `pedantic`, `nursery`, `cargo`) at `deny`
> plus 29 explicit quality denies, Layer C unchanged, or (c)
> **Layer B groups only** without the explicit quality denies,
> we decided for **(b) the full strict baseline** as defined
> by the `rust-strict-lint` skill (Anthropic's curated Rust
> lint guidance), against (a) (leaves regression detection
> on design lints off; gives no compile-time guarantee against
> `unwrap()` in production paths) and (c) (incoherent — those
> 29 lints catch issues the groups don't, especially the
> `restriction` group's `as_conversions` and `clone_on_ref_ptr`
> which are not in `pedantic`/`nursery`), to achieve a
> regression-proof code-quality gate that rejects unwrap/panic/
> dbg in production paths at compile time, catches design smells
> via group denies, and forces `#[expect(...)]` over `#[allow(...)]`
> so lint escapes self-expire when the underlying issue is fixed,
> accepting that activation surfaces a one-time fix wave (size
> measured before this ADR — see Evidence), that some pedantic
> /nursery lints are noisy and may need narrow per-call-site
> `#[expect(..., reason = "...")]` annotations, and that the CI
> lint job grows by ~30 s on cold cache.

## Context and Problem Statement

The crate already has:

- **Layer A** (currently at `deny`, not `forbid`): `unwrap_used`,
  `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`,
  `exit`, `mem_forget`, `infinite_loop`, `print_stdout`,
  `print_stderr`. The original choice for `deny` (instead of
  `forbid`) cited two reasons in `Cargo.toml`'s comment block:
  derive macros injecting internal `allow(clippy::restriction)`
  attributes, and test code legitimately using `unwrap()` for
  fixture setup.
- **Layer B**: completely absent. No `clippy::all`/`pedantic`/
  `nursery`/`cargo` group denies; no quality denies (`as_conversions`,
  `clone_on_ref_ptr`, `allow_attributes_without_reason`, etc.).
- **Layer C** (rustc): complete and at `deny`/`forbid` levels.
- `clippy.toml` with the skill's threshold defaults
  (cognitive=25, lines=30, args=7, type-complexity=250).
- A CI workflow (`.github/workflows/lint.yml`) running `cargo fmt
  --check`, two clippy passes (production + all-targets), and
  `cargo test` on every push and PR. No `continue-on-error`.

The audit (skill format) found: 0 errors at the canonical command
on the existing code, 0 allow-without-reason violations. The
gate works **for what it currently catches**. The issue is what
it doesn't catch:

1. `unwrap()`/`panic!()` added to a non-test path is only a
   `deny` warning — can be silenced crate-wide with a `cfg`-gated
   `#[allow]`. `forbid` removes that escape.
2. Design lints in `clippy::pedantic`/`nursery` (e.g.,
   `redundant_else`, `needless_pass_by_value`,
   `option_if_let_else`, `branches_sharing_code`) and the
   restriction group lints we want (`as_conversions`,
   `clone_on_ref_ptr`, `allow_attributes_without_reason`) are
   simply not active.
3. The crate is about to gain meaningful new code surface from
   ADR-0013 (hexagonal restructure) and ADR-0011 (persistence
   adapter swap). Tightening **before** that work means the new
   code is born under the strict gate.

## Decision Drivers

- **Regression-proof safety lints.** `forbid` removes the
  `#[allow]`/`#[expect]` escape hatch on lints that should
  never have one (e.g., `panic`, `dbg_macro`, `exit`).
- **Design-smell catch on every PR.** Pedantic + nursery + the
  29 explicit quality denies surface issues that take seconds
  to fix early and become intractable later.
- **Self-expiring lint escapes.** `allow_attributes` + `allow_attributes_without_reason`
  force the use of `#[expect(..., reason = "...")]`, which fails
  to compile if the underlying issue is resolved — a real
  TODO instead of a comment that lies.
- **Cheap to do now, expensive to do later.** Activating the
  baseline before the hex restructure means the ports/adapter
  code is born under it. Adding it later means re-formatting
  most of the crate in one PR.

## Considered Options

### Option A — Status quo (rejected)

Keep current Layer A-deny + Layer C. Pros: zero work. Cons:
no compile-time guarantee against `unwrap()` in prod paths;
no design-smell coverage; PR review must catch by eye what
clippy could catch automatically.

### Option B — Full strict baseline (chosen)

Apply the `rust-strict-lint` skill's three layers verbatim,
with two documented carve-outs for this crate:

1. `unwrap_used` and `expect_used` stay at `deny` (not `forbid`).
   Rationale: `forbid` blocks `#[expect(..., reason)]` even in
   test modules, and our test fixtures legitimately use
   `unwrap()` with narrow per-module annotations. The skill
   itself names this as the only acceptable Layer A relaxation.
2. `multiple_crate_versions = "allow"`. Rationale: transitive
   dependency churn is outside our control; this lint produces
   noise without value for a single-binary crate.

### Option C — Layer B groups only, no explicit quality denies (rejected)

Activate `clippy::all`/`pedantic`/`nursery`/`cargo` at `deny`
without the 29 quality lints. Pros: less typing in `Cargo.toml`.
Cons: the 29 lints come from the `restriction` group (intentionally
not in any default group because it is opt-in *per lint*) — we
cannot get them without listing them. Skipping them defeats the
point of the upgrade.

## Decision Outcome

Chosen option: **B — full strict baseline with two carve-outs**.

### Cargo.toml `[lints.clippy]`

- All current Layer A lints **except** `unwrap_used` and
  `expect_used` move from `deny` to `forbid`.
- `unwrap_used` and `expect_used` stay at `deny` with documented
  rationale.
- Group denies added: `all`, `pedantic`, `nursery`, `cargo`
  (priority `-1` so individual `deny` overrides take precedence).
- 29 explicit quality denies added (per skill section 1, Layer B).
- `multiple_crate_versions = "allow"` documented carve-out.

### Cargo.toml `[lints.rust]`

Unchanged from current config. Layer C is already complete.

### Code-side rules

- Every `#[allow(...)]` requires `reason = "..."` (enforced by
  `allow_attributes_without_reason = "deny"`).
- Prefer `#[expect(..., reason = "...")]` over `#[allow(...)]`
  whenever the lint is expected to be removable in finite time
  (enforced by `allow_attributes = "deny"`).
- Crate-wide or module-wide `allow` of any Layer A or B lint is
  forbidden (by clippy's `restriction` group + the explicit
  baseline). Only narrow per-item allows with `reason = "..."`
  are acceptable.

### CI gate

The existing workflow at `.github/workflows/lint.yml` runs the
canonical command. **Operator action (recorded as follow-up):**
mark the `lint` job as a required check on the `main` branch
in repo settings.

## Consequences

- **Good:** Adding `unwrap()`, `panic!()`, `dbg!()` to a
  non-test path becomes a hard compile error that cannot be
  silenced.
- **Good:** Pedantic + nursery + the 29 quality denies catch
  ~50 patterns that previously slipped through review.
- **Good:** `#[expect(...)]` makes lint escapes self-expire,
  killing a class of stale `#[allow]` comments.
- **Good:** New code from ADR-0013 and ADR-0011 is born under
  the strict gate.
- **Bad:** Activation surfaces a one-time fix wave. Size to be
  measured at activation time and recorded in **Evidence**.
- **Bad:** Some lints (notably `pedantic`/`nursery` ones) are
  occasionally noisy. Resolved by narrow per-call-site
  `#[expect(..., reason = "...")]` annotations.
- **Neutral:** CI lint job runtime grows by ~30 s on cold cache
  due to additional clippy passes. Cached re-runs unchanged.

## Fitness function

- **CI gate:** `cargo clippy --all-features --all-targets --workspace
  -- -D warnings` is run on every push and PR by
  `.github/workflows/lint.yml`. Any new violation fails the
  build before merge.
- **Self-expiring escapes:** `allow_attributes = "deny"` causes
  any `#[allow(...)]` that could be `#[expect(...)]` to fail
  the build. Stale escapes auto-detect themselves when the
  underlying issue is fixed.
- **Forbid layer:** crate-wide rejection of `unwrap()`/`panic!()`/
  `dbg!()` etc. in production paths is enforced at compile
  time and cannot be downgraded by `#[allow]`/`#[expect]`.

## More information

- `Cargo.toml` (`[lints.clippy]`, `[lints.rust]`).
- `clippy.toml` (thresholds).
- `.github/workflows/lint.yml` (CI gate).
- `rust-strict-lint` skill (Anthropic) — the source of the
  baseline this ADR adopts verbatim.

## Follow-ups

- **Required-check bit on `main`.** Operator action in repo
  settings — outside the scope of this ADR but recorded so it
  is not forgotten.
- **Restriction-lint hardening.** The `unwrap_in_result`,
  `indexing_slicing`, `shadow_*`, and `partial_pub_fields`
  lints are valuable but produce more noise than the rest.
  Defer to a separate ADR after the codebase has lived under
  the current baseline for a few weeks.
- **`cargo deny` and `cargo audit`.** Supply-chain hygiene
  belongs in CI but is a separate concern (license / advisory /
  banned-versions) and warrants its own ADR.

## Evidence and amendments

- _2026-04-25 — Initial recording. Activation produces a fix
  wave whose size is measured at apply time. The post-fix
  exit-0 status of the canonical command on this repo is the
  proof that the baseline is live._
- _2026-04-25 — Activation results. Lint upgrade applied; the
  fix wave was **154 clippy errors + 18 E0453 derive-incompat
  compile errors = 172 issues**. After fixing, all three gates
  exit 0 (`cargo fmt --check`, `cargo clippy --all-features
  --all-targets --workspace -- -D warnings`, `cargo test
  --all-features`); 20 unit tests pass with no regression.
  Top categories: `absolute_paths` (50), `missing_errors_doc`
  (22), `pub_use` (10), `default_trait_access` (9),
  `too_many_lines` (7), `must_use_candidate` (7),
  `doc_markdown` (7), `missing_const_for_fn` (6),
  cast/`as_conversions` family (14)._
- _2026-04-25 — **Surprising consequence: `clap` derive is
  incompatible with Layer A `forbid`**. `clap`'s `#[derive(Parser)]`
  and `#[derive(Subcommand)]` proc-macros expand to code
  containing `#[allow(clippy::restriction)]` (touching
  `unwrap_used`, `panic`, etc.). Layer A `forbid` cannot be
  downgraded by `#[allow]` from anyone — including macros — so
  the build fails with E0453. This is a fundamental rustc
  invariant; neither `#[expect]` nor `#[allow]` at the call
  site rescues it. **Resolution:** rewrote `src/main.rs` from
  the `clap` derive API to `clap`'s builder API. Behavior is
  identical (same flags `--config`, same subcommands `serve`/
  `validate`, same default behavior). No behaviour regression
  (tests + manual `--help` checked). This deviation is
  recorded here rather than as a Layer A carve-out because
  the issue is the macro-expansion shape, not the lint
  itself; the lint stays at `forbid` for the project's own
  code, which is what we wanted._
- _2026-04-25 — `#[expect(...)]` annotations added (9 total):
  five module-level `#![expect(clippy::pub_use, reason = "...
  restructured by ADR-0013")]` in `config/mod.rs`,
  `corpus/mod.rs`, `embeddings/mod.rs`, `mcp/mod.rs`,
  `retrieval/mod.rs` (will resolve when ADR-0013 lands);
  two on `PingParams`/`ListCorpusParams` for
  `clippy::empty_structs_with_brackets` (schemars/serde
  shape JSON `{}`, not `null` — converting to unit structs
  would change the MCP wire schema); one on `fn ping` for
  `clippy::same_name_method` (rmcp `tool_router` macro
  generates an inner method with the same name) and one for
  `clippy::unused_self` on the same fn (rmcp tool handlers
  must be methods on the server type)._
- _2026-04-27 — operational playbook landed at
  `arch/operations/rust-lint-playbook.md`. Mirrors the
  `rust-strict-lint` Anthropic skill section 4 ("most-encountered
  lints") with concrete remediation patterns, file/line
  evidence from this repo, and a hooks-for-agents block
  forbidding `Cargo.toml [lints.*]` / `clippy.toml` /
  `rust-toolchain.toml` edits without an ADR amendment. CLAUDE.md
  links it from the "Coding conventions" section so a fresh
  contributor (human or agent) finds the cookbook before
  reaching for `#[allow]`._

- _2026-04-25 — **Layer C amendment: `unsafe_code` downgraded
  from `forbid` to `deny`** as the original ADR text already
  flagged would happen "if FASE 2 ever requires `unsafe`".
  Trigger: ADR-0011's persistence swap. The `sqlite-vec`
  0.1.9 crate exposes its loader only as a raw `extern "C"
  fn sqlite3_vec_init()`; loading it via `rusqlite`'s
  `auto_extension::register_auto_extension(...)` is declared
  `pub unsafe fn`, and `Connection::load_extension(...)` is
  also `pub unsafe fn` and additionally requires a
  separately-shipped shared library (incompatible with our
  `bundled` SQLite). There is no safe API to load the
  extension; aspirational `sqlite_vec::load(&conn)` cited
  in ADR-0011 does not exist in the 0.1.9 release.
  **Resolution:** one narrow `#[expect(unsafe_code, reason
  = "sqlite-vec 0.1.9 ships only `extern \"C\" fn
  sqlite3_vec_init`; ADR-0011 + ADR-0012-amendment
  2026-04-25")]` block at the single call site in
  `src/adapters/sqlite_vec_store.rs::open` registering the
  vec0 auto-extension. Every other code path in the crate
  remains at `deny` — adding new `unsafe` anywhere else
  fails the gate. `expect` (not `allow`) is used so the
  carve-out self-expires the moment `sqlite-vec` ships a
  safe loader. **Follow-up:** when `sqlite-vec` publishes a
  safe API (tracked in ADR-0011 follow-ups), remove the
  `#[expect]` block; clippy will then flag the unused
  attribute and force the cleanup._
