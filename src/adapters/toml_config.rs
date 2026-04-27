//! `schema.toml` definition and loader.
//!
//! The on-disk format is the single contract project consumers depend on. Adding
//! or removing fields is a breaking change for them; do it by bumping
//! `[project] version` and shipping a migration note in CLAUDE.md.

use std::env;
use std::fmt::Display;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::domain::CorpusKind;

/// Top-level structure of `schema.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaConfig {
    pub project: ProjectMeta,

    #[serde(default)]
    pub corpus: Vec<Corpus>,

    #[serde(default)]
    pub embedding: EmbeddingConfig,

    #[serde(default)]
    pub retrieval: RetrievalConfig,

    #[serde(default)]
    pub security: SecurityConfig,

    #[serde(default)]
    pub llm: LlmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMeta {
    /// Human-readable project name. Combined with the project's canonical path
    /// hash to form `ProjectId`. Must be unique within the user's machine.
    pub name: String,

    /// Schema version of the `schema.toml` file itself. Used for migration
    /// detection if the format ever evolves.
    #[serde(default = "default_config_version")]
    pub version: String,
}

fn default_config_version() -> String {
    "1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corpus {
    /// Path relative to the project root (where `schema.toml` lives).
    /// Path traversal (`..`) is refused at validation time.
    pub path: PathBuf,

    /// Type of content; drives chunking strategy.
    pub kind: CorpusKind,

    /// Optional glob patterns to exclude from this corpus entry.
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Embedding model identifier. FASE 1.0 only supports `"bge-m3"`.
    #[serde(default = "default_embedding_model")]
    pub model: String,

    /// Process scheduler nice value applied by the service unit (ADR-0018).
    /// Range 0..=19; default 5. Higher = lower priority. The launchd plist
    /// (`Nice` integer) and the systemd unit (`Nice=`) substitute this value
    /// at install time so embed bursts do not preempt the operator's
    /// foreground processes.
    #[serde(default = "default_embedding_nice")]
    pub nice: u8,
}

fn default_embedding_model() -> String {
    "bge-m3".to_string()
}

const fn default_embedding_nice() -> u8 {
    5
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: default_embedding_model(),
            nice: default_embedding_nice(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalConfig {
    /// Default top-K for retrieval queries when the caller does not specify.
    #[serde(default = "default_top_k")]
    pub top_k_default: usize,

    /// Maximum chunk size in bytes. Chunks above this are split by chunker.
    #[serde(default = "default_chunk_size_max")]
    pub chunk_size_max: usize,

    /// Maximum file size in bytes. Files above this are skipped with a warning.
    #[serde(default = "default_file_size_max")]
    pub file_size_max: u64,
}

const fn default_top_k() -> usize {
    8
}
const fn default_chunk_size_max() -> usize {
    8_192
}
const fn default_file_size_max() -> u64 {
    5_242_880 // 5 MiB
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            top_k_default: default_top_k(),
            chunk_size_max: default_chunk_size_max(),
            file_size_max: default_file_size_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// If `true`, follow symlinks during corpus walking. Default: `false`
    /// (safer; symlinks can escape the project root).
    #[serde(default)]
    pub follow_symlinks: bool,

    /// Globs always excluded from any corpus, regardless of per-entry overrides.
    #[serde(default = "default_exclude_default")]
    pub exclude_default: Vec<String>,
}

fn default_exclude_default() -> Vec<String> {
    vec![
        ".git".into(),
        "node_modules".into(),
        "target".into(),
        "dist".into(),
        "build".into(),
        ".venv".into(),
        "__pycache__".into(),
    ]
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            exclude_default: default_exclude_default(),
        }
    }
}

/// `[llm]` knob block (ADR-0025).
///
/// Drives provider selection for the `synthesize` MCP tool. When
/// `provider == "none"` (or unset and no `*_API_KEY` is in the
/// environment), `synthesize` runs in disabled mode per ADR-0025
/// §"silent-degrade evidence".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Provider selector. `"anthropic"`, `"openai"`, `"none"`, or
    /// `"auto"` (auto-detect from `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`).
    /// Default: `"auto"`.
    #[serde(default = "default_llm_provider")]
    pub provider: String,

    /// Model id passed to the provider. Empty string means
    /// "use the per-provider compiled-in default at composition root"
    /// — Anthropic → `claude-haiku-4-5-20251001`, `OpenAI` → `gpt-5`.
    #[serde(default)]
    pub model: String,

    /// Optional override for output-token cap. When `None`, the
    /// outbound adapter substitutes its provider-specific default
    /// (Anthropic 1024, `OpenAI` 8192) per ADR-0025 amendment of
    /// 2026-04-27. Operators only set this to override.
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Optional override for sampling temperature. When `None`, the
    /// outbound adapter substitutes its provider-specific default
    /// (Anthropic 0.0, `OpenAI` 1.0) per ADR-0025 amendment of
    /// 2026-04-27.
    #[serde(default)]
    pub temperature: Option<f32>,
}

fn default_llm_provider() -> String {
    "auto".to_string()
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: default_llm_provider(),
            model: String::new(),
            max_tokens: None,
            temperature: None,
        }
    }
}

impl SchemaConfig {
    /// Read `schema.toml` from a path and validate it.
    ///
    /// Direct file load with no ENV overlay. Used by tests and by the
    /// daemon mode where the absolute config path comes from the
    /// service unit at install-time (ADR-0019/0020). For interactive
    /// CLI verbs, use [`Self::resolve`] instead.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read, parsed as TOML, or fails
    /// semantic validation (empty name, path traversal, unsupported model).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading schema.toml at {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw).with_context(|| "parsing schema.toml as TOML")?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// Resolve `schema.toml` per ADR-0023: pick the file via the
    /// CLI flag → `SCHEMA_CONFIG` env → walk-up cascade, parse it,
    /// then layer ENV overrides on top.
    ///
    /// Emits one `tracing` `info` event per knob announcing its
    /// resolved source (file / env / default).
    ///
    /// # Errors
    /// Returns an error if no `schema.toml` is found, the file fails to
    /// parse, an ENV var has an invalid value, or post-overlay
    /// validation fails.
    pub fn resolve(cli_arg: Option<&Path>) -> Result<(Self, PathBuf)> {
        let (path, resolution) = resolve_config_path(cli_arg)?;
        info!(
            path = %path.display(),
            resolution = %resolution,
            "config: source path",
        );
        let mut cfg = Self::load(&path)?;
        cfg.apply_env_overrides()?;
        cfg.validate(&path)?;
        cfg.log_knob_sources();
        Ok((cfg, path))
    }

    /// Apply `SCHEMA_*` env overlay onto `self`, reading values from the
    /// process environment.
    fn apply_env_overrides(&mut self) -> Result<()> {
        self.apply_overrides_from(&|key| env_str_via_process(key))
    }

    /// Apply `SCHEMA_*` env overlay onto `self`, reading values from
    /// `lookup`. Test-friendly hook: pass a closure backed by a
    /// `HashMap` to avoid mutating process-global env.
    fn apply_overrides_from(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        self.apply_embedding_overrides(lookup)?;
        self.apply_retrieval_overrides(lookup)?;
        self.apply_security_overrides(lookup)?;
        self.apply_llm_overrides(lookup)?;
        Ok(())
    }

    fn apply_embedding_overrides(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        if let Some(value) = lookup("SCHEMA_EMBEDDING_MODEL") {
            self.embedding.model = value;
        }
        if let Some(value) = parse_lookup::<u8>(lookup, "SCHEMA_EMBEDDING_NICE")? {
            self.embedding.nice = value;
        }
        Ok(())
    }

    fn apply_retrieval_overrides(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        if let Some(value) = parse_lookup::<usize>(lookup, "SCHEMA_RETRIEVAL_TOP_K_DEFAULT")? {
            self.retrieval.top_k_default = value;
        }
        if let Some(value) = parse_lookup::<usize>(lookup, "SCHEMA_RETRIEVAL_CHUNK_SIZE_MAX")? {
            self.retrieval.chunk_size_max = value;
        }
        if let Some(value) = parse_lookup::<u64>(lookup, "SCHEMA_RETRIEVAL_FILE_SIZE_MAX")? {
            self.retrieval.file_size_max = value;
        }
        Ok(())
    }

    fn apply_security_overrides(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        if let Some(value) = parse_lookup::<bool>(lookup, "SCHEMA_SECURITY_FOLLOW_SYMLINKS")? {
            self.security.follow_symlinks = value;
        }
        Ok(())
    }

    fn apply_llm_overrides(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        if let Some(value) = lookup("SCHEMA_LLM_PROVIDER") {
            self.llm.provider = value;
        }
        if let Some(value) = lookup("SCHEMA_LLM_MODEL") {
            self.llm.model = value;
        }
        if let Some(value) = parse_lookup::<u32>(lookup, "SCHEMA_LLM_MAX_TOKENS")? {
            self.llm.max_tokens = Some(value);
        }
        if let Some(value) = parse_lookup::<f32>(lookup, "SCHEMA_LLM_TEMPERATURE")? {
            self.llm.temperature = Some(value);
        }
        Ok(())
    }

    /// Emit one `tracing` `info` event per ENV-relevant knob, listing
    /// the resolved value and whether it came from env, file, or
    /// default. Sources help operators debug "why is this value
    /// not what schema.toml says?" without grepping logs.
    #[expect(
        clippy::too_many_lines,
        reason = "one log line per knob keeps the source-of-truth log self-contained; splitting per-section would scatter it across helpers and reduce diagnostic locality"
    )]
    fn log_knob_sources(&self) {
        log_knob(
            "[embedding].model",
            &self.embedding.model,
            "SCHEMA_EMBEDDING_MODEL",
        );
        log_knob(
            "[embedding].nice",
            &self.embedding.nice,
            "SCHEMA_EMBEDDING_NICE",
        );
        log_knob(
            "[retrieval].top_k_default",
            &self.retrieval.top_k_default,
            "SCHEMA_RETRIEVAL_TOP_K_DEFAULT",
        );
        log_knob(
            "[retrieval].chunk_size_max",
            &self.retrieval.chunk_size_max,
            "SCHEMA_RETRIEVAL_CHUNK_SIZE_MAX",
        );
        log_knob(
            "[retrieval].file_size_max",
            &self.retrieval.file_size_max,
            "SCHEMA_RETRIEVAL_FILE_SIZE_MAX",
        );
        log_knob(
            "[security].follow_symlinks",
            &self.security.follow_symlinks,
            "SCHEMA_SECURITY_FOLLOW_SYMLINKS",
        );
        log_knob("[llm].provider", &self.llm.provider, "SCHEMA_LLM_PROVIDER");
        log_knob("[llm].model", &self.llm.model, "SCHEMA_LLM_MODEL");
        log_knob(
            "[llm].max_tokens",
            &self
                .llm
                .max_tokens
                .map_or_else(|| "auto".to_string(), |v| v.to_string()),
            "SCHEMA_LLM_MAX_TOKENS",
        );
        log_knob(
            "[llm].temperature",
            &self
                .llm
                .temperature
                .map_or_else(|| "auto".to_string(), |v| v.to_string()),
            "SCHEMA_LLM_TEMPERATURE",
        );
    }

    /// Resolve the project root directory — the directory containing `schema.toml`.
    ///
    /// # Errors
    /// Returns an error if `config_path` cannot be canonicalised or has no parent.
    pub fn project_root(config_path: &Path) -> Result<PathBuf> {
        let canonical = config_path
            .canonicalize()
            .with_context(|| format!("canonicalising {}", config_path.display()))?;
        let parent = canonical
            .parent()
            .ok_or_else(|| anyhow::anyhow!("schema.toml has no parent directory"))?;
        Ok(parent.to_path_buf())
    }

    /// Validate the loaded config (path traversal, unknown kinds already caught
    /// by serde, name non-empty, etc.).
    fn validate(&self, config_path: &Path) -> Result<()> {
        if self.project.name.is_empty() {
            bail!("[project] name must not be empty");
        }
        let root = Self::project_root(config_path)?;
        for (idx, corpus) in self.corpus.iter().enumerate() {
            validate_corpus_path(idx, corpus, &root)?;
        }
        self.validate_embedding()?;
        self.validate_llm()?;
        Ok(())
    }

    fn validate_embedding(&self) -> Result<()> {
        if self.embedding.model != "bge-m3" {
            bail!(
                "[embedding] model = {:?} is not supported in FASE 1.0 (only \"bge-m3\")",
                self.embedding.model
            );
        }
        if self.embedding.nice > 19 {
            bail!(
                "[embedding] nice = {} is out of range (must be 0..=19 per ADR-0018)",
                self.embedding.nice
            );
        }
        Ok(())
    }

    fn validate_llm(&self) -> Result<()> {
        match self.llm.provider.as_str() {
            "anthropic" | "openai" | "none" | "auto" => {}
            other => bail!(
                "[llm] provider = {other:?} is not supported (must be one of \
                 \"anthropic\", \"openai\", \"none\", \"auto\")"
            ),
        }
        if let Some(t) = self.llm.temperature
            && !(0.0..=2.0).contains(&t)
        {
            bail!(
                "[llm] temperature = {t} is out of range (must be 0.0..=2.0); \
                 omit the knob to use the per-provider default"
            );
        }
        Ok(())
    }
}

/// Read an env var as `String`, treating empty as absent. Uses the process
/// environment.
fn env_str(key: &str) -> Option<String> {
    env_str_via_process(key)
}

/// Same as [`env_str`] but explicit about the source — useful for splitting
/// out the testable seam.
fn env_str_via_process(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Read a value via the supplied `lookup` and parse it. Returns `Ok(None)`
/// when unset, `Ok(Some(_))` on success, or a descriptive error naming the
/// env var on parse failure.
fn parse_lookup<T: FromStr>(lookup: &dyn Fn(&str) -> Option<String>, key: &str) -> Result<Option<T>>
where
    <T as FromStr>::Err: Display,
{
    let Some(raw) = lookup(key) else {
        return Ok(None);
    };
    raw.parse::<T>()
        .map(Some)
        .map_err(|e| anyhow::anyhow!("invalid value for {key}: {raw:?} (parse error: {e})"))
}

/// Resolve `schema.toml` path per ADR-0023.
fn resolve_config_path(cli_arg: Option<&Path>) -> Result<(PathBuf, &'static str)> {
    if let Some(arg) = cli_arg
        && !arg.as_os_str().is_empty()
    {
        if cli_arg_is_default_marker(arg) {
            // The CLI default literal `schema.toml` is treated as "not
            // explicitly set" so walk-up takes over. Operators who want
            // exactly the CWD `schema.toml` can pass `--config ./schema.toml`.
        } else {
            return Ok((arg.to_path_buf(), "cli-flag"));
        }
    }
    if let Some(value) = env_str("SCHEMA_CONFIG") {
        return Ok((PathBuf::from(value), "env(SCHEMA_CONFIG)"));
    }
    if let Some(path) = walk_up_for_schema_toml()? {
        return Ok((path, "walk-up"));
    }
    if let Some(path) = home_dotfile_fallback() {
        return Ok((path, "home-dotfile"));
    }
    bail!(
        "schema.toml not found in CWD or any parent directory, and no \
         ~/.schema.toml fallback present. Pass --config <path> or set \
         SCHEMA_CONFIG=<path>."
    )
}

/// The clap `default_value("schema.toml")` we keep on every subcommand
/// produces this exact `PathBuf` when the operator passes nothing. The
/// resolver treats it as "fall through to walk-up" so that CLI verbs
/// run from a project subdirectory still find the root `schema.toml`.
fn cli_arg_is_default_marker(arg: &Path) -> bool {
    arg == Path::new("schema.toml")
}

/// Walk-up CWD ancestors until `schema.toml` is found. Stops only at the
/// FS root (no `$HOME` boundary, per ADR-0023 §"Walk-up boundary").
/// Returns `Ok(None)` when no `schema.toml` is reachable from CWD —
/// callers chain to the HOME-dotfile fallback before erroring.
fn walk_up_for_schema_toml() -> Result<Option<PathBuf>> {
    let cwd = env::current_dir().context("reading current working directory")?;
    Ok(walk_up_from(&cwd))
}

/// Pure walk-up helper, parameterised on the start directory. Extracted so
/// tests can exercise the algorithm against a `TempDir` without mutating
/// the process-global CWD (which is racy under concurrent test runs).
fn walk_up_from(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        let candidate = current.join("schema.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    None
}

/// HOME-dotfile fallback (`~/.schema.toml`). Triggered only when walk-up
/// returns nothing. Rationale: CLI verbs run from outside any project
/// tree (e.g., `~/scratch/`) still resolve to a sane config without
/// forcing `--config`. ADR-0023 amendment, 2026-04-26.
///
/// `dirs::home_dir()` resolves the platform-correct HOME root
/// (`$HOME` on Unix, `%USERPROFILE%` on Windows); this avoids the
/// `env::var("HOME")` Windows-empty pitfall.
fn home_dotfile_fallback() -> Option<PathBuf> {
    home_dotfile_fallback_in(dirs::home_dir().as_deref())
}

/// Pure helper, parameterised on the HOME root. Extracted so tests can
/// exercise the algorithm against a `TempDir` without mutating the
/// process-global HOME env (which is racy under concurrent test runs).
fn home_dotfile_fallback_in(home: Option<&Path>) -> Option<PathBuf> {
    let candidate = home?.join(".schema.toml");
    candidate.is_file().then_some(candidate)
}

/// Emit `INFO config: knob <name> = <value> source=<env|file>`.
fn log_knob<T: Display>(name: &str, value: &T, env_key: &str) {
    let source = if env_str(env_key).is_some() {
        format!("env({env_key})")
    } else {
        "file".to_string()
    };
    info!(knob = %name, value = %value, source = %source, "config: knob");
}

/// Reject absolute paths, parent-dir traversal, and resolved paths outside `root`.
fn validate_corpus_path(idx: usize, corpus: &Corpus, root: &Path) -> Result<()> {
    if corpus.path.is_absolute() {
        bail!(
            "corpus[{idx}].path must be relative to project root (got absolute: {})",
            corpus.path.display()
        );
    }
    if corpus
        .path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        bail!(
            "corpus[{idx}].path must not contain '..' (got {})",
            corpus.path.display()
        );
    }
    let resolved = root.join(&corpus.path);
    if let Ok(canon) = resolved.canonicalize()
        && !canon.starts_with(root)
    {
        bail!(
            "corpus[{idx}].path escapes project root (resolves to {})",
            canon.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let toml_str = r#"
[project]
name = "demo"
version = "1"

[[corpus]]
path = "docs/decisions"
kind = "adr-madr"
"#;
        let cfg: SchemaConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.project.name, "demo");
        assert_eq!(cfg.corpus.len(), 1);
        assert_eq!(cfg.corpus[0].kind, CorpusKind::AdrMadr);
        assert_eq!(cfg.embedding.model, "bge-m3");
        assert_eq!(cfg.retrieval.top_k_default, 8);
    }

    #[test]
    fn applies_defaults() {
        let toml_str = r#"
[project]
name = "x"
"#;
        let cfg: SchemaConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.project.version, "1");
        assert_eq!(cfg.embedding.model, "bge-m3");
        assert_eq!(cfg.embedding.nice, 5, "ADR-0018 default nice");
        assert!(!cfg.security.follow_symlinks);
        assert!(cfg.security.exclude_default.contains(&".git".to_string()));
    }

    /// ADR-0018 — `[embedding] nice` is honoured when set.
    #[test]
    fn parses_embedding_nice_override() {
        let toml_str = r#"
[project]
name = "demo"

[embedding]
nice = 10
"#;
        let cfg: SchemaConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.embedding.nice, 10);
    }

    /// ADR-0018 — `validate()` rejects nice values above 19.
    #[test]
    fn rejects_nice_out_of_range() {
        use std::fs as stdfs;
        use std::io::Write;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("schema.toml");
        let mut f = stdfs::File::create(&path).unwrap();
        write!(f, "[project]\nname = \"demo\"\n[embedding]\nnice = 25\n").unwrap();
        drop(f);
        let err = SchemaConfig::load(&path).unwrap_err();
        assert!(
            err.to_string().contains("nice = 25"),
            "expected nice-out-of-range error, got: {err}"
        );
    }

    /// ADR-0023 fitness — walk-up from a deep subdirectory finds the
    /// `schema.toml` at the project root.
    #[test]
    fn walk_up_finds_schema_toml_in_ancestor() {
        use std::fs as stdfs;
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let deep = root.join("docs").join("decisions").join("sub");
        stdfs::create_dir_all(&deep).unwrap();
        let mut f = stdfs::File::create(root.join("schema.toml")).unwrap();
        writeln!(f, "[project]\nname = \"demo\"\n").unwrap();
        drop(f);

        let resolved = walk_up_from(&deep).unwrap();
        assert_eq!(resolved, root.join("schema.toml"));
    }

    /// ADR-0023 fitness — walk-up returns `None` when no `schema.toml`
    /// exists in the chain. The descriptive error mentioning `--config`
    /// and `SCHEMA_CONFIG` is composed by `resolve_config_path` after the
    /// HOME-dotfile fallback also fails (covered by
    /// `home_dotfile_fallback_returns_none_when_absent`).
    #[test]
    fn walk_up_returns_none_when_no_schema_toml() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        assert!(walk_up_from(tmp.path()).is_none());
    }

    /// ADR-0023 fitness (HOME-dotfile fallback) — when `~/.schema.toml`
    /// exists in the simulated HOME, the helper returns it.
    #[test]
    fn home_dotfile_fallback_returns_path_when_present() {
        use std::fs as stdfs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        stdfs::write(tmp.path().join(".schema.toml"), b"[project]\nname=\"x\"\n").unwrap();
        assert_eq!(
            home_dotfile_fallback_in(Some(tmp.path())),
            Some(tmp.path().join(".schema.toml"))
        );
    }

    /// ADR-0023 fitness (HOME-dotfile fallback) — absent file returns
    /// `None`; chain proceeds to error.
    #[test]
    fn home_dotfile_fallback_returns_none_when_absent() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        assert_eq!(home_dotfile_fallback_in(Some(tmp.path())), None);
    }

    /// ADR-0023 fitness — `dirs::home_dir()` returning `None` (rare:
    /// stripped env on some sandboxes) propagates as `None`, not panic.
    #[test]
    fn home_dotfile_fallback_handles_no_home() {
        assert_eq!(home_dotfile_fallback_in(None), None);
    }

    /// ADR-0023 fitness — `cli_arg_is_default_marker` recognises clap's
    /// `default_value("schema.toml")` so the CLI default does not block
    /// walk-up. An explicit `--config ./schema.toml` does block walk-up.
    #[test]
    fn cli_default_marker_recognition() {
        assert!(cli_arg_is_default_marker(Path::new("schema.toml")));
        assert!(!cli_arg_is_default_marker(Path::new("./schema.toml")));
        assert!(!cli_arg_is_default_marker(Path::new("/abs/schema.toml")));
        assert!(!cli_arg_is_default_marker(Path::new("foo/schema.toml")));
    }

    /// ADR-0023 fitness — explicit `--config` returns the `cli-flag`
    /// resolution label without consulting walk-up or the HOME-dotfile
    /// fallback.
    #[test]
    fn resolve_config_path_labels_cli_flag() {
        let (path, label) = resolve_config_path(Some(Path::new("/explicit/schema.toml"))).unwrap();
        assert_eq!(path, Path::new("/explicit/schema.toml"));
        assert_eq!(label, "cli-flag");
    }

    use std::collections::HashMap;

    type EnvLookup = Box<dyn Fn(&str) -> Option<String>>;

    /// Build a fixed-map env lookup for tests, avoiding process-global
    /// `env::set_var` (which is `unsafe` in Rust 2024 and racy across
    /// concurrent test runs).
    fn map_lookup(pairs: &[(&'static str, &'static str)]) -> EnvLookup {
        let owned: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Box::new(move |key| owned.get(key).cloned())
    }

    fn empty_cfg() -> SchemaConfig {
        SchemaConfig {
            project: ProjectMeta {
                name: "demo".into(),
                version: "1".into(),
            },
            corpus: Vec::new(),
            embedding: EmbeddingConfig::default(),
            retrieval: RetrievalConfig::default(),
            security: SecurityConfig::default(),
            llm: LlmConfig::default(),
        }
    }

    /// ADR-0023 fitness — `apply_overrides_from` reads values via the
    /// injected lookup and overlays them onto the loaded config.
    #[test]
    fn env_overlay_overrides_file_value() {
        let lookup = map_lookup(&[("SCHEMA_RETRIEVAL_FILE_SIZE_MAX", "12345")]);
        let mut cfg = empty_cfg();
        cfg.apply_overrides_from(&*lookup).unwrap();
        assert_eq!(cfg.retrieval.file_size_max, 12_345);
    }

    /// ADR-0023 fitness — multiple knobs overridden in one pass.
    #[test]
    fn env_overlay_overrides_multiple_knobs() {
        let lookup = map_lookup(&[
            ("SCHEMA_EMBEDDING_NICE", "12"),
            ("SCHEMA_RETRIEVAL_TOP_K_DEFAULT", "32"),
            ("SCHEMA_SECURITY_FOLLOW_SYMLINKS", "true"),
        ]);
        let mut cfg = empty_cfg();
        cfg.apply_overrides_from(&*lookup).unwrap();
        assert_eq!(cfg.embedding.nice, 12);
        assert_eq!(cfg.retrieval.top_k_default, 32);
        assert!(cfg.security.follow_symlinks);
    }

    /// ADR-0023 fitness — parse failures surface a descriptive error
    /// naming the env var (no silent fallback to file value).
    #[test]
    fn env_overlay_invalid_value_returns_descriptive_error() {
        let lookup = map_lookup(&[("SCHEMA_EMBEDDING_NICE", "not-a-number")]);
        let mut cfg = empty_cfg();
        let err = cfg.apply_overrides_from(&*lookup).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SCHEMA_EMBEDDING_NICE"),
            "error must name the env var, got: {msg}"
        );
        assert!(msg.contains("not-a-number"), "got: {msg}");
    }

    /// ADR-0023 fitness — `env_str_via_process` filters empty strings to
    /// `None` so an accidentally-blank `SCHEMA_*=` does not silently wipe
    /// the file value.
    #[test]
    fn env_str_via_process_treats_empty_as_absent() {
        let key = "SCHEMA_TEST_NEVER_SET_ANYWHERE";
        // Pre-condition: we have not set this var; reader returns None.
        assert_eq!(env_str_via_process(key), None);
    }
}
