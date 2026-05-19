//! `~/.velocity/config` — multi-context CLI configuration.
//!
//! Modelled after `kubectl`'s kubeconfig: a list of named *contexts*
//! and one `current_context` pointer. Each context carries the
//! data-plane URL and a bearer token. The token lives in the same file
//! (mode `0600` on Unix) — OIDC device flow is Phase 9 deferred per the
//! slice plan.
//!
//! ## Resolution precedence (applied by `resolve()`)
//!
//! 1. The `--context <name>` global flag.
//! 2. The `VELOCITY_CONTEXT` env var.
//! 3. `current_context` in the config file.
//!
//! A missing config file is not an error for `velocity context add` —
//! the first add creates the file and points `current_context` at it.
//! For every other subcommand, a missing or empty config produces a
//! crisp `no context configured` error.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// One named environment. Adding fields here means an old config still
/// parses — `#[serde(default)]` on new fields, never rename existing
/// ones without a migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Context {
    /// User-chosen name (`prod`, `staging`, `dev`). Unique within the
    /// config. Used as the lookup key, so we don't store it twice.
    #[serde(skip)]
    pub(crate) name: String,

    /// Data-plane base URL — e.g. `https://velocity.acme.internal`. No
    /// trailing slash; `ApiClient` appends paths.
    pub(crate) api_url: String,

    /// Bearer token. Stored as-is; the file is `0600`. Empty string
    /// means "no token configured" — calls that need auth will fail
    /// loud on the first 401 rather than at config load time.
    #[serde(default)]
    pub(crate) token: String,
}

/// The on-disk shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Config {
    /// Name of the currently selected context. Empty when no contexts
    /// exist yet.
    #[serde(default, rename = "current-context")]
    pub(crate) current_context: String,

    /// Named contexts. YAML map keyed by name so the on-disk shape
    /// reads naturally.
    #[serde(default)]
    pub(crate) contexts: std::collections::BTreeMap<String, ContextOnDisk>,
}

/// On-disk variant — `name` is the map key, not a field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ContextOnDisk {
    pub(crate) api_url: String,
    #[serde(default)]
    pub(crate) token: String,
}

impl From<ContextOnDisk> for Context {
    fn from(d: ContextOnDisk) -> Self {
        Self { name: String::new(), api_url: d.api_url, token: d.token }
    }
}

impl Config {
    /// Default path: `$VELOCITY_CONFIG`, else `~/.velocity/config`.
    /// Returns `None` only when home resolution fails AND the env var
    /// is unset — that's an environment-level error the caller should
    /// surface, not silently default to `./config`.
    pub(crate) fn default_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("VELOCITY_CONFIG") {
            return Some(PathBuf::from(p));
        }
        dirs::home_dir().map(|h| h.join(".velocity").join("config"))
    }

    /// Load from `path`. Returns an empty `Config` when the file is
    /// absent — that's the bootstrap case for `velocity context add`.
    /// All other I/O / parse errors propagate.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Ok(Self::default()),
            Ok(s) => serde_yaml::from_str(&s).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow!(e).context(format!("read {}", path.display()))),
        }
    }

    /// Atomic write + `0600` on Unix. Writes to `<path>.tmp` first, then
    /// renames so a crash mid-write can't truncate the existing config.
    /// Parent directory is created if missing.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        let yaml = serde_yaml::to_string(self).context("serialise config")?;
        // OpenOptions with explicit mode on Unix; on Windows we settle
        // for ACL inheritance (the binary still works there, but the
        // 0600 guarantee is Unix-only — documented in the security
        // section of operations.md).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("open {}", tmp.display()))?;
            f.write_all(yaml.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
            f.sync_all().ok();
        }
        #[cfg(not(unix))]
        {
            fs::write(&tmp, yaml.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Resolve the active context by name, applying the precedence
    /// rules from this module's header doc.
    pub(crate) fn resolve(&self, flag: Option<&str>) -> Result<Context> {
        let name = flag
            .map(str::to_string)
            .or_else(|| std::env::var("VELOCITY_CONTEXT").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.current_context.clone());

        if name.is_empty() {
            bail!(
                "no context configured — run `velocity context add <name> \
                 --api-url <url> --token <token>` first"
            );
        }

        let on_disk = self.contexts.get(&name).ok_or_else(|| {
            anyhow!(
                "context `{name}` not found in config (known: {})",
                self.context_names().join(", ")
            )
        })?;

        Ok(Context { name, api_url: on_disk.api_url.clone(), token: on_disk.token.clone() })
    }

    pub(crate) fn context_names(&self) -> Vec<String> {
        self.contexts.keys().cloned().collect()
    }

    /// Upsert. Returns whether this was a fresh insert (true) or an
    /// overwrite (false) so callers can report it.
    pub(crate) fn upsert(&mut self, name: &str, api_url: &str, token: &str) -> bool {
        let inserted = !self.contexts.contains_key(name);
        self.contexts.insert(
            name.to_string(),
            ContextOnDisk { api_url: api_url.to_string(), token: token.to_string() },
        );
        if self.current_context.is_empty() {
            self.current_context = name.to_string();
        }
        inserted
    }

    /// Switch `current_context`. Errors if `name` isn't in the map.
    pub(crate) fn use_context(&mut self, name: &str) -> Result<()> {
        if !self.contexts.contains_key(name) {
            bail!("context `{name}` not found (known: {})", self.context_names().join(", "));
        }
        self.current_context = name.to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn sample() -> Config {
        let mut c = Config::default();
        c.upsert("prod", "https://api.prod", "tok-prod");
        c.upsert("staging", "https://api.staging", "tok-staging");
        c.use_context("prod").unwrap();
        c
    }

    #[test]
    fn upsert_first_sets_current_context() {
        let mut c = Config::default();
        let inserted = c.upsert("dev", "https://api.dev", "t");
        assert!(inserted);
        assert_eq!(c.current_context, "dev");
    }

    #[test]
    fn upsert_existing_returns_false_and_keeps_current_context() {
        let mut c = sample();
        let inserted = c.upsert("prod", "https://api.prod2", "tok2");
        assert!(!inserted);
        assert_eq!(c.current_context, "prod");
        assert_eq!(c.contexts.get("prod").unwrap().api_url, "https://api.prod2");
    }

    #[test]
    fn use_context_unknown_errors() {
        let mut c = sample();
        let err = c.use_context("ghost").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resolve_flag_wins_over_env_and_current() {
        let c = sample();
        // Env points at one, flag at another — flag should win.
        std::env::set_var("VELOCITY_CONTEXT", "prod");
        let r = c.resolve(Some("staging")).unwrap();
        std::env::remove_var("VELOCITY_CONTEXT");
        assert_eq!(r.name, "staging");
    }

    #[test]
    fn resolve_env_overrides_current() {
        let c = sample();
        std::env::set_var("VELOCITY_CONTEXT", "staging");
        let r = c.resolve(None).unwrap();
        std::env::remove_var("VELOCITY_CONTEXT");
        assert_eq!(r.name, "staging");
    }

    #[test]
    fn resolve_falls_back_to_current_context() {
        let c = sample();
        let r = c.resolve(None).unwrap();
        assert_eq!(r.name, "prod");
        assert_eq!(r.api_url, "https://api.prod");
    }

    #[test]
    fn resolve_empty_config_errors() {
        let c = Config::default();
        let err = c.resolve(None).unwrap_err();
        assert!(err.to_string().contains("no context configured"));
    }

    #[test]
    fn resolve_unknown_name_errors() {
        let c = sample();
        let err = c.resolve(Some("ghost")).unwrap_err();
        assert!(err.to_string().contains("`ghost` not found"));
    }

    #[test]
    fn round_trip_serialises_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".velocity").join("config");
        let original = sample();
        original.save(&path).unwrap();
        let parsed = Config::load(&path).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope");
        let c = Config::load(&path).unwrap();
        assert_eq!(c, Config::default());
    }

    #[cfg(unix)]
    #[test]
    fn save_uses_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        sample().save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must be 0600, got {mode:o}");
    }

    #[test]
    fn save_is_atomic_via_tmp_rename() {
        // After save the .tmp file must be gone — proves we renamed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        sample().save(&path).unwrap();
        let tmp = path.with_extension("tmp");
        assert!(!tmp.exists(), ".tmp must not linger after save");
    }
}
