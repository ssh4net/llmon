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

/// The session catalog of every harness's log directory, merged by project.
pub(crate) fn build_catalogs(sources: &[(Harness, PathBuf)]) -> Result<scan::Catalog> {
    let catalogs = sources
        .iter()
        .map(|(harness, dir)| scan::build_catalog(*harness, dir))
        .collect::<Result<Vec<_>>>()?;
    Ok(scan::merge_catalogs(catalogs))
}

pub(crate) fn build_browser(sources: &[(Harness, PathBuf)]) -> Result<tui::BrowserState> {
    Ok(tui::BrowserState::new(build_catalogs(sources)?))
}

fn validate_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}
