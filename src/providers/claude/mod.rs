//! Claude Code: session transcripts under the Claude config directory.

pub(crate) mod history;
pub(crate) mod limits;
pub(crate) mod usage;

use crate::usage::{is_uuid_like, session_cwd_identity};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const MAX_OWNER_IDENTITY_LINE_BYTES: usize = 512 * 1024;

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

/// The project and session a transcript belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionOwner {
    pub(crate) cwd: String,
    pub(crate) session_id: Option<String>,
}

/// The owner is the cwd of the first record that names one (the launch
/// directory). Later `cd` in tool calls never re-homes a session, and the
/// lossy project-directory slug is never used. The session id comes from the
/// same record, or from a UUID file name.
pub(crate) fn resolve_session_owner(path: &Path) -> Result<Option<SessionOwner>> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let file_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| is_uuid_like(stem))
        .map(str::to_string);

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            return Ok(None);
        }
        if line.len() > MAX_OWNER_IDENTITY_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(cwd) = value
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(session_cwd_identity)
        else {
            continue;
        };
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(file_session_id);
        return Ok(Some(SessionOwner { cwd, session_id }));
    }
}
