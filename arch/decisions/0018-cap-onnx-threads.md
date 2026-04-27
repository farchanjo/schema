---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
---

# 0018 — Cap embedder CPU footprint via process-level scheduler nice + (when available) `ort` intra-op threads

> **Y-statement** — In the context of `FastembedEmbedder::new_bge_m3`
> instantiating a `fastembed::TextEmbedding` whose underlying `ort`
> ONNX Runtime session is constructed without explicit thread caps
> (verified in `ort` 2.0-rc.12 source: `SessionBuilder::with_intra_threads`
> at `src/session/builder/impl_options.rs:52` calls
> `SetIntraOpNumThreads`; when not called, ONNX Runtime defaults to
> `std::thread::hardware_concurrency()`, i.e. one intra-op worker per
> logical CPU), and observing that `fastembed` 5.13's public
> `InitOptions` exposes only `with_execution_providers` and **does not
> expose intra-op thread count to library consumers**, facing the
> choice between (a) **leave the default** (saturates every core
> during embed bursts; multiplied per concurrent process pre-ADR-0019),
> (b) **wait for an upstream fastembed PR exposing intra-op threads
> on `InitOptions`** (best long-term answer, blocked on upstream),
> (c) **drop fastembed and use `ort` directly** (~200-line rewrite
> of the embedder adapter; gains thread cap and other knobs, loses
> fastembed's model-download convenience), (d) **process-level CPU
> share via launchd / systemd `Nice` and macOS `LowPriorityIO`**
> (granularity is the whole `schema` process, but ADR-0020 introduces
> these unit files anyway), or (e) **per-process scheduler nice via
> the `nix` crate at startup**, we decided for **(d) +
> opportunistically (b) when upstream lands**, against (a) (the
> watcher batch + multi-process compounding makes the default a
> regression every time the operator types in another window),
> against (c) (high cost to remove a working dep just to flip one
> flag), and against (e) (duplicates what the service unit already
> does), to achieve **predictable embedder CPU envelope per process
> through the OS scheduler today, with a fastembed-native intra-op
> cap layered in as soon as upstream exposes it**, accepting **a
> coarser-grained cap (whole process, not just embedder threads)
> as the deliberate trade for shipping a working interactive
> experience now rather than blocking on upstream**.

## Context and Problem Statement

`fastembed` 5.13 wraps `ort` 2.0-rc.12 ONNX Runtime. Verifying the
default behaviour against `ort` source:

- `ort::session::SessionBuilder::with_intra_threads(num_threads: usize)`
  calls `SetIntraOpNumThreads` (`/tmp/ort/src/session/builder/impl_options.rs:52`).
- When the builder method is not called, the ONNX Runtime C++ side
  reads `std::thread::hardware_concurrency()` and uses that for the
  intra-op pool.
- On the operator's 16-physical-core / 32-thread machine the default
  is therefore 32 intra-op threads. Two concurrent `schema serve`
  processes (operator-reported state pre-ADR-0019) thus drive
  64 threads against a 32-logical-core box, oversubscribing 2:1.

Verifying fastembed's public surface:

- `fastembed::InitOptions::with_execution_providers(Vec<ort::ExecutionProviderDispatch>)`
  is the only intra-op-adjacent knob (`/tmp/fastembed/src/init.rs:73-77`).
- `ExecutionProviderDispatch` does not carry intra-op thread settings;
  intra-op threads are a `SessionBuilder`-level setting, not a
  provider-level one.
- `fastembed::TextEmbedding::try_new(opts: InitOptions)` builds the
  `ort::Session` internally and **never invokes
  `SessionBuilder::with_intra_threads`**.
- Conclusion: **fastembed 5.13 does not expose intra-op thread count
  to library consumers**. The exact knob exists in `ort` but is not
  reachable through fastembed's public API.

`ORT_INTRA_OP_NUM_THREADS` and `OMP_NUM_THREADS` env vars do not
reliably cap intra-op threads in `ort` 2.0; the env-var path was a
stale habit from older ONNX Runtime C++ builds, and the operator
should not rely on it. Verified by reading
`/tmp/ort/src/session/builder/impl_options.rs` — the only path that
sets intra-op threads is the explicit `with_intra_threads` call.

ADR-0020 introduces a launchd `LaunchAgent` (macOS) and a
`systemd --user` service (Linux) per project. Both have first-class
scheduler/IO-priority knobs (`Nice`, `IOSchedulingPriority`,
`LowPriorityIO`). The cleanest cap available today, without
forking fastembed and without dropping it, is to use those knobs.

## Decision Drivers

- **Predictable interactive CPU.** Background indexing must not
  freeze the editor, the browser, the MCP client.
- **Operator visibility.** The cap should be in `schema.toml` so a
  consumer project can tune it without editing service units by
  hand.
- **No new unsafe code.** Older drafts of this ADR floated
  `std::env::set_var` as a fallback; in Rust 2024 this is `unsafe`
  fn (process-env mutation is `Sync`-unsafe), and ADR-0012 keeps
  `unsafe_code = "deny"` (Layer C). The fallback is dropped.
- **No premature dep removal.** fastembed continues to provide
  model download + tokenizer plumbing; replacing it just to set
  one flag is a bad trade.
- **Compose with the OS service.** ADR-0020 already builds the
  service file; threading the nice value through it costs zero
  extra dep.

## Considered Options

### Option A — Default `ort` thread settings (rejected)

Status quo: 1 intra-op thread per logical CPU. Smallest one-shot
latency, biggest impact on every other process during embed bursts,
compounds catastrophically pre-ADR-0019.

### Option B — Upstream fastembed PR exposing `with_intra_threads` (deferred, opportunistic)

The cleanest answer is for `fastembed::InitOptions` to gain
`with_intra_threads(usize)` (and `with_inter_threads(usize)`),
which `TextEmbedding::try_new` would forward to
`SessionBuilder::with_intra_threads`. We prepare the PR locally and
file it upstream. **Adoption is not on this ADR's critical path**;
when upstream lands a release, we add the call site and treat the
process-level nice as belt + suspenders.

### Option C — Drop fastembed, use `ort` directly (rejected)

Rewrite `FastembedEmbedder` against `ort::Session` directly.
~200 lines of adapter code, including model download, tokenizer
glue (Hugging Face Hub via `hf-hub`), and ONNX session lifecycle.
Gains intra-op thread control and other knobs. Loses fastembed's
maintained download-and-cache logic. **Not worth it for a single
flag** when Option D ships the same outcome from a different layer.
Re-open this option only if multiple knobs (intra-op + inter-op +
quantisation + custom tokenizer) accumulate on the wishlist.

### Option D — Process-level CPU share via launchd / systemd nice (chosen)

ADR-0020 generates the LaunchAgent plist (macOS) and
`systemd --user` service (Linux) per project. We add to those
templates:

- **macOS plist** — `Nice` integer key, default 5 (lower priority
  than interactive); optional `LowPriorityIO` boolean for IO
  scheduling.
- **Linux systemd unit** — `Nice=5` and `IOSchedulingClass=idle`
  (or `IOSchedulingClass=best-effort` + `IOSchedulingPriority=7`).

`schema.toml` exposes one knob:

```toml
[embedding]
nice = 5    # OS scheduler nice value, 0..19; default 5
```

The CLI reads `nice` at install time and writes it into the
rendered service file. The running process therefore runs at the
configured nice; all its threads — embedder, watcher, sqlite —
inherit. Coarse but effective: under contention, the operator's
foreground processes preempt schema's threads.

### Option E — Per-process `nix::sched::nice` at startup (rejected)

Same outcome as Option D but applied from inside the binary on
every start. Duplicates what the service unit already does and
introduces a `nix` dep just to call `nice(2)`. Reject as
redundant.

## Decision Outcome

**Option D — process-level scheduler nice via the service unit
(launchd `Nice`, systemd `Nice=`), defaulting to 5, configurable
via `schema.toml [embedding] nice = N`.**

Implementation crosses ADRs:

1. `schema.toml` gains an optional `[embedding] nice = u8` (range
   0..19; default 5; floor 0 for "do not nice"). Validation rejects
   values > 19.
2. `schema install --service --config X` (verb defined in ADR-0020)
   reads the `nice` value and substitutes `{nice}` into the plist
   / unit template.
3. **No** `unsafe std::env::set_var`, **no** new dep on `nix` or
   `num_cpus` for the cap path. `available_parallelism` is only
   used downstream by Option B's eventual call when fastembed
   exposes intra_threads.

When Option B (fastembed upstream PR) ships, this ADR is amended:
add `[embedding] threads = N` to `schema.toml` and call
`InitOptions::with_intra_threads` in `FastembedEmbedder::new_bge_m3`.
Default for `threads` will be `max(1, available_parallelism / 4)`,
hard-capped at 4 by default (the operator's foreground processes
keep more head-room than `num_cpus / 2` would leave on a 32-thread
laptop). Until that release, only the nice-based cap is shipped.

## Consequences

- **Good:** the operator's foreground experience improves
  immediately on adoption. nice 5 vs 0 alone halves embed-burst
  preemption against the editor under contention.
- **Good:** zero new Rust deps, zero new unsafe, no fastembed fork
  needed.
- **Good:** the cap follows the binary regardless of what spawned
  it (service, manual `schema serve --http`, future Docker
  package). Coarse-grained, but the granularity matches the
  contention model.
- **Neutral:** the cap is **whole-process**, not embedder-only.
  Watcher, sqlite-vec, and tracing also run niced. Acceptable —
  none of those is on a tight latency budget.
- **Bad:** without Option B, ONNX still spawns
  `hardware_concurrency()` threads; the nice value just makes
  the OS schedule them last. A heavy embed burst on an idle box
  still uses every core. Mitigation: ADR-0017 makes embed bursts
  rare; ADR-0019 collapses N processes into 1.
- **Bad:** Linux `IOSchedulingClass=idle` may delay sqlite WAL
  checkpoints under disk contention. Switch to `best-effort` /
  `priority 7` if observed in the wild. Documented as a follow-up
  knob.

## Fitness function

- **Unit test:** `EmbeddingConfig::nice` defaults to 5 when omitted,
  parses and validates 0..19, rejects 20.
- **Integration test (Linux, gated):** install service with
  `[embedding] nice = 10`; `cat /proc/<pid>/status | grep ^Nice:`
  reports 10. Tear down. `#[cfg(target_os = "linux")]` gate.
- **Integration test (macOS, gated):** install service with
  `[embedding] nice = 10`; `ps -o nice -p <pid>` reports 10.
  Manual on operator's box (`launchctl bootstrap` requires GUI
  session per ADR-0020, GHA macOS runners cannot honor it).
- **CI lint:** `schema.toml` validation rejects `nice = 20+` and
  any negative value.

## Cross-references and follow-ups

- **ADR-0005 — fastembed bge-m3.** Embedder choice unchanged.
  This ADR adds a process-level cap; if Option B's upstream PR
  lands, this ADR is amended with the in-process knob without
  re-opening ADR-0005.
- **ADR-0004 — config-driven projects.** New `[embedding] nice`
  knob follows the `[retrieval]` / `[security]` precedent.
- **ADR-0012 — strict lint baseline.** No `unsafe_code` carve-out
  needed; the env-var fallback was rejected for that reason.
- **ADR-0017 — mtime-size short-circuit.** Reduces the *frequency*
  of embed bursts; this ADR caps the *priority* of each surviving
  burst. Compose.
- **ADR-0019 — HTTP transport (proposed).** Eliminates duplicated
  embedders across processes; this ADR caps the surviving single
  embedder's process-level priority.
- **ADR-0020 — service permanent (proposed).** This ADR's
  implementation lives entirely in ADR-0020's service templates.
  ADR-0020's render step substitutes `{nice}`.
- **Follow-up:** when fastembed upstream exposes
  `InitOptions::with_intra_threads`, amend this ADR with an
  `Evidence and amendments` entry adding `[embedding] threads`
  and the call site in `FastembedEmbedder::new_bge_m3`.
- **Follow-up:** if Option D's whole-process granularity proves
  too coarse (e.g., watcher latency suffers), open a new ADR
  re-evaluating Option C (drop fastembed) or pursue Option B
  more aggressively.

## Evidence and amendments

- **2026-04-26 — `[embedding] nice` config field added at
  `src/adapters/toml_config.rs::EmbeddingConfig::nice`** (default 5,
  validated 0..=19, rejected at `SchemaConfig::validate`). Unit
  tests `parses_embedding_nice_override` and `rejects_nice_out_of_range`
  cover both branches. The substitution slot `{nice}` in the launchd
  plist and systemd unit templates lands with ADR-0020 (service
  templates); the config field is now ready for that wiring. Gate
  green (39 tests pass; clippy/fmt clean).
