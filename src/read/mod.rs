pub(crate) mod catalog;
pub(crate) mod scan;
pub(crate) mod tui;

use crate::harness::Harness;
use crate::providers::{claude, codex};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) harness: Harness,
    pub(crate) sessions_dir: PathBuf,
}

pub(crate) fn print_sessions_dir(
    harness: Harness,
    harness_home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<()> {
    let config = build_config(harness, harness_home, sessions_dir)?;
    println!("{}", config.sessions_dir.display());
    Ok(())
}

/// `harness_home` is the harness config directory override (`--codex-home`
/// or `--claude-dir`); `sessions_dir` overrides the log directory itself.
pub(crate) fn build_config(
    harness: Harness,
    harness_home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<Config> {
    let sessions_dir = match sessions_dir {
        Some(path) => validate_dir(&path, "--sessions-dir")?,
        None => match harness {
            Harness::Codex => codex::sessions_root(
                &codex::resolve_codex_home(harness_home)
                    .context("Unable to resolve CODEX_HOME (default: ~/.codex)")?,
            ),
            Harness::Claude => claude::projects_root(
                &claude::resolve_claude_dir(harness_home)
                    .context("Unable to resolve the Claude Code config directory")?,
            ),
        },
    };
    Ok(Config {
        harness,
        sessions_dir,
    })
}

pub(crate) fn build_browser(config: &Config) -> Result<tui::BrowserState> {
    let catalog = scan::build_catalog(config.harness, &config.sessions_dir)?;
    Ok(tui::BrowserState::new(catalog))
}

fn validate_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}
