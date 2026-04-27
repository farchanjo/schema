---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0014 — Install at `/usr/local/bin` + Apple codesign on macOS

> **Y-statement** — In the context of distributing the `schema`
> binary to the operator's macOS dev machines (and eventually
> to teammates' machines once the tool stabilises) so that
> Claude Code can spawn it via `command = "schema"` from any
> consumer project's `.mcp.json`, facing the choice between
> (a) **`cargo install --path .` into `~/.cargo/bin/`** (the
> Rust default; per-user prefix; relies on `~/.cargo/bin` being
> in `PATH`; produces an ad-hoc-signed Mach-O), (b) **install
> at `/usr/local/bin/`** with the binary signed using the
> operator's Apple Development identity (system-wide; `PATH`
> includes it by default on macOS; Gatekeeper-friendly), or
> (c) **distribute via Homebrew tap** (clean for teammates but
> requires bottle/formula maintenance and is overkill for a
> single-operator FASE-1.0 tool), we decided for **(b) install
> at `/usr/local/bin/schema` codesigned with the operator's
> Apple Development identity**, against (a) (each shell init
> needing `~/.cargo/bin` on `PATH`; no signature beyond
> ad-hoc; Claude Code subprocess inheritance is sometimes
> shell-config-sensitive) and (c) (Homebrew machinery is
> disproportionate for a single-operator tool that has not
> shipped FASE 1.1 yet), to achieve a deterministic,
> system-wide command name that any Claude Code session and
> any shell finds without per-user `PATH` setup, plus a
> Gatekeeper-friendly signature that survives macOS quarantine
> on download / move / scp, accepting that the install step
> requires `sudo` (writing under `/usr/local/`), that the
> signature ties the binary to the operator's developer
> certificate (re-signing required if the certificate
> rotates), and that a future Homebrew tap can replace this
> via a fresh ADR if/when teammates adopt the tool.

## Context and Problem Statement

`schema` is a single Rust binary spawned by Claude Code via
stdio (per ADRs 0001/0002). Each consumer project's
`.mcp.json` declares `"command": "schema"` — the tool must
resolve through the operator's `PATH` without per-shell
configuration. Two operational concerns:

1. **`PATH` resolution.** `~/.cargo/bin/` is in `PATH` only
   if the shell init (zshrc / bash_profile) sources Cargo's
   env script. Claude Code spawns the MCP server in a
   subprocess that inherits the user's shell environment —
   usually fine, but occasional reports of "command not
   found" when the shell is non-interactive or when a
   direnv/asdf shim shadows the cargo bin.
2. **Code signature.** The default Rust release build on
   Apple Silicon produces an **ad-hoc** signature
   (`linker-signed`). That is sufficient for first-launch
   on the same machine that built it, but moving the binary
   between machines (or via `scp`) triggers Gatekeeper's
   quarantine bit and macOS prompts the operator. A signed
   binary with a real identity skips the prompt.

`/usr/local/bin/` is on every macOS shell's default `PATH`,
is owned by `root:wheel` (so installation is intentional —
`sudo`-gated), and is the conventional location for
locally-built developer tools. The operator already has an
Apple Development certificate keychained for Xcode work.

## Decision Drivers

- **`PATH` determinism.** `/usr/local/bin/` is on every
  macOS user's `PATH` out of the box; no shell-init dance.
- **Cross-machine portability.** `scp ./schema other-mac:`
  followed by a launch must not trigger a Gatekeeper prompt.
  An Apple Development signature avoids that.
- **Single-operator simplicity.** Today's surface is one
  developer on one or two machines. A signing identity they
  already own is the right tool. Notarization (which Apple
  requires only for distribution to *other* users outside
  the App Store) is unnecessary today.
- **Future-proof for teammates.** If/when teammates adopt
  the tool, swap `Apple Development` → `Developer ID
  Application` and add notarization in a follow-up ADR.
  Today's choice does not block that.

## Considered Options

### Option A — `cargo install --path .` to `~/.cargo/bin/` (rejected)

Pros: zero ceremony; idiomatic Rust; no `sudo`. Cons:
ad-hoc signature only; relies on `~/.cargo/bin` being in
`PATH` for every shell context Claude Code might spawn under;
moving the binary off the build machine triggers Gatekeeper.

### Option B — `/usr/local/bin/schema` + Apple Development codesign (chosen)

```bash
cargo build --release
codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
         --options runtime \
         --force \
         target/release/schema
sudo install -m 0755 target/release/schema /usr/local/bin/schema
codesign --verify --verbose=2 /usr/local/bin/schema
```

Pros: predictable `PATH`; signature survives transport;
operator's existing Apple Development cert is enough. Cons:
`sudo` required; the binary is tied to the developer
certificate (re-sign on cert rotation; tracked as a
follow-up); the cert lives in the operator's keychain and is
not portable to CI by default.

### Option C — Homebrew tap (rejected for now)

Pros: clean teammate onboarding (`brew install <tap>/schema`);
versioning and uninstall by Homebrew. Cons: requires writing
+ maintaining a formula; bottling for arm64 + x86_64; the tool
is single-operator FASE 1.0 — premature. Reconsider after
FASE 1.1 if teammates adopt.

## Decision Outcome

Chosen option: **B — `/usr/local/bin/schema` + Apple
Development codesign** on the operator's primary signing
identity.

### Install procedure (canonical)

```bash
# From the schema repo root:
cargo build --release
codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
         --options runtime \
         --force \
         target/release/schema
sudo install -m 0755 target/release/schema /usr/local/bin/schema
codesign --verify --verbose=2 /usr/local/bin/schema
schema --version
```

The `--options runtime` flag enables the Hardened Runtime;
combined with the Apple Development identity it satisfies the
local Gatekeeper for *this* developer's machines without
notarization.

### Re-install / upgrade

Same procedure. `install -m 0755` overwrites atomically.
Existing Claude Code sessions continue running the old
binary in memory until they restart; new sessions pick up
the new one.

### Cert rotation

Apple Development certs renew yearly. When the cert rotates:

1. Re-build (`cargo build --release`).
2. Re-sign with the new identity name (the SHA hash in the
   identity changes; the human-readable name does not).
3. `install` again.

If the operator forgets and the cert expires, the binary
keeps working (Gatekeeper checks the signature at install
time, not at every launch). Fresh installs need a current
cert.

## Consequences

- **Good:** `PATH` always resolves `schema` without shell
  config dancing.
- **Good:** Binary is portable to other Macs the operator
  owns without Gatekeeper prompts.
- **Good:** Hardened Runtime hardens against common dyld
  injection / trust attacks.
- **Bad:** Install requires `sudo`. The runbook documents
  the exact one-liner.
- **Bad:** Binary is tied to the operator's signing identity.
  CI / teammate machines cannot run an installer that re-
  signs without holding the same certificate. This is fine
  for FASE 1.0 (one operator).
- **Neutral:** `/usr/local/bin/schema` shadows any
  `~/.cargo/bin/schema` left over from past `cargo install`
  runs. Recommend cleaning that up: `rm
  ~/.cargo/bin/schema` once after first `/usr/local/bin`
  install to avoid `which -a schema` ambiguity.

## Fitness function

- **Verify signature:** `codesign --verify --verbose=2
  /usr/local/bin/schema` exits 0 and does not print
  `not signed at all` / `invalid Authority`. The operator's
  identity is present in the displayed Authority chain.
- **Path resolution:** `which schema` returns
  `/usr/local/bin/schema` (single hit). If a `~/.cargo/bin`
  copy lingers, the path order on macOS still gives priority
  to `/usr/local/bin`; this is the contract.
- **Hardened runtime:** `codesign --display --verbose=2
  /usr/local/bin/schema | grep flags=` includes `runtime`.
- **Manual smoke:** `schema --version` prints `schema
  0.1.0` with no Gatekeeper prompt on a clean shell.

## More information

- ADR-0001 — Rust + Cargo binary (the build step).
- ADR-0003 — multi-project architecture (why `PATH`
  resolution matters: every consumer's `.mcp.json` says
  `"command": "schema"`).
- `arch/operations/runbook.md` — step-by-step install in
  the **Install** section (updated by this ADR).
- Apple's `codesign(1)` and `security find-identity` manpages.

## Follow-ups

- **Cert rotation reminder.** Apple Development certs renew
  yearly. Add a calendar reminder; the signature itself
  does not expire after install but a fresh install needs a
  current cert.
- **Homebrew tap.** When teammates adopt the tool,
  re-evaluate Option C and write a fresh ADR.
- **Notarization.** Required only when distributing the
  binary to other users outside the App Store. Defer.
- **CI signing.** A future CI build that publishes
  releases would need a Developer ID Application cert in a
  GitHub Actions secret + the `codesign-and-notarize`
  workflow. Out of scope for FASE 1.0.

## Evidence and amendments

- _2026-04-25 — Initial recording. Operator already had the
  cert "Apple Development: Fabricio Fonseca (J3LVNXCU3U)"
  keychained from Xcode work; `/usr/local/bin/schema` was
  the existing install location from a prior `sudo cp` run
  (the new procedure formalises and codesigns it). The
  current `target/release/schema` build is ad-hoc-signed
  (`linker-signed,adhoc`) and gets re-signed on install._

- **2026-04-26 — Codesign re-ordered: sign IN PLACE at
  `/usr/local/bin/schema` (not at the cargo output).** Original
  procedure signed `target/release/schema` and then
  `sudo install`-copied it to `/usr/local/bin/`. Two practical
  problems with that order:

  1. The `install` step copies the file. Codesign signatures
     live in the Mach-O `__TEXT,__signature` section (not in
     extended attributes), so they survive a plain `cp`/`install`.
     But some macOS filesystem combinations (APFS → SMB → APFS)
     and certain `install` flags can drop trailer xattrs on the
     destination, leading to verify failures. Signing at the
     destination removes the variable.
  2. Cargo's output may carry `com.apple.quarantine` (typical
     after building from a `git clone` of a downloaded zip).
     That flag survives `install` and triggers Gatekeeper on
     first run. Signing at the final path with `sudo` (root
     does not inherit quarantine) gives a clean signature.

  **Revised procedure:**

  ```bash
  cargo build --release
  sudo install -m 0755 target/release/schema /usr/local/bin/schema
  sudo codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
                --options runtime --force \
                /usr/local/bin/schema
  codesign --verify --verbose=2 /usr/local/bin/schema
  ```

  `sudo codesign` is required because `/usr/local/bin/schema`
  is root-owned. The verify step (no sudo) confirms the
  signature outside the privileged context — fitness function
  must still exit 0 with the developer's identity in the
  Authority chain.

- **2026-04-26 — Codesign explicitly marked optional.** The
  original ADR text framed codesign as a hard requirement.
  Realistic posture: codesign + Hardened Runtime is the right
  default for the primary operator (Apple Development identity
  available), but the binary runs unsigned just fine —
  Gatekeeper warns once on first launch and can be cleared
  with `xattr -d com.apple.quarantine /usr/local/bin/schema`
  or via right-click → Open. Operators without an Apple
  identity (CI runners, contributors without a Developer
  Program subscription, Linux-only builds) skip the codesign
  step:

  ```bash
  cargo build --release
  sudo install -m 0755 target/release/schema /usr/local/bin/schema
  # codesign skipped; binary runs, Gatekeeper warns once on first launch.
  ```

  **Caveat for service mode (ADR-0020 launchd).** A LaunchAgent
  whose `ExecStart` points at an *unsigned* binary loads on
  current macOS, but the Hardened Runtime flag in the plist
  (equivalent to `--options runtime`) may have stricter
  behaviour on a future Gatekeeper release. Operators skipping
  codesign should drop the Hardened Runtime expectation from
  the plist or accept that the LaunchAgent may stop loading on
  a future macOS update. Documented in
  `arch/operations/runbook.md`.
