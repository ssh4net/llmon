//! Claude Code: session transcripts under the Claude config directory.

pub(crate) mod usage;

use std::path::{Path, PathBuf};

/// Resolves the Claude Code config directory: explicit override, then
/// `CLAUDE_CONFIG_DIR`, then `~/.claude`.
pub(crate) fn resolve_claude_dir(override_dir: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_dir {
        return Some(path);
    }
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed).join(".claude"));
            }
        }
    }
    None
}

/// Directory with the session transcripts (`<project-slug>/<session>.jsonl`
/// plus `<session>/subagents/*.jsonl`) that usage and history scan.
pub(crate) fn projects_root(claude_dir: &Path) -> PathBuf {
    claude_dir.join("projects")
}
