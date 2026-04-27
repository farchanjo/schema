---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2026-10-26
---

# 0022 — Bench-driven evaluation of `tokio-uring` for delta-sync IO (Linux only)

> **Y-statement** — In the context of `DeltaSync::run`'s walker phase
> issuing one `fs::metadata` + one `fs::read` per file across the
> full corpus on every pass (5 K+ syscalls per pass on the
> lowcow-platform corpus), facing the operator's request to "use the
> best of each OS — io_uring, epoll, kqueue, IOCP transparently",
> with the architectural reality that **`tokio` 1.x + `mio` 1.x
> already covers epoll / kqueue / IOCP transparently**, leaving only
> the io_uring (Linux ≥ 5.10) gap, and where `tokio-uring` 0.5.0 is
> a current-thread runtime that cannot be the project's default
> (HTTP server in ADR-0019 needs the multi-thread executor) but can
> be **spawned in a dedicated thread for the duration of a
> delta-sync pass on Linux**, facing the choice between (a) **adopt
> `tokio-uring` D-3 hybrid right now** (commits ~500 lines of
> dual-impl Linux/non-Linux code before measuring), (b) **leave
> io_uring out forever** (closes a door without evidence), or (c)
> **execute a controlled bench that compares the post-ADR-0017 +
> ADR-0018 baseline against a minimal `tokio-uring` D-3 prototype on
> the build VM, and adopt only if the delta-sync warmup speedup
> exceeds 10 % AND the warmup wall clock still exceeds 5 s**, we
> decided for **(c)**, against (a) (premature optimisation in
> dual-impl debt) and (b) (rejects io_uring without empirical
> grounds), to achieve **an evidence-based decision on whether the
> Linux-only IO speedup is worth the long-term dual-implementation
> maintenance**, accepting **the bench cost (4-8 hours on the
> already-provisioned Linux VM) and the explicit acknowledgement
> that this ADR may close as `rejected` if the numbers do not
> support the rewrite**.

## Context and Problem Statement

The operator asked, in summary:

> "Use the best of each OS — io_uring, epoll, kqueue, IOCP
> transparently. Why can't we use io_uring?"

Two facts answer the technical premise:

1. **`tokio` + `mio` is already transparent for 3 of 4 primitives.**
   `mio` 1.x picks `epoll` on Linux, `kqueue` on macOS / BSD, and
   `IOCP` (I/O Completion Ports) on Windows. The "OS factory" the
   operator described already exists.
2. **`io_uring` is not a drop-in replacement for `epoll`.**
   Readiness-based (`epoll`/`kqueue`/`IOCP`-via-tokio) gives the
   app a "fd is ready" notification; the app reads. Completion-based
   (`io_uring`) submits an op + a buffer, the kernel takes ownership,
   and notifies on completion. The two models have incompatible
   trait shapes (`AsyncRead<&mut [u8]>` vs `read_at(buf: B) -> (res, B)`).
   `tokio` proper does not implement io_uring; the maintainers
   rejected it because the API change would break every async lib.

`tokio-uring` 0.5.0 is a separate runtime that combines a tokio
current-thread runtime with an io_uring driver. Its docs confirm
it can host tokio-ecosystem libraries (hyper, etc.), but it is
single-threaded and Linux-kernel-≥-5.10-only.

Three viable shapes for adopting it (validated against the cloned
sources of `compio`, `rust-sdk` (rmcp), `axum`, and `tokio-uring`
under `/tmp/`):

- **D-1.** Replace the project's main runtime with `tokio_uring::start`
  (Linux only; current-thread; loses HTTP server multi-threading).
- **D-2.** Run main in tokio multi-thread, dispatch every file IO
  through a channel to a dedicated tokio-uring thread (heavy
  channel boilerplate; refactor into a new async port).
- **D-3.** Spawn a dedicated thread that runs `tokio_uring::start` for
  the duration of `DeltaSync::run` on Linux only; HTTP serve runs
  on the main tokio multi-thread runtime (no contamination).

D-3 is the only shape with an acceptable surface area for a
single-developer project, **if** the speedup justifies it.

## Decision Drivers

- **Honest evidence over architectural fashion.** io_uring is
  trendy; the corpus walker is dominated by `blake3` (CPU) and
  ONNX embedding (CPU). Whether io_uring helps a specific phase
  here is an empirical question.
- **Acknowledge that ADR-0017 collapses the question.** With the
  mtime + size short-circuit landing first, the IO calls in idle
  delta-sync drop by ~99 % — most passes never read file content
  at all. Any IO-layer speedup applies only to the rare "file
  actually changed" case and the cold-boot first reindex.
- **Solo-dev maintenance budget.** A Linux-only branch in the IO
  layer doubles every change to that layer. The branch must earn
  its keep.
- **Cross-platform parity.** macOS and Windows users (currently
  one — the operator — but ADR-0014 is macOS-aware) cannot benefit
  from io_uring. Whatever we ship must continue to work the same
  on those OSes.
- **Operator's primary workstation is macOS.** Acceptance of the
  D-3 prototype benefits **only Linux deployments** (the build VM
  today; future Linux-deployed `schema-daemon` tomorrow). The
  operator's day-to-day editing on macOS sees zero benefit. This
  asymmetry is explicit so adoption is judged against the actual
  deployment surface, not the development surface.

## Considered Options

### Option A — Adopt D-3 immediately (rejected without evidence)

Write the dual-impl, measure later. Risks: the operator pays the
maintenance debt forever even if the gain turns out to be marginal.
ADR-0011 §Concurrency, ADR-0017, and ADR-0018 already address
the operator-reported high-CPU symptom; bolting io_uring on top
without measurement is optimisation at the wrong layer.

### Option B — Reject io_uring outright (rejected)

Closes a door without evidence. The operator's instinct (use the
best primitive) is correct in spirit; the engineering question is
whether the cost-benefit holds for our workload.

### Option C — Bench, then decide (chosen)

Run a controlled bench on the Linux build VM (kernel 6.17, see
operator-private memory `reference_linux_build_vm.md`) comparing
four configurations on the same fixture corpus:

| Pass | Configuration                         | Expectation                          |
|------|---------------------------------------|--------------------------------------|
| P1   | tokio + mio, status quo               | baseline                             |
| P2   | + ADR-0017 (mtime+size short-circuit) | ~99 % fewer hash calls in pass 2     |
| P3   | + ADR-0018 (cap ONNX threads)         | predictable CPU envelope             |
| P4   | + D-3 tokio-uring (Linux thread for delta-sync) | unknown          |

Adoption rule: **P4 vs P3** must show **> 10 % wall-clock speedup
on cold reindex AND P3's cold-reindex wall clock must still exceed
5 s** for D-3 to be adopted. If P3 already brings cold reindex
under 5 s on the lowcow-platform fixture, the case for D-3
collapses and this ADR closes `rejected`.

## Decision Outcome

**Option C — bench-driven gate.** This ADR records the bench plan,
the adoption rule, and the consequences of either outcome. It does
**not** authorise implementing D-3 unconditionally. Any code change
toward D-3 happens only after the bench runs and its result clears
the threshold.

### Bench plan

Fixture: clone of `lowcow-platform` corpus into the VM
workspace `/mnt/volumes/build/bench-fixtures/lowcow-platform/`,
~5 K Markdown files. Reproducible.

For each of P1-P4, **a shell wrapper** at
`bench/uring_eval/run.sh` performs the steps that need elevated
privileges or non-Rust tooling, then delegates the timing and
HTTP load to a Rust binary `bench/uring_eval/src/main.rs`:

1. (shell) Drops the page cache:
   `echo 3 | sudo tee /proc/sys/vm/drop_caches`. Done in shell
   because `cargo run` does not (and should not) escalate.
2. (shell) Removes the project cache:
   `rm -rf /mnt/volumes/build/cache/schema/projects/<id>-*`.
3. (shell) Starts `iostat -d -x 1` and `pidstat -d -p <pid> 1`
   in the background, captures their output to per-pass log
   files for later analysis.
4. (Rust) Runs `schema serve` (post-ADR-0019 HTTP build) with
   `RUST_LOG=info`.
5. (Rust) Issues an MCP `initialize` then a `vector_search` over
   HTTP to force the embedder path; ignores results, only
   timing.
6. (Rust) Captures: cold-reindex wall clock, second-pass wall
   clock, peak RSS, peak %CPU, total `delta_sync` `tracing`
   span duration, **plus** the IO time fraction parsed from the
   captured `pidstat -d` log.
7. (shell) Tears down iostat/pidstat. Repeats 5 runs; reports
   median + 95th percentile.

Bench harness: shell wrapper + single-file Rust binary in
`bench/uring_eval/`; committed alongside the
ADR-0017/0018/0019 implementation work, before any D-3
prototype is written.

### D-3 prototype scope (only built if P3 baseline does not satisfy)

If P3 alone does not push cold reindex below 5 s, the operator
authorises a minimal D-3 prototype:

- New module `src/adapters/uring_metadata_store.rs`,
  `#[cfg(target_os = "linux")]`. Implements the existing
  `MetadataStore` port using `tokio_uring::fs::File::statx` for
  mtime/size and `read_at` chunks fed into `blake3::Hasher`.
- `DeltaSync::run` on Linux spawns a `std::thread` that calls
  `tokio_uring::start` and runs `walker.discover() ->
  classify_file -> hash` inside that runtime. macOS / non-Linux
  paths use the existing sync `metadata_store`.
- Channel bridge between the main tokio runtime and the uring
  thread; results merged before continuing the rest of the
  delta-sync (chunker, embedder, persistence — unchanged).

Maintenance cost estimate: ~500 lines of new code, ~150 of which
are tests, plus ongoing dual-implementation upkeep.

### Adoption rule (binding)

1. Run the four-pass bench on the VM.
2. **IO-floor sanity check** (computed from P3 `pidstat -d`
   capture): if file IO time (read + write) is **less than 20 %
   of P3 cold wall clock**, the speedup ceiling for *any*
   IO-layer change is mathematically below 20 %. In that case the
   ADR closes `rejected` immediately — io_uring would optimise a
   non-bottleneck. **Skip steps 3-5.**
3. **Anti-pattern check** (validates the D-3 prototype itself):
   the P4 prototype's uring-side IO must report **at least 2×
   the IOPS** of the corresponding tokio (read syscall) path
   captured from P3, on the same files. If not, the prototype is
   broken (likely throttled by the channel bridge between
   runtimes); fix the prototype before declaring P4 measured.
4. Compute `speedup = (P3_cold_wall - P4_cold_wall) / P3_cold_wall`.
5. **Adoption gate** (no arbitrary wall-clock floor — replaced
   the previous "P3 > 5 s" floor with a deployment-relevance
   check):
   - `speedup > 0.10` (the percentage threshold survives), **AND**
   - **estimated weekly time saved on the operator's deployment
     pattern > 30 seconds**, computed as
     `(P3_cold_wall - P4_cold_wall) * cold_starts_per_week +
      (P3_warm_wall - P4_warm_wall) * watcher_batches_per_week`.
   - This replaces the discontinuous `P3 > 5s` floor with a
     deployment-pattern saving that smoothly tracks reality.
6. If both gates pass → this ADR accepts; D-3 prototype gets
   promoted to a real adapter behind
   `#[cfg(target_os = "linux")]`. A separate ADR (ADR-0023+)
   records the implementation details. ADR-0022 stays accepted
   as the gate that proved it.
7. Otherwise → this ADR closes `rejected`. tokio + mio is final
   for the foreseeable future. Reopening requires a new ADR with
   new evidence (e.g., kernel 7.x changes, new io_uring features,
   deployment pattern shift toward Linux-heavy).

## Consequences

- **Good (either outcome):** the io_uring question is settled by
  measurement, not vibes.
- **Good (rejection branch):** zero code change to the project,
  zero ongoing maintenance debt.
- **Good (acceptance branch):** Linux operators get a measurable
  speedup on cold reindex; the gate ensures it is real.
- **Neutral:** the bench harness committed in `bench/` becomes a
  reusable fixture for any future IO-layer optimisation question
  (e.g., parallelizing walker, batching `statx`).
- **Bad:** the bench costs operator time (4-8 hours including
  fixture setup, prototype, run, analysis). Bounded.

## Fitness function

This ADR's fitness function **is** the bench result. Concretely:

- Bench harness lives at `bench/uring_eval/` and is invokable via
  `cargo run --release -p uring-eval` (or a documented script).
- Bench results table lives at the top of this ADR's
  `Evidence and amendments` section, populated when the bench
  is run.
- The adoption rule above is the literal CI check: if a future
  pull request asserts D-3 should be merged, the rule above
  decides accept/reject.

## Cross-references and follow-ups

- **ADR-0017 — mtime+size short-circuit.** Must land before the
  bench. Its effect is the largest single contributor to the
  expected baseline reduction.
- **ADR-0018 — cap ONNX threads.** Must land before the bench.
  Establishes the steady-state CPU envelope.
- **ADR-0019 — HTTP transport.** Must land before the bench.
  Eliminates duplicated processes that would otherwise contaminate
  bench numbers.
- **Follow-up ADR-0023 (conditional).** Records implementation
  details only if this ADR's adoption rule fires.

### Why not adopt unconditionally

The operator's single-developer maintenance budget cannot absorb a
permanent dual-implementation in IO without a measured payoff.
ADR-0017 alone already reduces the IO surface to ~1 % of pre-ADR
volume; the IO speedup applies to that 1 % plus cold boot. If
cold boot is already fast enough post-ADR-0017+0018, io_uring
buys nothing the operator will feel.

### Why not reject unconditionally

The operator instinct ("use the best primitive") is correct.
Refusing to even check is engineering by prejudice. The bench is
cheap; running it costs less than the meta-discussion would.

## Evidence and amendments

- **2026-04-26 — bench prerequisites landed.** ADR-0017 (mtime+size
  short-circuit), ADR-0018 (embedder nice cap), and ADR-0019 (HTTP
  transport) are implemented and gate-green. The lowest-risk
  contributors to the four-pass plan are therefore in place; the
  P3 baseline ("tokio + mio + ADR-0017 + ADR-0018") can be measured
  whenever the operator runs the new binary against a real corpus.
- **2026-04-26 — bench harness deferred.** The runner skeleton at
  `bench/uring_eval/` (shell wrapper + Rust binary) is intentionally
  not written yet. Rationale: per ADR-0022's adoption rule, the
  bench is only worth building once the P3 baseline shows >5 s
  cold reindex on the operator's actual workload. The operator
  needs to migrate at least one consumer project to the new HTTP
  transport (per the runbook in `arch/operations/`), measure
  `delta-sync complete` wall-clock from `tracing` logs, and decide
  whether the IO-floor sanity check (file IO > 20% of P3 wall
  clock) plausibly holds. If yes, the harness lands as a follow-up
  in this ADR's `Evidence` section; if no, this ADR closes
  `rejected` per the adoption rule.
- **(pending — bench has not yet been run)**
  Will be populated with the four-pass numbers + adoption decision
  the day the bench runs.
