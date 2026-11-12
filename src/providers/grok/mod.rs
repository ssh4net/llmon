//! Grok CLI: session logs under the Grok home directory.

pub(crate) mod history;
pub(crate) mod usage;

use std::path::{Path, PathBuf};

/// Resolves the Grok home directory: explicit override, then `GROK_HOME`,
/// then `~/.grok`.
pub(crate) fn resolve_grok_dir(override_dir: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_dir {
        return Some(path);
    }
    if let Ok(value) = std::env::var("GROK_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed).join(".grok"));
            }
        }
    }
    None
}

/// Directory with the sessions (`<url-encoded cwd>/<session id>/`) that usage
/// and history scan.
pub(crate) fn sessions_root(grok_dir: &Path) -> PathBuf {
    grok_dir.join("sessions")
}
