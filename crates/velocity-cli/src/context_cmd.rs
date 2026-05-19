//! `velocity context` — manage `~/.velocity/config` entries.
//!
//! Sub-commands:
//! - `list`   — table of all contexts, marks the current one.
//! - `show`   — print the active context's resolved values (token redacted).
//! - `use`    — switch `current-context`.
//! - `add`    — upsert; first add seeds `current-context`.
//! - `delete` — remove a context; resets `current-context` if it was active.

use anyhow::{Context as _, Result};
use clap::Subcommand;

use crate::config::Config;
use crate::output::{print, OutputFormat};

#[derive(Debug, Subcommand)]
pub(crate) enum ContextCmd {
    /// List all contexts; the active one is marked with `*`.
    List,
    /// Print the active context (token redacted).
    Show,
    /// Switch the active context.
    Use {
        /// Context name.
        name: String,
    },
    /// Add or overwrite a context. First add seeds `current-context`.
    Add {
        /// Context name (e.g. `prod`).
        name: String,
        /// Data-plane API base URL.
        #[arg(long)]
        api_url: String,
        /// Bearer token. Stored in `~/.velocity/config` (mode 0600 on
        /// Unix). Read interactively from a future `--token-stdin`
        /// once we ship OIDC; for now passed verbatim.
        #[arg(long, default_value = "")]
        token: String,
    },
    /// Remove a context.
    Delete { name: String },
}

pub(crate) async fn run(
    cmd: ContextCmd,
    config_path_override: Option<&std::path::Path>,
    output: OutputFormat,
) -> Result<()> {
    let path = match config_path_override {
        Some(p) => p.to_path_buf(),
        None => Config::default_path()
            .context("could not resolve config path (set $VELOCITY_CONFIG or $HOME)")?,
    };
    let mut cfg = Config::load(&path)?;
    match cmd {
        ContextCmd::List => {
            let rows: Vec<Vec<String>> = cfg
                .contexts
                .iter()
                .map(|(name, c)| {
                    let active = if *name == cfg.current_context { "*" } else { "" };
                    vec![active.to_string(), name.clone(), c.api_url.clone()]
                })
                .collect();
            print(&["", "NAME", "API URL"], &rows, output);
            Ok(())
        }
        ContextCmd::Show => {
            let active = cfg.resolve(None)?;
            let token_state = if active.token.is_empty() { "(unset)" } else { "(redacted)" };
            let rows = vec![vec![active.name, active.api_url, token_state.to_string()]];
            print(&["NAME", "API URL", "TOKEN"], &rows, output);
            Ok(())
        }
        ContextCmd::Use { name } => {
            cfg.use_context(&name)?;
            cfg.save(&path)?;
            eprintln!("switched to `{name}`");
            Ok(())
        }
        ContextCmd::Add { name, api_url, token } => {
            let inserted = cfg.upsert(&name, &api_url, &token);
            cfg.save(&path)?;
            eprintln!("{} `{}` -> {}", if inserted { "added" } else { "updated" }, name, api_url);
            Ok(())
        }
        ContextCmd::Delete { name } => {
            if cfg.contexts.remove(&name).is_none() {
                anyhow::bail!("context `{name}` not found");
            }
            if cfg.current_context == name {
                cfg.current_context = cfg.contexts.keys().next().cloned().unwrap_or_default();
            }
            cfg.save(&path)?;
            eprintln!("deleted `{name}`");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tempfile::tempdir;

    fn cfg_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("config")
    }

    #[tokio::test]
    async fn add_creates_file_and_sets_current() {
        let dir = tempdir().unwrap();
        let path = cfg_path(&dir);
        run(
            ContextCmd::Add {
                name: "prod".into(),
                api_url: "https://api.prod".into(),
                token: "tok".into(),
            },
            Some(&path),
            OutputFormat::Table,
        )
        .await
        .unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.current_context, "prod");
        assert_eq!(cfg.contexts.get("prod").unwrap().api_url, "https://api.prod");
    }

    #[tokio::test]
    async fn use_unknown_errors() {
        let dir = tempdir().unwrap();
        let path = cfg_path(&dir);
        // Seed one context first.
        run(
            ContextCmd::Add {
                name: "prod".into(),
                api_url: "https://api".into(),
                token: "".into(),
            },
            Some(&path),
            OutputFormat::Table,
        )
        .await
        .unwrap();

        let err = run(ContextCmd::Use { name: "ghost".into() }, Some(&path), OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`ghost` not found"));
    }

    #[tokio::test]
    async fn delete_active_clears_current_context() {
        let dir = tempdir().unwrap();
        let path = cfg_path(&dir);
        run(
            ContextCmd::Add { name: "prod".into(), api_url: "u".into(), token: "".into() },
            Some(&path),
            OutputFormat::Table,
        )
        .await
        .unwrap();
        run(ContextCmd::Delete { name: "prod".into() }, Some(&path), OutputFormat::Table)
            .await
            .unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.current_context, "");
    }

    #[tokio::test]
    async fn delete_inactive_keeps_current_context() {
        let dir = tempdir().unwrap();
        let path = cfg_path(&dir);
        run(
            ContextCmd::Add { name: "prod".into(), api_url: "u".into(), token: "".into() },
            Some(&path),
            OutputFormat::Table,
        )
        .await
        .unwrap();
        run(
            ContextCmd::Add { name: "staging".into(), api_url: "u2".into(), token: "".into() },
            Some(&path),
            OutputFormat::Table,
        )
        .await
        .unwrap();
        run(ContextCmd::Delete { name: "staging".into() }, Some(&path), OutputFormat::Table)
            .await
            .unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.current_context, "prod");
    }

    #[tokio::test]
    async fn delete_missing_errors() {
        let dir = tempdir().unwrap();
        let path = cfg_path(&dir);
        let err = run(ContextCmd::Delete { name: "nope".into() }, Some(&path), OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
