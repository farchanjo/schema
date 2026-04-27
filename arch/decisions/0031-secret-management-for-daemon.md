---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0020", "ADR-0025", "ADR-0027"]
---

# 0031 — Secret management for the schema daemon (LLM provider keys)

> **Y-statement** — In the context of ADR-0025 selecting LLM
> provider via `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` environment
> variables and ADR-0020/ADR-0027 running the daemon under
> `launchd` (macOS) or `systemd --user` (Linux) with a rendered
> service unit file, and the current rendered macOS plist
> (`~/Library/LaunchAgents/com.farchanjo.schema.daemon.plist`)
> embedding the live `ANTHROPIC_API_KEY` literal value inside an
> `<EnvironmentVariables>` block, and the macOS default
> `LaunchAgents/*.plist` permission profile being world-readable
> (`-rw-r--r--`) so any local user, Time Machine backup, support
> tarball, or in-process file-read tool surfaces the bearer in
> plaintext, facing the choice between
> (a) **status quo: literal key in the plist** (simple; works;
> leaks on any plist read, on Time Machine backups, on `tar`
> archives shared for support, and on AI/agent file-read tool
> calls — exactly the failure mode that already happened),
> (b) **`chmod 600` on the plist** (still readable to anyone
> with the operator's UID — every other process the operator
> runs, including agents; does not address backup/archive
> leakage), (c) **macOS Keychain entry read at daemon startup**
> (key never on disk in plaintext; macOS-native secret store;
> does not solve Linux), (d) **mode-`0600` sidecar TOML**
> (`~/Library/Application Support/schema/secrets.toml` /
> `~/.config/schema/secrets.toml`) **read by the daemon at
> startup and on `SIGHUP`** (cross-platform; matches the
> `endpoint.toml` pattern from ADR-0021; still on disk but
> isolated and gitignore-able; rotation by editing one file),
> we decided for **(d) as the canonical mechanism, with (c)
> available as an opt-in macOS-only enhancement gated behind a
> follow-up ADR if Keychain access becomes worth the
> extra-platform-divergence cost**, against (a) (world-readable
> plist plus Time Machine and tarball capture surfaces is a
> structural leak vector — not a hypothetical risk),
> (b) (does not stop in-process secret-reachable agents — the
> very class of consumer this daemon serves), and pure-(c)
> (Linux gap; macOS Keychain access from a long-running
> launchd agent requires either user-session interaction or
> upgraded entitlements not currently part of ADR-0014's
> codesign profile), to achieve **a uniform cross-platform
> secret-loading path that keeps API keys out of service unit
> files, out of `~/Library/LaunchAgents`, out of
> `~/.config/systemd/user`, and out of any artefact a
> file-read agent might surface in transcript**, accepting
> **(1) one new file (`secrets.toml`) the operator must
> author once and `chmod 600` (the daemon enforces and
> repairs `0600` on startup), (2) a 30-day transition window
> where the legacy `EnvironmentVariables` plist shape still
> works for backwards compatibility — the daemon prefers
> `secrets.toml` when present and falls back to env, logging
> a `WARN` on the env path, and (3) the rendered service
> unit no longer carries any secret — only the path to
> `secrets.toml` (or no env at all when the daemon
> discovers the file at the canonical location).**

## Context and Problem Statement

ADR-0025 made the daemon **pluggable across LLM providers** via
env vars: `ANTHROPIC_API_KEY` for Anthropic, `OPENAI_API_KEY`
for OpenAI. ADR-0020 (later amended by ADR-0027) made the
daemon a permanent service rendered into a per-platform unit
file. The composition today injects the secret straight into the
unit:

```xml
<key>EnvironmentVariables</key>
<dict>
    <key>PATH</key>          <string>/usr/local/bin:/usr/bin:/bin</string>
    <key>ANTHROPIC_API_KEY</key>
    <string>sk-ant-api03-…AAA</string>   <!-- LITERAL VALUE -->
    <key>SCHEMA_LLM_PROVIDER</key>      <string>anthropic</string>
    <key>SCHEMA_LLM_MODEL</key>
    <string>claude-haiku-4-5-20251001</string>
</dict>
```

`~/Library/LaunchAgents/*.plist` is world-readable by macOS
default (`-rw-r--r--`). Time Machine backs it up. Support
tarballs sweep it up. Any process running as the operator UID
can `cat` it without prompt. Any agent or tool the operator
hands a file path to can read it — service unit files are
read for routine diagnostics; secret files are not.

The same hazard exists for Linux `~/.config/systemd/user/
schema-*.service` (`Environment=ANTHROPIC_API_KEY=…`), and for
any future provider key (OpenAI, Cohere, Mistral) the daemon
would consume per ADR-0025.

The cost of (a) is no longer hypothetical. ADR-0021's
`endpoint.toml` pattern (mode `0600`, project-cache directory,
schema-versioned, daemon-managed) is already the proven shape
for a daemon-only-readable artefact in this codebase. We extend
the pattern to LLM secrets.

## Decision drivers

- **Stop the leak class.** Secrets must be absent from any
  file an agent or operator has reason to read. Service unit
  files are read for diagnostics; secret files are not.
- **Cross-platform parity.** ADR-0027 keeps the daemon
  cross-platform (macOS launchd, Linux systemd-user). The
  secret loader must work the same on both; macOS-only
  Keychain integration may come later but cannot be the
  default.
- **Reuse the `endpoint.toml` pattern.** A `0600` TOML in
  `~/Library/Application Support/schema/` (macOS) /
  `~/.config/schema/` (Linux) is exactly the shape ADR-0021
  established. Operators already understand it; the daemon
  already enforces `0600` semantics.
- **Backwards compatibility window.** Existing operators
  have working `EnvironmentVariables` blocks. A clean cut
  would break them. A 30-day soft-deprecate window with a
  `WARN` log gives time to migrate.
- **Honor ADR-0023 ENV overlay.** `SCHEMA_*` env vars
  remain the explicit override path — `secrets.toml` is the
  default file source, env is the operator-explicit
  override. Precedence: `ENV > secrets.toml > none`.

## Considered options

### Option (a) — Status quo (literal key in service unit)
- **Rejected.** Leak class is structural to the choice of
  storage location, not contingent on a specific incident:
  the plist is world-readable; any reader trivially exfils
  the bearer.

### Option (b) — `chmod 600` on the service unit
- Closes the multi-user leak. Does **not** close the
  same-UID-agent leak (every process running as the
  operator can still read the plist).
- launchd plist permissions are tolerated at `0600` on
  macOS; tested.
- **Rejected** — does not address the structural failure
  mode this ADR exists to close: same-UID readers
  (including in-process agents) bypass file-mode bits.

### Option (c) — macOS Keychain entry read at daemon startup
- macOS-only. Linux gap forces a different path or a
  second ADR.
- launchd agents starting at login can access the
  user's Keychain after first unlock; before first
  unlock the daemon has to wait or fail.
- Codesign profile per ADR-0014 (`Apple Development:
  Fabricio Fonseca…`, `--options runtime` Hardened
  Runtime) is compatible with Keychain access via
  `SecKeychainItemCopyContent`, but the entitlement
  surface widens.
- Best-in-class on macOS; defer to follow-up ADR.
- **Held as a follow-up enhancement**, not the default.

### Option (d) — Mode-`0600` `secrets.toml` sidecar
- Cross-platform via the same path-resolution rules as
  `endpoint.toml`:
  - macOS: `~/Library/Application Support/schema/secrets.toml`
  - Linux: `${XDG_CONFIG_HOME:-~/.config}/schema/secrets.toml`
- File schema:

  ```toml
  version = 1

  [llm.anthropic]
  api_key = "sk-ant-…"

  [llm.openai]
  api_key = "sk-…"
  ```

- Daemon reads on startup; rejects unknown major versions
  (same shape as `endpoint.toml`).
- Daemon enforces `0600` mode on the file before reading
  the first byte; if mode is wrong, daemon either repairs
  it (`chmod 600`) or refuses to start with a clear
  diagnostic — operator preference, default `repair`.
- `SIGHUP` triggers a re-read so rotation does not need
  a daemon restart.
- Service unit file no longer carries any secret. The
  rendered plist for the daemon shrinks to:

  ```xml
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key> <string>/usr/local/bin:/usr/bin:/bin</string>
    <key>SCHEMA_LLM_PROVIDER</key> <string>anthropic</string>
    <key>SCHEMA_LLM_MODEL</key>
    <string>claude-haiku-4-5-20251001</string>
  </dict>
  ```

- Provider selection (`SCHEMA_LLM_PROVIDER`) and
  non-secret model knobs stay in the unit; **secrets
  alone migrate**.
- **Selected.**

## Decision

We adopt **option (d)** as the default, single-source secret
loader for the daemon.

1. **Add `secrets.toml`** at the canonical path
   (`~/Library/Application Support/schema/secrets.toml` on
   macOS; `${XDG_CONFIG_HOME:-~/.config}/schema/secrets.toml`
   on Linux). Mode `0600`, owner = operator UID. Schema
   versioned (`version = 1`).

2. **Implement `src/adapters/secrets_toml.rs`** as an
   outbound adapter behind a new application port
   `SecretStore` (Hexagonal per ADR-0013): `fn
   read_provider_key(&self, provider: ProviderId) ->
   Option<String>`. Domain never sees the file path.

3. **Wire `SecretStore` into the `LlmProvider` factory**
   established by ADR-0025. Resolution precedence:
   `ENV (SCHEMA_LLM_*) > secrets.toml > absent`. When
   absent, the `synthesize` tool hides itself from
   `tools/list` per ADR-0025's existing silent-degrade
   contract.

4. **Render service unit files without literal secrets.**
   `schema install --daemon` (ADR-0027) emits a unit that
   sets `SCHEMA_LLM_PROVIDER` and `SCHEMA_LLM_MODEL` only.
   Existing units carrying `ANTHROPIC_API_KEY` /
   `OPENAI_API_KEY` keep working during the deprecation
   window (see step 6); new installs do not.

5. **`SIGHUP` re-reads `secrets.toml`.** Operator can
   `kill -HUP $(launchctl print gui/$(id -u)/com.farchanjo.
   schema.daemon | awk '/pid =/ {print $3}')` to rotate
   without a service restart. Token rotation in
   `endpoint.toml` is unaffected.

6. **30-day soft deprecation of env-in-unit secrets.**
   When the daemon detects `ANTHROPIC_API_KEY` /
   `OPENAI_API_KEY` in process env on startup, it logs
   `WARN secrets: provider key found in process
   environment; migrate to ~/Library/Application Support/
   schema/secrets.toml — env-source will be removed
   2026-05-27`. Behaviour unchanged within the window.
   After 2026-05-27 the daemon ignores the env source and
   only `secrets.toml` is consulted (a follow-up
   `Evidence and amendments` entry will pin the cutover
   build).

7. **Migration tool: `schema secrets migrate`** —
   one-shot subcommand that reads the current process
   env (or the rendered service unit file) and writes
   `secrets.toml` with `0600` perms. Idempotent. Refuses
   to overwrite an existing file unless `--force`.

8. **`secrets.toml` is gitignored** in any consumer
   tree, although it lives outside repo roots in the
   canonical path. This ADR adds a runbook note; no
   repo-side gitignore change is required for `schema`
   itself (the canonical path is outside the working
   tree).

9. **Codesign profile unchanged.** `secrets.toml` reads
   require nothing beyond standard POSIX `open(O_RDONLY)`
   in the operator's home; ADR-0014 Hardened Runtime
   profile remains correct.

## Consequences

- **Good:** secrets leave the service unit. The structural
  leak vectors (agent reading the plist; Time Machine
  backup of the plist; support tarball with the plist) are
  closed.
- **Good:** cross-platform parity. macOS and Linux
  daemons read the same shape from the same kind of file.
- **Good:** consistent with ADR-0021's `endpoint.toml`
  pattern; reuses the path resolution, mode-`0600`
  enforcement, and version-pinning idioms.
- **Good:** rotation does not require restart (`SIGHUP`).
- **Neutral:** secret is still on disk, just in a more
  defensible place. macOS Keychain is strictly stronger
  and remains a follow-up.
- **Neutral:** during the 30-day window, two sources of
  truth exist. The `WARN` log makes the migration
  explicit.
- **Bad:** operator must hand-author `secrets.toml` (or
  run `schema secrets migrate`) the first time. One-time
  step.
- **Bad:** if the operator's home is on a network share
  (NFS, SMB) without POSIX mode bits, `0600` enforcement
  is ineffective. Out of scope; document in the runbook
  with a `WARN` recommending Keychain (post-follow-up
  ADR) for that environment.

## Fitness function

- **Unit test (`secrets.toml` reader):** parses the
  `version = 1` shape; rejects unknown major versions;
  returns `None` for absent provider keys; round-trips
  `provider = anthropic` and `provider = openai`.
- **Unit test (mode enforcement):** writer creates the
  file with `OpenOptions::mode(0o600)` and rejects an
  existing file with broader perms unless `--repair` was
  passed.
- **Integration test (precedence):** `SCHEMA_LLM_*` env >
  `secrets.toml` > none. Asserts the resolved
  `LlmProvider` matches the higher-precedence source
  when both are set.
- **Integration test (`SIGHUP` rotation):** spawn
  daemon with `secrets.toml` carrying key A → invoke
  `synthesize` → swap `secrets.toml` to key B (via
  atomic-rename to preserve `0600`) → send `SIGHUP` →
  invoke `synthesize` → assert the second call hit
  the second key (validated by Anthropic's stub /
  recording HTTP test fixture).
- **Lint (no secret in service unit):** CI grep gate
  against rendered service-unit fixtures —
  `grep -E '(api_key|API_KEY)\s*=' tests/fixtures/
  rendered_units/*.plist tests/fixtures/rendered_units/
  *.service && exit 1 || true`.
- **Doc lint (runbook):** `arch/operations/runbook.md`
  carries a "Secrets" subsection pointing at
  `secrets.toml`; the install playbook no longer
  instructs the operator to set `ANTHROPIC_API_KEY` in
  the unit file.
- **Audit lint (env-source `WARN`):** during the
  deprecation window, integration test asserts the
  `WARN secrets: provider key found in process
  environment` line appears on stderr exactly once per
  daemon startup when env-source is the only source.

## Cross-references and follow-ups

- **ADR-0013 — Hexagonal.** `SecretStore` is a new
  application port; `secrets_toml::FileSecretStore` is
  the outbound adapter. No domain changes.
- **ADR-0020 / ADR-0027 — service lifecycle.** Service
  unit rendering is updated; lifecycle semantics
  unchanged.
- **ADR-0021 — bearer auth.** `secrets.toml` adopts the
  exact mode-enforcement and version-pinning pattern
  this ADR proved out for `endpoint.toml`. No bearer
  validator change.
- **ADR-0023 — ENV overlay.** `SCHEMA_LLM_*` env stays
  the explicit override path; `secrets.toml` is the
  default file source.
- **ADR-0025 — `LlmProvider` port.** This ADR amends
  the loading mechanism; the port surface is unchanged.
  The factory consults `SecretStore` instead of
  reading the env directly.
- **Follow-up ADR (held):** macOS Keychain read at
  daemon startup. Promote when the
  Hardened-Runtime-entitlement implications of
  Keychain access are scoped, and when at least one
  operator reports `secrets.toml` mode-bit
  insufficiency (e.g., laptop-with-network-home).
- **Follow-up ADR (held):** Linux `secret-tool`
  (libsecret) integration as a parallel platform-
  native option to Keychain. Deferred until macOS
  Keychain ships.

## Evidence and amendments

- **2026-04-27 — accepted.** ADR scoped strictly to the
  forward-looking `secrets.toml` mechanism. Any
  remediation of pre-existing key exposure is a
  separate operator decision and intentionally **not**
  a precondition of this ADR.
