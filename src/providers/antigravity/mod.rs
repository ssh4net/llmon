//! Antigravity CLI (`agy`): conversations under `~/.gemini/antigravity-cli`.

pub(crate) mod history;
pub(crate) mod usage;

use std::path::{Path, PathBuf};

/// Resolves the Antigravity CLI directory: explicit override, then
/// `~/.gemini/antigravity-cli`.
pub(crate) fn resolve_antigravity_dir(override_dir: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_dir {
        return Some(path);
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(
                    PathBuf::from(trimmed)
                        .join(".gemini")
                        .join("antigravity-cli"),
                );
            }
        }
    }
    None
}

/// Directory with the per-conversation databases that usage and history scan.
pub(crate) fn sessions_root(antigravity_dir: &Path) -> PathBuf {
    antigravity_dir.join("conversations")
}
