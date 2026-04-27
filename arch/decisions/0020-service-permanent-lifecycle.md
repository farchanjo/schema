---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
---

# 0020 — Permanent service lifecycle: launchd (macOS) + systemd user unit (Linux), one service per project

> **Y-statement** — In the context of ADR-0019 introducing an HTTP
> MCP server that needs to outlive any single Claude Code session and
> be reachable when the next session opens, facing the choice between
> (a) **auto-spawn from the first connecting client** (idle exit
> races, no parent for stderr, lifecycle entangled with the MCP
> client), (b) **operator runs a tmux/screen session manually**
> (works once, decays the instant the operator reboots), or (c)
> **OS-native service: launchd `LaunchAgent` on macOS, `systemd --user`
> unit on Linux, one service per project, installed via a new
> `schema install --service --config <path>` verb**, we decided for
> **(c)**, against (a) (lifecycle should not be a function of MCP
> client behaviour) and (b) (not survivable across reboots), to
> achieve **a server that starts on login, is supervised by the OS,
> survives Claude Code restarts and machine reboots, and exposes
> `schema service status` for operator introspection**, accepting
> **N services per N projects on launchctl/systemctl listings as
> the explicit cost of per-project isolation (ADR-0008 cache
> isolation honored)**.

## Context and Problem Statement

ADR-0019 collapses N `schema serve` processes into one HTTP server
per project. That is only valuable if the one process is reliably
running before any Claude Code session opens.

Three lifecycles were considered:

1. **Tied to MCP client** — first Claude Code spawn starts the
   server, last unloads it. Implicit shared state across clients,
   stderr lost, race when two clients open the same second.
2. **Manual** — operator runs `schema serve --http &` in a tmux
   pane. Survives until reboot, then nothing. Forgotten reliably.
3. **OS service** — `launchd` (macOS) or `systemd --user` (Linux)
   keeps the server alive, restarts on crash, captures stderr to
   a known log path, starts at user login.

Option 3 is the standard answer for long-running personal-machine
services. The `Cargo.toml` already pulls `dirs` for cache-dir
resolution, so the per-OS path lookup is trivial.

Per ADR-0008, each project has its own cache directory keyed by
project_id. Per ADR-0019, each project has its own HTTP server.
The simplest mapping is **one service per project** — an
`schema install --service` verb writes a service file whose
identifier embeds the project_id.

## Decision Drivers

- **Survives reboot.** Operator does not have to remember to start
  the server.
- **Restarts on crash.** A panic in the server should not require
  manual recovery.
- **Captured logs.** stderr goes to a known location (launchd or
  systemd journal) rather than vanishing.
- **Operator legibility.** `launchctl list | grep schema` or
  `systemctl --user status schema@<project>` returns useful
  information.
- **Per-project isolation.** A bug in one project's server cannot
  derail another project's index (ADR-0008).
- **Cross-platform symmetry.** macOS (the operator's primary
  workstation, ADR-0014) and Linux (the build VM, validated 2026-04-26)
  both supported with comparable verbs. Windows out of scope until a
  consumer asks for it.
- **Hardened Runtime compatibility.** ADR-0014 codesigns the binary
  with `--options runtime` (Hardened Runtime enabled). LaunchAgents
  running a Hardened Runtime binary that binds a TCP listener on
  `127.0.0.1` do **not** require a `com.apple.security.network.server`
  entitlement — outbound network connections and loopback binds are
  unrestricted by the Hardened Runtime sandbox; only the *Sandbox*
  feature (which we do not enable) would require entitlements.
  Verified against Apple's "Hardened Runtime" docs and reproduced on
  the operator's box during ADR-0014 acceptance. No new entitlements
  needed.

## Considered Options

### Option A — Auto-spawn from MCP client (rejected)

First Claude Code connection that fails to find a running server on
the recorded port spawns one in the background. Issues:

- Stderr is captured by no parent; logs are lost.
- Two simultaneous spawns race (Claude Code session 1 and session 2
  open within milliseconds of each other).
- Idle-exit policy collides with reconnect-on-restart in MCP clients
  that hold long-lived sessions.
- Crashes lose the server until the next client connects.

### Option B — Operator runs `schema serve --http` manually (rejected)

Cheapest. Decays at the first reboot, panic, or laptop sleep cycle.
The operator's reported high-CPU debug session would have started
with "step 0: did I remember to start schema?" — friction we are
trying to remove.

### Option C — OS service per project (chosen)

Two flavours, one per OS:

- **macOS** — `~/Library/LaunchAgents/com.farchanjo.schema.<project_id>.plist`
  with `RunAtLoad=true`, `KeepAlive=true`,
  `StandardErrorPath=~/.cache/schema/projects/<id>/stderr.log`.
- **Linux** — `~/.config/systemd/user/schema@<project_id>.service`
  templated, with `Restart=on-failure`, `StandardError=journal`,
  `WantedBy=default.target`.

Installed via three new CLI verbs:

```
schema install --service --config /path/to/schema.toml
schema uninstall --service --config /path/to/schema.toml
schema service status --config /path/to/schema.toml
```

Each verb resolves `project_id` from the config (ADR-0008 identity
hash), templates the unit/plist file using `format!()`-substituted
constants, writes it, and asks `launchctl bootstrap` /
`systemctl --user enable --now`.

## Decision Outcome

**Option C — OS service per project, install via CLI verbs.**

### Service file generation

Templates live in `src/cli/install/templates/` as plain `&'static str`
constants (`include_str!`). Substitutions:

- `{binary_path}` — output of `which schema` resolved at install time
  (defaults to `/usr/local/bin/schema` per ADR-0014; operator can
  override).
- `{project_id}` — ADR-0008 identity hash.
- `{config_path}` — absolute path to `schema.toml` (must exist at
  install time).
- `{stderr_path}` — `~/.cache/schema/projects/<project_id>/stderr.log`.
- `{user}` — current user, resolved via `std::env::var("USER")` with
  `id -un` fallback when `USER` is unset (no new dep; `whoami` crate
  rejected to keep the dep tree minimal).
- `{nice}` — scheduler nice value from ADR-0018's
  `[embedding] nice` (default 5; 0..19).

### macOS plist template (sketch)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
    <key>Label</key>             <string>com.farchanjo.schema.{project_id}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary_path}</string>
        <string>serve</string>
        <string>--config</string>
        <string>{config_path}</string>
    </array>
    <key>RunAtLoad</key>          <true/>
    <key>KeepAlive</key>          <true/>
    <key>Nice</key>               <integer>{nice}</integer>
    <key>ExitTimeOut</key>        <integer>10</integer>
    <key>StandardErrorPath</key>  <string>{stderr_path}</string>
    <key>StandardOutPath</key>    <string>{stderr_path}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>           <string>/usr/local/bin:/usr/bin:/bin</string>
    </dict>
</dict>
</plist>
```

### systemd user unit template (sketch)

```ini
[Unit]
Description=schema MCP server for project {project_id}
After=default.target

[Service]
Type=simple
ExecStart={binary_path} serve --config {config_path}
Restart=on-failure
RestartSec=2s
TimeoutStopSec=10s
Nice={nice}
StandardError=append:{stderr_path}

[Install]
WantedBy=default.target
```

Initial implementation uses the **concrete unit per project**
(`schema-<project_id>.service`) rather than the systemd-idiomatic
template (`schema@<project_id>.service`). Reason: the install verb
already renders + writes one file per project; a templated form
gains nothing operationally because each project still needs its own
`{nice}`, `{stderr_path}`, and `{config_path}` substitution at
install time. The template form (`%i` placeholder) is a follow-up
only if a future feature asks for "share a single unit definition
across N projects with environment-driven differentiation".

### Install verb behaviour

```
schema install --service --config /path/to/schema.toml [--binary-path /opt/bin/schema]
```

1. Validate config (existing `schema validate`).
2. Resolve `project_id`.
3. Detect OS; pick template.
4. Render template to a temporary file.
5. Move into the OS-appropriate location.
6. **Idempotency:** if a service with the same identifier is
   already loaded, unload it first before bootstrapping the new
   plist / restarting the unit. Specifically:
   - macOS:
     `launchctl bootout gui/$(id -u)/com.farchanjo.schema.<id> 2>/dev/null; launchctl bootstrap gui/$(id -u) <plist>`
   - Linux:
     `systemctl --user daemon-reload && systemctl --user enable --now schema-<project_id>.service`
     (`enable --now` is idempotent; reload picks up changes.)
7. Print: "service `com.farchanjo.schema.<project_id>` installed,
   running on `http://127.0.0.1:<port>`; URL written to
   `~/.cache/schema/projects/<id>/endpoint.toml`."

`uninstall --service` reverses the steps. `service status` reports
the OS-supervisor state plus the contents of `endpoint.toml`.

### Graceful shutdown

The server traps SIGTERM and:

1. stops accepting new connections (axum graceful shutdown);
2. drains in-flight requests with a 10 s ceiling;
3. closes the SQLite connection (commits WAL checkpoint);
4. drops the embedder (releases ONNX memory);
5. unlinks `endpoint.toml`;
6. exits 0.

launchd and systemd send SIGTERM on `unload` and `stop` respectively.

## Consequences

- **Good:** the operator's "is schema running?" question becomes
  `launchctl print gui/$(id -u)/com.farchanjo.schema.<project>` or
  `systemctl --user status schema-<project>`.
- **Good:** crashes auto-restart with logs preserved.
- **Good:** survives reboots and laptop sleep.
- **Neutral:** the operator must run `schema install --service` once
  per project. A multi-project automation script is a follow-up.
- **Bad:** N services per N projects show up in launchctl/systemctl
  listings. Acceptable; per-project isolation honored.
- **Bad:** ADR-0014 (install + codesign) gains companion verbs.
  ADR-0014 will receive an `Evidence and amendments` entry on
  acceptance of this ADR.
- **Bad:** removing a service requires running `schema uninstall
  --service` *before* `rm -rf ~/.cache/schema/`; otherwise the
  service file points at a stale config. Documented in the runbook.

## Fitness function

The CI environment (GitHub Actions `macos-latest` / `ubuntu-latest`)
**cannot** drive `launchctl bootstrap gui/...` (no GUI session) nor
`systemctl --user enable` (no user session bus by default), so the
fitness function is **split** between CI and local validation:

- **Unit test (runs in CI, both OSes):** template rendering
  produces a syntactically valid plist (parser round-trip via the
  `plist` crate) and a syntactically valid systemd unit
  (verified via `systemd-analyze verify <path>` invoked as a
  subprocess on Linux runners; skipped on macOS runners). On
  macOS, additionally validate the rendered plist with
  `plutil -lint <plist>`.
- **Unit test (runs in CI, both OSes):** for each
  `(project_id, binary_path, config_path, stderr_path, nice)`
  permutation, render → parse → assert each substitution slot
  resolved (no leftover `{...}` placeholders).
- **Manual smoke test (operator's macOS box; logged in
  `arch/operations/0020-service-install-runbook.md`):**
  `schema install --service --config <fixture>`,
  `launchctl print gui/$(id -u)/com.farchanjo.schema.<id>` reports
  `state = running`, `curl http://127.0.0.1:$(jq -r .port
  ~/.cache/schema/projects/<id>/endpoint.toml)/health` returns 200.
  Tear down with `schema uninstall --service`.
- **Manual smoke test (build VM Linux, same runbook):**
  `schema install --service --config <fixture>`,
  `systemctl --user is-active schema-<id>.service` reports
  `active`, same `curl` smoke.
- **Smoke test (post-install, runbook):** with the consumer's
  `.mcp.json` regenerated, `claude mcp list` from a fresh
  Claude Code session reports the schema server `connected`.

## Cross-references and follow-ups

- **ADR-0014 — install + codesign.** Receives amendment recording
  the new verbs (`install --service`, `uninstall --service`,
  `service status`). Codesign procedure unchanged; the binary is
  signed once and referenced by every project's plist/unit.
  Hardened Runtime + LaunchAgent + 127.0.0.1 bind compatibility
  documented in this ADR's Decision Drivers (no new entitlements
  required).
- **ADR-0018 — embedder CPU cap.** `{nice}` substitution slot in
  both templates is wired from `[embedding] nice = N` in
  `schema.toml`.
- **ADR-0019 — Streamable HTTP transport.** This ADR is the
  lifecycle answer for the server introduced there. The three
  ADRs (0019, 0020, 0021) form an atomic accept set.
- **ADR-0021 — bearer auth.** Token written by the running server
  is read by `launchctl`/`systemctl` clients via `endpoint.toml`.
- **`arch/operations/`.** Runbook
  `arch/operations/0020-service-install-runbook.md` covers the
  install/uninstall/status verbs, the manual fitness functions
  above, and the recovery path when launchd / systemd refuses
  to start the service.
- **Follow-up (out of scope here):** Windows support. Not in scope
  until a consumer asks. Implementing it would require a Windows
  Service template (`sc.exe` or `nssm`) and a parallel install
  verb path.

## Evidence and amendments

- **2026-04-26 — implemented.** Templates landed at
  `src/cli/templates/launchd.plist.template` and
  `src/cli/templates/systemd.service.template`, embedded as
  `&'static str` constants in `src/cli/install.rs` via `include_str!`.
  Render functions `render_macos_plist` / `render_linux_unit`
  perform `{slot}` substitution; `launchd_label` and
  `systemd_unit_name` carry the per-project naming convention.
  Four CLI verbs added: `schema install --service --config X
  [--binary-path Y]`, `schema uninstall --service --config X`,
  `schema service status --config X`, `schema mcp-config --config X`.
  All four are headless-safe (they do **not** shell out to
  `launchctl bootstrap` / `systemctl --user`); the install verb
  prints the operator command needed to load the unit. Eight
  unit tests cover render + naming. `Nice={nice}` slot in both
  templates is wired from ADR-0018's `[embedding] nice` config
  field. `ExitTimeOut` (macOS) and `TimeoutStopSec` (Linux) hard-
  coded to 10 seconds per the ADR. Validation gate green.
