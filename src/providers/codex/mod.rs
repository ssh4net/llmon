//! Codex CLI: session logs under `CODEX_HOME`.

pub(crate) mod history;
pub(crate) mod rpc;
pub(crate) mod usage;

use std::path::{Path, PathBuf};

pub(crate) fn resolve_codex_home(override_home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_home {
        return Some(path);
    }
    if let Ok(value) = std::env::var("CODEX_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed).join(".codex"));
            }
        }
    }
    None
}

/// Directory with the Codex session logs that usage and history scan.
pub(crate) fn sessions_root(codex_home: &Path) -> PathBuf {
    codex_home.join("sessions")
}
