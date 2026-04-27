# Rust strict-lint playbook

Operational cookbook for the strict Clippy + rustc gate adopted by
**ADR-0012**. The ADR pins the *policy*; this playbook pins the
*remediation patterns* a contributor (human or agent) reaches for when
the gate fires. Mirrors the `rust-strict-lint` Anthropic skill.

> **Scope.** Read this when `cargo clippy --all-targets --all-features
> -- -D warnings` is red. Do **not** edit `Cargo.toml [lints.*]`,
> `clippy.toml`, or `rust-toolchain.toml` to silence a lint — fix the
> code (mirror of the project's PMD rule). Re-opening a lint requires
> an ADR amending ADR-0012.

## Canonical gate

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Pre-commit baseline. CI runs the same. Anything green here ships.

## Forbidden in production paths (Layer A — `forbid`)

| Symbol               | Replace with                               |
|----------------------|--------------------------------------------|
| `unwrap()`           | `?` + `Result`, `unwrap_or`, `unwrap_or_else`, `ok_or_else(\|\| anyhow!(...))` |
| `expect("…")`        | `with_context(\|\| "…")` (anyhow) or typed error |
| `panic!()`           | `return Err(...)`                          |
| `dbg!()`             | `tracing::debug!()` / `tracing::trace!()`  |
| `todo!()` / `unimplemented!()` | finish the path, or move it to `tests/`/feature-gate |
| `process::exit()`    | bubble error to `main` and let `?` propagate |
| `mem::forget()`      | `drop(x)` or `ManuallyDrop`                |
| `loop {}` (no break) | constraint-bounded loop with `break` condition |
| `println!()` / `print!()` | `tracing::info!()` (info → stderr per project tracing config) |
| `eprintln!()`        | `tracing::error!()` / `tracing::warn!()`   |

`unwrap_used` / `expect_used` are **`deny`** (not `forbid`) so test
modules can keep them with a narrow `#![allow(clippy::unwrap_used,
reason = "test fixtures may panic if the env is broken")]` at the
top of the `mod tests` block. Production paths cannot.

## Most-encountered lints — symptom → fix

### `absolute_paths`
**Symptom.** `consider bringing this path into scope with the use keyword`
on `std::fs::read_to_string`, `tokio::net::TcpListener`, etc.

**Fix.** Add a `use`:
```rust
use std::fs::read_to_string;
use tokio::net::TcpListener;
```
Even inside `#[cfg(test)]` modules — the rule is global. When the
short name collides, alias: `use std::sync::Mutex as StdMutex;`.

**Evidence in this repo.** `src/cli/mcp_shim.rs::tests` swapped
`tokio::net::TcpListener::bind` → `TcpListener::bind` after
importing the type at the top of the test module.

### `too_many_lines` (>30)
**Symptom.** `this function has too many lines (NN/30)`.

**Fix.** Extract one logical step into a private helper. Don't
collapse logic; split it. Two common shapes:

1. **Pipeline split** — each stage of a pipeline becomes a fn:
   `fn parse → fn validate → fn persist` called by the parent.
2. **Match-arm extraction** — long match arms move to free fns:
   `match err { Reauth => map_reauth(err), … }`.

**Evidence.** `src/cli/mcp_shim.rs::forward_with_retry` was
originally 40 lines; split into `retry_after_reauth` +
`retry_after_unreachable` + `map_retry_after_reauth_error` +
`map_retry_after_unreachable_error` to land at 18 lines.

### `format_push_string`
**Symptom.** `\`format!(..)\` appended to existing String`.

**Fix.** Use `write!` / `writeln!` from `std::fmt::Write`:
```rust
use std::fmt::Write as _;
let _ = writeln!(out, "[llm.{provider}]");
```
The leading `let _ =` is required because `write!` returns
`fmt::Result` and `let_underscore_drop` is denied (`drop()` is
the alternative; `let _ =` is fine because the `Result` is `Copy`-
poor and ignoring is intentional, but prefer matching style of
nearby code).

**Evidence.** `src/cli/install.rs::escape_json_string` and
`src/cli/secrets.rs::render_toml`.

### `as_conversions`
**Symptom.** `using a potentially dangerous silent \`as\` conversion`.

**Fix.** Use `From`/`TryFrom` instead:
```rust
let n = u32::from(c);                  // safe widening
let n = u32::try_from(big)?;           // narrowing — checked
```
Never `as` for narrowing or signed↔unsigned; only `as` is
acceptable for true zero-cost identity casts on POD enums, and
even then prefer `into()`.

**Evidence.** `src/cli/install.rs::escape_json_string` uses
`u32::from(c)` instead of `c as u32`.

### `missing_errors_doc`
**Symptom.** `docs for function returning Result missing # Errors section`.

**Fix.** Add a `# Errors` block to the public function docstring:
```rust
/// Run the shim until stdin closes …
///
/// # Errors
/// Returns `Err` when the global `endpoint.toml` cannot be read on
/// first call, when stdin / stdout I/O fails, when the daemon
/// returns a non-401 non-success status, or when both retry
/// attempts fail.
pub async fn run(endpoint_path: PathBuf) -> Result<()> {
```
Required on every `pub fn -> Result<…>`. Internal helpers don't
need it (denied lint only fires on `pub`).

### `doc_markdown` (item missing backticks)
**Symptom.** `item in documentation is missing backticks` on a
camelCase or PascalCase identifier.

**Fix.** Wrap in backticks: `OpenAI` → `` `OpenAI` ``,
`StreamableHttp` → `` `StreamableHttp` ``. Even brand names
(`OpenAI`, `JavaScript`) trigger this — wrap them.

### `too_long_first_doc_paragraph`
**Symptom.** `first doc comment paragraph is too long`.

**Fix.** Break the first paragraph after one sentence; everything
else moves to a second paragraph after a blank line:
```rust
/// Read-side mode-bit enforcement.
///
/// ADR-0031 §"Decision" item 1 says the daemon enforces `0600` …
```

### `missing_const_for_fn`
**Symptom.** `this could be a const fn`.

**Fix.** Add `const`:
```rust
pub const fn new(path: PathBuf) -> Self { Self { path, … } }
```
Works for fns whose body is `const`-eligible (no allocs, no trait
calls beyond `const` traits). For accessors and simple constructors
it's almost always feasible.

### `duration_suboptimal_units`
**Symptom.** `constructing a Duration using a smaller unit when a
larger unit would be more readable`.

**Fix.** Use the larger unit:
```rust
const REQUEST_TIMEOUT: Duration = Duration::from_mins(2);  // not from_secs(120)
```

### `let_underscore_drop`
**Symptom.** `non-binding let on a synchronization point` /
`temporary with significant Drop can be early dropped`.

**Fix.** Use `drop()` explicitly:
```rust
drop(response.bytes().await);
```
Or `let _unused = …;` (named binding, drop at scope end).

### `redundant_clone`
**Symptom.** `redundant clone`.

**Fix.** Move instead of clone:
```rust
let err = migrate_with_pairs(MigrateInputs { destination: Some(dest), … }, …);  // no .clone()
```
`dest.clone()` is correct only when `dest` is used after the
move. If it isn't, drop the clone.

### `collapsible_if`
**Symptom.** `this if statement can be collapsed`.

**Fix.** Combine with `&&` and `let-chains`:
```rust
if let Some(value) = headers.get(KEY)
    && session.session_id.as_ref() != Some(value)
{
    session.session_id = Some(value.clone());
}
```

### `let_chains` / `if_let_chain`
**Symptom.** Two nested `if let` blocks.

**Fix.** As above — `if let X = e1 && cond { … }` (Edition 2024
already supports it).

### `unsafe_code`
**Symptom.** `usage of an unsafe block`.

**Fix.** This is **`forbid`** crate-wide except for one documented
carve-out (sqlite-vec auto-extension; see ADR-0012 Evidence
2026-04-25). Refactor to remove the `unsafe` block. If the third-
party API truly needs it, propose an ADR amendment.

### `same_name_method` / `unused_self`
**Symptom.** Common on rmcp `#[tool]` handlers.

**Fix.** Use `#[expect(...)]` (not `#[allow(...)]`) with a
`reason = "..."` pointing at the macro/spec causing it:
```rust
#[expect(
    clippy::same_name_method,
    reason = "rmcp tool_router macro generates an inner method with the same name"
)]
```
`#[expect]` (not `#[allow]`) makes the carve-out self-expire when
the lint stops firing.

## Forbidden patterns in code (process)

- `#[allow(...)]` without `reason = "..."` — denied.
- `#[allow(...)]` where `#[expect(...)]` works — denied. **Always
  prefer `#[expect]`** so escapes self-expire.
- Crate- or module-wide `allow` of any Layer A or B lint —
  forbidden by construction.
- `#[deny(...)]`/`#[forbid(...)]` per-item — generally a smell;
  Cargo.toml is the source of truth.

## Adding a new module / file

1. Open the relevant `mod.rs` and add `pub mod <name>;`.
2. Inside the new file:
   - **No** crate-level `#![allow(...)]`. Adopt the gate.
   - Top-level `//!` doc comment explaining the role (Hexagonal
     per ADR-0013: domain / application / adapter).
   - For test modules, the only acceptable carve-out is:
     ```rust
     #![allow(
         clippy::unwrap_used,
         reason = "test fixtures may panic if the env is broken"
     )]
     ```
3. After every edit: run the canonical gate (`fmt --check` +
   `clippy -D warnings` + `test`).

## Adding a new dependency

1. Verify the version against `https://crates.io/api/v1/crates/<name>`
   (latest stable).
2. Pin the major-minor in `Cargo.toml`. Avoid `*`.
3. Prefer `default-features = false` + explicit features.
4. Run `cargo build` once to update `Cargo.lock`.
5. Run the canonical gate. New `unused_crate_dependencies` denials
   mean the dep is imported but never used in the binary target —
   either remove it or surface the use case.

## Hooks for agents (Claude Code, etc.)

Agents working in this repo must:

- Run the canonical gate **before** declaring a code change done.
  No exceptions.
- Treat clippy errors as **architectural feedback**, not annoyance.
  A `too_many_lines` is a refactor cue; a `format_push_string` is a
  hint about idiom.
- Never edit `Cargo.toml [lints.*]`, `clippy.toml`, or
  `rust-toolchain.toml` to make the gate pass — fix the code.
  Per the project's PMD rule (CLAUDE.md), this requires explicit
  operator permission and an ADR amendment.

## When the gate **does** need to change

Two legitimate cases:

1. **A new lint variant lands in stable Rust / Clippy** and fires
   on existing code that is genuinely correct (e.g., a deserializer
   pattern flagged by a new pedantic check). Path: short ADR
   amending ADR-0012 with `Evidence and amendments` recording the
   carve-out and either a per-item `#[expect(...)]` or a
   targeted `Cargo.toml` change.
2. **A new dependency forces a `forbid` carve-out** (e.g.,
   sqlite-vec for `unsafe_code`). Path: same — ADR-0012 amendment
   pointing at the offending crate version and a single, narrow
   `#[expect]`. The carve-out should self-expire as soon as the
   dep ships a safe alternative.

In both cases, the canonical command must still exit 0 after the
change. The CI gate is the proof.

## Cross-references

- **ADR-0012** — Strict lint baseline (the policy this playbook
  operationalises).
- **`rust-strict-lint`** Anthropic skill — original source of
  the three-layer baseline.
- **CLAUDE.md** — global "What NOT to do" + "Coding conventions"
  rules referenced from this playbook.
