//! `schema install --service` / `uninstall --service` / `service status` /
//! `mcp-config` — service-lifecycle verbs introduced by ADR-0020.
//!
//! Each verb resolves the project identity (ADR-0008), templates either a
//! macOS launchd plist or a Linux systemd user unit, and either writes /
//! loads / unloads the unit or prints status. The fitness functions in
//! ADR-0020 are the manual smoke tests on the operator's box and the unit
//! tests below covering template rendering.
//!
//! Templates live as `&'static str` constants; the substitution slots are
//! `{binary_path}`, `{config_path}`, `{project_id}`, `{stderr_path}`,
//! `{nice}`. macOS plist also carries `{nice_int}` (same value, kept as a
//! separate slot to discourage accidentally substituting the path string).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::adapters::endpoint_toml::Endpoint;
use crate::adapters::project_identity::ProjectIdentity;
use crate::adapters::toml_config::SchemaConfig;

/// macOS `LaunchAgent` plist template — per-project unit (ADR-0020).
const MACOS_PLIST_TEMPLATE: &str = include_str!("templates/launchd.plist.template");

/// Linux systemd user unit template — per-project unit (ADR-0020).
const LINUX_UNIT_TEMPLATE: &str = include_str!("templates/systemd.service.template");

/// macOS `LaunchAgent` plist template — workstation-level daemon (ADR-0027).
const MACOS_DAEMON_PLIST_TEMPLATE: &str = include_str!("templates/launchd-daemon.plist.template");

/// Linux systemd user unit template — workstation-level daemon (ADR-0027).
const LINUX_DAEMON_UNIT_TEMPLATE: &str = include_str!("templates/systemd-daemon.service.template");

/// Inputs needed to render a workstation-level daemon unit (ADR-0027).
///
/// No `config_path` or `project_id` slots — the daemon resolves projects
/// lazily from each tool call's `working_directory`. `nice` and
/// `stderr_path` carry over from the per-project shape so the operator
/// keeps the same log discipline + CPU politeness across deployment
/// modes.
#[derive(Debug, Clone)]
pub struct DaemonInstallInputs {
    pub binary_path: PathBuf,
    pub stderr_path: PathBuf,
    pub nice: u8,
}

impl DaemonInstallInputs {
    /// Resolve daemon install inputs. `binary_path` defaults to the
    /// ADR-0014 install path; `stderr_path` lands under the
    /// platform-default daemon state directory next to the global
    /// `endpoint.toml` (`~/Library/Application Support/schema/`
    /// on macOS; `~/.local/state/schema/` on Linux).
    ///
    /// # Errors
    /// Returns an error if the home directory cannot be resolved.
    pub fn resolve(binary_override: Option<PathBuf>, nice: u8) -> Result<Self> {
        let binary_path = binary_override.unwrap_or_else(default_binary_path);
        let stderr_path = default_daemon_stderr_path()?;
        Ok(Self {
            binary_path,
            stderr_path,
            nice,
        })
    }
}

fn default_daemon_stderr_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        anyhow::anyhow!("could not resolve home directory for daemon stderr path")
    })?;
    let suffix: &Path = if cfg!(target_os = "macos") {
        Path::new("Library/Application Support/schema/stderr.log")
    } else {
        Path::new(".local/state/schema/stderr.log")
    };
    Ok(home.join(suffix))
}

/// Workstation-level launchd label (ADR-0027): no `<project_id>` slot.
pub const DAEMON_LAUNCHD_LABEL: &str = "com.farchanjo.schema.daemon";

/// Workstation-level systemd unit name (ADR-0027): no `<project_id>` slot.
pub const DAEMON_SYSTEMD_UNIT_NAME: &str = "schema-daemon.service";

/// Render the workstation-level daemon plist (macOS, ADR-0027).
#[must_use]
pub fn render_macos_daemon_plist(inputs: &DaemonInstallInputs) -> String {
    substitute_daemon_template(MACOS_DAEMON_PLIST_TEMPLATE, inputs)
}

/// Render the workstation-level daemon systemd unit (Linux, ADR-0027).
#[must_use]
pub fn render_linux_daemon_unit(inputs: &DaemonInstallInputs) -> String {
    substitute_daemon_template(LINUX_DAEMON_UNIT_TEMPLATE, inputs)
}

fn substitute_daemon_template(template: &str, inputs: &DaemonInstallInputs) -> String {
    template
        .replace("{binary_path}", &inputs.binary_path.to_string_lossy())
        .replace("{stderr_path}", &inputs.stderr_path.to_string_lossy())
        .replace("{nice}", &inputs.nice.to_string())
}

/// Inputs needed to render a service file.
#[derive(Debug, Clone)]
pub struct InstallInputs {
    pub binary_path: PathBuf,
    pub config_path: PathBuf,
    pub project_id: String,
    pub stderr_path: PathBuf,
    pub nice: u8,
}

impl InstallInputs {
    /// Resolve install inputs from the loaded config + identity, with a
    /// default `binary_path` derived from `which schema` (or
    /// `/usr/local/bin/schema` per ADR-0014 when `which` is unavailable).
    ///
    /// # Errors
    /// Returns an error if the config path cannot be canonicalised.
    pub fn resolve(
        config_path: &Path,
        cfg: &SchemaConfig,
        identity: &ProjectIdentity,
        binary_override: Option<PathBuf>,
    ) -> Result<Self> {
        let binary_path = binary_override.unwrap_or_else(default_binary_path);
        let canonical_config = config_path
            .canonicalize()
            .with_context(|| format!("canonicalising {}", config_path.display()))?;
        let stderr_path = identity.cache_dir.join("stderr.log");
        Ok(Self {
            binary_path,
            config_path: canonical_config,
            project_id: identity.id.to_string(),
            stderr_path,
            nice: cfg.embedding.nice,
        })
    }
}

/// Default install path defined by ADR-0014 (macOS Apple-codesigned binary).
fn default_binary_path() -> PathBuf {
    PathBuf::from("/usr/local/bin/schema")
}

/// Render the macOS `LaunchAgent` plist for these inputs.
#[must_use]
pub fn render_macos_plist(inputs: &InstallInputs) -> String {
    substitute_template(MACOS_PLIST_TEMPLATE, inputs)
}

/// Render the Linux systemd user unit for these inputs.
#[must_use]
pub fn render_linux_unit(inputs: &InstallInputs) -> String {
    substitute_template(LINUX_UNIT_TEMPLATE, inputs)
}

/// Replace `{...}` placeholders in `template` using `inputs`.
fn substitute_template(template: &str, inputs: &InstallInputs) -> String {
    template
        .replace("{binary_path}", &inputs.binary_path.to_string_lossy())
        .replace("{config_path}", &inputs.config_path.to_string_lossy())
        .replace("{project_id}", &inputs.project_id)
        .replace("{stderr_path}", &inputs.stderr_path.to_string_lossy())
        .replace("{nice}", &inputs.nice.to_string())
}

/// Service identifier per platform conventions.
///
/// macOS `LaunchAgent` label: `com.farchanjo.schema.<project_id>`.
/// Linux systemd unit name: `schema-<project_id>.service`.
#[must_use]
pub fn launchd_label(project_id: &str) -> String {
    format!("com.farchanjo.schema.{project_id}")
}

#[must_use]
pub fn systemd_unit_name(project_id: &str) -> String {
    format!("schema-{project_id}.service")
}

/// Render an `mcpServers` JSON snippet ready to paste into a consumer's
/// `.mcp.json`.
///
/// The snippet carries `"type": "http"` per the MCP client configuration
/// schema (Claude Code rejects entries that miss it with
/// `Does not adhere to MCP server configuration schema`).
///
/// Output is a fragment, not a full `.mcp.json`. The operator merges it
/// into their existing config.
#[must_use]
pub fn render_mcp_config_fragment(endpoint: &Endpoint) -> String {
    format!(
        "  \"schema\": {{\n    \"type\": \"http\",\n    \"url\": \"{}\",\n    \"headers\": {{\n      \"Authorization\": \"Bearer {}\"\n    }}\n  }}",
        endpoint.url, endpoint.token,
    )
}

/// Render an `mcpServers` snippet pointing at `schema mcp-shim`.
///
/// Per ADR-0030, the shim is a stdio MCP server that re-reads the global
/// `endpoint.toml` on every restart, so the consumer's `.mcp.json` no
/// longer needs to be regenerated when the daemon rotates its bearer or
/// port. The bearer never appears in the snippet.
#[must_use]
pub fn render_mcp_config_shim_fragment(binary_path: &Path) -> String {
    let escaped = escape_json_string(&binary_path.to_string_lossy());
    format!(
        "  \"schema\": {{\n    \"type\": \"stdio\",\n    \"command\": \"{escaped}\",\n    \"args\": [\"mcp-shim\"]\n  }}"
    )
}

fn escape_json_string(raw: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use std::path::PathBuf;

    use super::{
        DAEMON_LAUNCHD_LABEL, DAEMON_SYSTEMD_UNIT_NAME, DaemonInstallInputs, InstallInputs,
        launchd_label, render_linux_daemon_unit, render_linux_unit, render_macos_daemon_plist,
        render_macos_plist, systemd_unit_name,
    };

    fn fixture_inputs() -> InstallInputs {
        InstallInputs {
            binary_path: PathBuf::from("/usr/local/bin/schema"),
            config_path: PathBuf::from("/Users/op/dev/proj/schema.toml"),
            project_id: "demo-a3f9c2d1".to_string(),
            stderr_path: PathBuf::from("/Users/op/.cache/schema/projects/demo-a3f9c2d1/stderr.log"),
            nice: 5,
        }
    }

    #[test]
    fn macos_plist_substitutes_every_slot() {
        let rendered = render_macos_plist(&fixture_inputs());
        assert!(!rendered.contains('{'), "no leftover slots: {rendered}");
        assert!(rendered.contains("/usr/local/bin/schema"));
        assert!(rendered.contains("schema.toml"));
        assert!(rendered.contains("com.farchanjo.schema.demo-a3f9c2d1"));
        assert!(rendered.contains("<integer>5</integer>"));
        assert!(
            rendered.contains("<integer>10</integer>"),
            "ExitTimeOut 10s"
        );
        assert!(rendered.contains("<key>RunAtLoad</key>"));
        assert!(rendered.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn linux_unit_substitutes_every_slot() {
        let rendered = render_linux_unit(&fixture_inputs());
        assert!(!rendered.contains('{'), "no leftover slots: {rendered}");
        assert!(rendered.contains("ExecStart=/usr/local/bin/schema serve --config"));
        assert!(rendered.contains("Nice=5"));
        assert!(rendered.contains("Restart=on-failure"));
        assert!(rendered.contains("TimeoutStopSec=10s"));
        assert!(rendered.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_label_format() {
        assert_eq!(
            launchd_label("demo-a3f9c2d1"),
            "com.farchanjo.schema.demo-a3f9c2d1"
        );
    }

    #[test]
    fn systemd_unit_name_format() {
        assert_eq!(
            systemd_unit_name("demo-a3f9c2d1"),
            "schema-demo-a3f9c2d1.service"
        );
    }

    fn fixture_daemon_inputs() -> DaemonInstallInputs {
        DaemonInstallInputs {
            binary_path: PathBuf::from("/usr/local/bin/schema"),
            stderr_path: PathBuf::from("/Users/op/Library/Application Support/schema/stderr.log"),
            nice: 5,
        }
    }

    #[test]
    fn macos_daemon_plist_substitutes_every_slot() {
        let rendered = render_macos_daemon_plist(&fixture_daemon_inputs());
        assert!(!rendered.contains('{'), "no leftover slots: {rendered}");
        assert!(rendered.contains("/usr/local/bin/schema"));
        assert!(rendered.contains("<string>daemon</string>"));
        assert!(rendered.contains("com.farchanjo.schema.daemon"));
        assert!(rendered.contains("<integer>5</integer>"), "Nice");
        assert!(rendered.contains("<key>SoftResourceLimits</key>"));
        assert!(
            rendered.contains("<integer>10240</integer>"),
            "NumberOfFiles 10240 to dodge kqueue fd exhaustion across N projects"
        );
        assert!(!rendered.contains("--config"), "daemon takes no --config");
        assert!(
            !rendered.contains("project_id"),
            "daemon plist must not carry per-project slot"
        );
    }

    #[test]
    fn linux_daemon_unit_substitutes_every_slot() {
        let rendered = render_linux_daemon_unit(&fixture_daemon_inputs());
        assert!(!rendered.contains('{'), "no leftover slots: {rendered}");
        assert!(rendered.contains("ExecStart=/usr/local/bin/schema daemon"));
        assert!(!rendered.contains("--config"), "daemon takes no --config");
        assert!(rendered.contains("Nice=5"));
        assert!(rendered.contains("LimitNOFILE=10240"));
        assert!(rendered.contains("Restart=on-failure"));
        assert!(rendered.contains("TimeoutStopSec=10s"));
    }

    #[test]
    fn daemon_label_constants_match_runbook() {
        assert_eq!(DAEMON_LAUNCHD_LABEL, "com.farchanjo.schema.daemon");
        assert_eq!(DAEMON_SYSTEMD_UNIT_NAME, "schema-daemon.service");
    }
}
