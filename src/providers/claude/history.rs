//! Claude Code session history: list metadata (title, start time, model,
//! client version) and the turn/tool/token detail of one transcript.
//! Subagent transcripts are attached to their parent session.

use super::{resolve_session_owner, SessionOwner};
use crate::harness::Harness;
use crate::read::scan::{
    session_start_fields, truncate_single_line, SessionDetail, SessionSummary, MAX_TITLE_CHARS,
    MAX_TURN_PREVIEW_CHARS, UNRESOLVED_SESSION_OWNER,
};
use crate::usage::TokenBreakdown;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Lines longer than this are skipped by the summary scan (large tool results).
const MAX_SUMMARY_LINE_BYTES: usize = 512 * 1024;
/// Header lines parsed in full; later lines only when they carry a title.
const HEADER_LINE_LIMIT: usize = 128;
const SUBAGENTS_DIR: &str = "subagents";

/// Splits transcripts into top-level sessions and subagent transcripts
/// (`<project>/<session-id>/subagents/*.jsonl`), keyed by parent session id.
pub(crate) fn split_subagent_files(
    candidates: Vec<PathBuf>,
) -> (Vec<PathBuf>, BTreeMap<String, Vec<PathBuf>>) {
    let mut subagents: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut top_level = Vec::new();
    for path in candidates {
        match subagent_parent_session_id(&path) {
            Some(parent) => subagents.entry(parent).or_default().push(path),
            None => top_level.push(path),
        }
    }
    (top_level, subagents)
}

fn subagent_parent_session_id(path: &Path) -> Option<String> {
    let dir = path.parent()?;
    if dir.file_name()?.to_str()? != SUBAGENTS_DIR {
        return None;
    }
    dir.parent()?.file_name()?.to_str().map(ToOwned::to_owned)
}

#[derive(Debug, Default)]
struct SessionSummaryBuilder {
    file_path: PathBuf,
    session_id: Option<String>,
    first_user_text: Option<String>,
    meaningful_title: Option<String>,
    ai_title: Option<String>,
    custom_title: Option<String>,
    summary_title: Option<String>,
    started_at_raw: Option<String>,
    started_at_sort_key_ms: i64,
    git_branch: Option<String>,
    client_version: Option<String>,
    model: Option<String>,
}

/// Scans a transcript for list metadata. The header lines are parsed fully;
/// later lines are parsed only when they carry a title record, because a
/// title can be (re)generated at any point of a session.
pub(crate) fn scan_session_summary(path: &Path) -> Result<SessionSummary> {
    let owner = resolve_session_owner(path).unwrap_or(None);
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut builder = SessionSummaryBuilder {
        file_path: path.to_path_buf(),
        ..SessionSummaryBuilder::default()
    };

    let mut lines_seen = 0usize;
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            break;
        }
        lines_seen += 1;
        if line.len() > MAX_SUMMARY_LINE_BYTES {
            continue;
        }
        let in_header = lines_seen <= HEADER_LINE_LIMIT;
        if !in_header && !is_title_record_line(&line) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        builder.observe(&value);
    }

    Ok(builder.finish(owner))
}

fn is_title_record_line(line: &str) -> bool {
    [
        "\"type\":\"ai-title\"",
        "\"type\":\"custom-title\"",
        "\"type\":\"summary\"",
    ]
    .iter()
    .any(|marker| line.contains(marker))
}

impl SessionSummaryBuilder {
    fn observe(&mut self, value: &Value) {
        let text_field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        };

        if self.started_at_raw.is_none() {
            if let Some(timestamp) = value.get("timestamp").and_then(Value::as_str) {
                if let Some(ms) = crate::read::scan::parse_rfc3339_to_epoch_ms(timestamp) {
                    self.started_at_raw = Some(timestamp.to_string());
                    self.started_at_sort_key_ms = ms;
                }
            }
        }
        if self.session_id.is_none() {
            self.session_id = text_field("sessionId");
        }
        if self.git_branch.is_none() {
            self.git_branch = text_field("gitBranch");
        }
        if self.client_version.is_none() {
            self.client_version = text_field("version");
        }

        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "ai-title" => {
                if let Some(title) = text_field("aiTitle") {
                    self.ai_title = Some(truncate_single_line(&title, MAX_TITLE_CHARS));
                }
            }
            "custom-title" => {
                if let Some(title) = text_field("customTitle") {
                    self.custom_title = Some(truncate_single_line(&title, MAX_TITLE_CHARS));
                }
            }
            "summary" => {
                if let Some(title) = text_field("summary") {
                    self.summary_title = Some(truncate_single_line(&title, MAX_TITLE_CHARS));
                }
            }
            "user" => {
                if self.meaningful_title.is_some()
                    || value.get("isMeta").and_then(Value::as_bool) == Some(true)
                {
                    return;
                }
                let Some(text) = value.get("message").and_then(user_message_text) else {
                    return;
                };
                if self.first_user_text.is_none() {
                    self.first_user_text = Some(truncate_single_line(&text, MAX_TITLE_CHARS));
                }
                if is_meaningful_user_text(&text) {
                    self.meaningful_title = Some(truncate_single_line(&text, MAX_TITLE_CHARS));
                }
            }
            "assistant" if self.model.is_none() => {
                self.model = value
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                    .filter(|model| *model != "<synthetic>")
                    .map(ToOwned::to_owned);
            }
            _ => {}
        }
    }

    fn finish(self, owner: Option<SessionOwner>) -> SessionSummary {
        let cwd = owner
            .as_ref()
            .map(|owner| owner.cwd.clone())
            .unwrap_or_else(|| UNRESOLVED_SESSION_OWNER.to_string());
        let session_id = self
            .session_id
            .or_else(|| owner.and_then(|owner| owner.session_id))
            .unwrap_or_else(|| self.file_path.display().to_string());
        let title = self
            .custom_title
            .or(self.ai_title)
            .or(self.summary_title)
            .or(self.meaningful_title)
            .or(self.first_user_text)
            .unwrap_or_else(|| format!("Session {session_id}"));
        let (started_at_raw, started_at_label, started_at_sort_key_ms) = session_start_fields(
            self.started_at_raw,
            self.started_at_sort_key_ms,
            &self.file_path,
        );

        SessionSummary {
            harness: Harness::Claude,
            file_path: self.file_path,
            session_id,
            cwd,
            title,
            started_at_raw,
            started_at_label,
            started_at_sort_key_ms,
            git_branch: self.git_branch,
            git_commit: None,
            repo_url: None,
            model_provider: None,
            client_version: self.client_version,
            model: self.model,
            subagent_files: Vec::new(),
        }
    }
}

/// Text of a user message: a plain string, or the joined text blocks of a
/// block list. Tool results are not user text.
fn user_message_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join(" "))
        }
        _ => None,
    }
}

/// Filters out text Claude Code injects into the user role: slash-command
/// wrappers, local command output, reminders, notifications, and caveats.
fn is_meaningful_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    ![
        "<command-",
        "<local-command",
        "<system-reminder>",
        "<task-notification>",
        "<bash-input>",
        "<bash-stdout>",
        "<bash-stderr>",
        "Caveat:",
        "[Request interrupted",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

pub(crate) fn load_session_detail(path: &Path) -> Result<SessionDetail> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut detail = SessionDetail::default();
    let mut last_usage_key: Option<String> = None;
    let mut last_text_message_id: Option<String> = None;

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            break;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(message) = value.get("message") else {
            continue;
        };

        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                let is_meta = value.get("isMeta").and_then(Value::as_bool) == Some(true);
                match message.get("content") {
                    Some(Value::String(text)) => push_user_turn(&mut detail, text, is_meta),
                    Some(Value::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                                        push_user_turn(&mut detail, text, is_meta);
                                    }
                                }
                                Some("tool_result") => detail.tool_outputs += 1,
                                Some("image") => detail.input_images += 1,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                if message.get("model").and_then(Value::as_str) == Some("<synthetic>") {
                    continue;
                }
                let message_id = message.get("id").and_then(Value::as_str).unwrap_or("");
                for block in message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") if last_text_message_id.as_deref() != Some(message_id) => {
                            last_text_message_id = Some(message_id.to_string());
                            detail.assistant_messages += 1;
                        }
                        Some("tool_use") => detail.tool_calls += 1,
                        Some("thinking") | Some("redacted_thinking") => detail.thinking_blocks += 1,
                        _ => {}
                    }
                }
                // One response is written as one line per content block, each
                // repeating the same usage; count it once.
                let request_id = value.get("requestId").and_then(Value::as_str).unwrap_or("");
                let key = format!("{message_id}:{request_id}");
                if last_usage_key.as_deref() == Some(key.as_str()) {
                    continue;
                }
                last_usage_key = Some(key);
                if let Some(usage) = message.get("usage") {
                    let read =
                        |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0).max(0);
                    detail
                        .usage
                        .get_or_insert_with(TokenBreakdown::default)
                        .add(TokenBreakdown {
                            input: read("input_tokens"),
                            cache_write: read("cache_creation_input_tokens"),
                            cache_write_1h: 0,
                            cache_read: read("cache_read_input_tokens"),
                            output: read("output_tokens"),
                        });
                }
            }
            _ => {}
        }
    }

    Ok(detail)
}

fn push_user_turn(detail: &mut SessionDetail, text: &str, is_meta: bool) {
    let preview = truncate_single_line(text, MAX_TURN_PREVIEW_CHARS);
    if !is_meta && is_meaningful_user_text(text) {
        detail.meaningful_user_turns.push(preview.clone());
    }
    detail.all_user_turns.push(preview);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::scan::build_catalog;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("llmon-claude-history-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_lines(path: &Path, lines: &[Value]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(path, body).expect("write session");
    }

    fn user_text(cwd: &str, session: &str, timestamp: &str, text: &str) -> Value {
        serde_json::json!({
            "type": "user",
            "cwd": cwd,
            "sessionId": session,
            "timestamp": timestamp,
            "gitBranch": "main",
            "version": "2.1.0",
            "promptId": format!("p-{timestamp}"),
            "message": {"role": "user", "content": text}
        })
    }

    fn assistant_block(message_id: &str, block: Value, usage: [i64; 4]) -> Value {
        serde_json::json!({
            "type": "assistant",
            "cwd": "/work/app",
            "timestamp": "2026-09-01T10:00:05Z",
            "requestId": format!("req-{message_id}"),
            "message": {
                "id": message_id,
                "model": "claude-test-1",
                "role": "assistant",
                "content": [block],
                "usage": {
                    "input_tokens": usage[0],
                    "cache_creation_input_tokens": usage[1],
                    "cache_read_input_tokens": usage[2],
                    "output_tokens": usage[3]
                }
            }
        })
    }

    #[cfg(not(windows))]
    #[test]
    fn build_catalog_keeps_case_distinct_linux_paths_separate() {
        let root = make_temp_dir("group");
        let projects = root.join("projects");
        write_lines(
            &projects.join("a/a.jsonl"),
            &[
                user_text(
                    "/mnt/e/Work/app",
                    "a",
                    "2026-03-16T08:30:22Z",
                    "<command-name>/init</command-name>",
                ),
                user_text(
                    "/mnt/e/Work/app",
                    "a",
                    "2026-03-16T08:30:23Z",
                    "show me the session history",
                ),
            ],
        );
        write_lines(
            &projects.join("b/b.jsonl"),
            &[user_text(
                "/mnt/e/work/app",
                "b",
                "2026-03-16T09:30:22Z",
                "list prompts",
            )],
        );

        let catalog = build_catalog(Harness::Claude, &projects).expect("catalog");
        assert_eq!(catalog.projects.len(), 2);
        assert_eq!(catalog.projects[0].display_path, "/mnt/e/Work/app");
        assert_eq!(
            catalog.projects[0].sessions[0].title,
            "show me the session history"
        );
        assert_eq!(catalog.projects[0].sessions[0].harness, Harness::Claude);
        assert_eq!(catalog.projects[1].display_path, "/mnt/e/work/app");
        assert_eq!(catalog.projects[1].sessions[0].title, "list prompts");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn title_prefers_custom_then_latest_ai_title_over_prompt() {
        let root = make_temp_dir("titles");
        let projects = root.join("projects");
        let session = projects.join("-work-app/s1.jsonl");
        let mut lines = vec![user_text(
            "/work/app",
            "s1",
            "2026-09-01T10:00:00Z",
            "fix the build",
        )];
        lines.push(
            serde_json::json!({"type": "ai-title", "sessionId": "s1", "aiTitle": "Early title"}),
        );
        // Titles written after the header window must still be picked up.
        for _ in 0..(HEADER_LINE_LIMIT + 10) {
            lines.push(serde_json::json!({"type": "mode", "mode": "default"}));
        }
        lines.push(
            serde_json::json!({"type": "ai-title", "sessionId": "s1", "aiTitle": "Fix musl build"}),
        );
        write_lines(&session, &lines);

        let catalog = build_catalog(Harness::Claude, &projects).expect("catalog");
        let summary = &catalog.projects[0].sessions[0];
        assert_eq!(summary.title, "Fix musl build");
        assert_eq!(summary.session_id, "s1");
        assert_eq!(summary.git_branch.as_deref(), Some("main"));
        assert_eq!(summary.client_version.as_deref(), Some("2.1.0"));

        lines.push(serde_json::json!({
            "type": "custom-title",
            "sessionId": "s1",
            "customTitle": "Release prep"
        }));
        write_lines(&session, &lines);
        let catalog = build_catalog(Harness::Claude, &projects).expect("catalog");
        assert_eq!(catalog.projects[0].sessions[0].title, "Release prep");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_keeps_session_at_launch_cwd_after_cd() {
        let root = make_temp_dir("launch-cwd");
        let projects = root.join("projects");
        write_lines(
            &projects.join("-outside-Lantern/s.jsonl"),
            &[
                user_text(
                    "/outside/Lantern",
                    "s",
                    "2026-06-23T08:30:22Z",
                    "work on the project",
                ),
                user_text(
                    "/outside/other",
                    "s",
                    "2026-06-23T08:31:22Z",
                    "continue elsewhere",
                ),
            ],
        );

        let catalog = build_catalog(Harness::Claude, &projects).expect("catalog");
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].display_path, "/outside/Lantern");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn subagent_transcripts_nest_under_their_parent_session() {
        let root = make_temp_dir("subagents");
        let projects = root.join("projects");
        let session_id = "00000000-0000-4000-8000-000000000001";
        write_lines(
            &projects.join(format!("-work-app/{session_id}.jsonl")),
            &[user_text(
                "/work/app",
                session_id,
                "2026-09-01T10:00:00Z",
                "parent prompt",
            )],
        );
        let agent = projects.join(format!("-work-app/{session_id}/subagents/agent-1.jsonl"));
        write_lines(
            &agent,
            &[user_text(
                "/work/app",
                session_id,
                "2026-09-01T10:01:00Z",
                "subagent task",
            )],
        );
        let orphan = projects.join("-work-app/gone/subagents/agent-2.jsonl");
        write_lines(
            &orphan,
            &[user_text(
                "/work/app",
                "gone",
                "2026-09-01T09:00:00Z",
                "orphan task",
            )],
        );

        let catalog = build_catalog(Harness::Claude, &projects).expect("catalog");
        assert_eq!(catalog.projects.len(), 1);
        let sessions = &catalog.projects[0].sessions;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].title, "parent prompt");
        assert_eq!(sessions[0].subagent_files, vec![agent]);
        assert_eq!(sessions[1].title, "orphan task");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_keeps_sessions_with_no_owner_visible() {
        let root = make_temp_dir("unresolved-owner");
        let projects = root.join("projects");
        write_lines(
            &projects.join("x/unknown.jsonl"),
            &[serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "orphaned session"}
            })],
        );

        let catalog = build_catalog(Harness::Claude, &projects).expect("catalog");
        assert_eq!(catalog.files_scanned, 1);
        assert_eq!(catalog.files_skipped, 0);
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].display_path, UNRESOLVED_SESSION_OWNER);
        assert_eq!(catalog.projects[0].sessions[0].title, "orphaned session");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_session_detail_extracts_turns_tools_and_deduplicated_tokens() {
        let root = make_temp_dir("detail");
        let path = root.join("session.jsonl");
        let mut meta = user_text("/work/app", "s", "2026-09-01T10:00:00Z", "skill body");
        meta["isMeta"] = serde_json::json!(true);
        write_lines(
            &path,
            &[
                user_text(
                    "/work/app",
                    "s",
                    "2026-09-01T10:00:00Z",
                    "<command-name>/init</command-name>",
                ),
                meta,
                user_text("/work/app", "s", "2026-09-01T10:00:01Z", "show all prompts"),
                assistant_block(
                    "m1",
                    serde_json::json!({"type": "thinking", "thinking": ""}),
                    [10, 20, 30, 4],
                ),
                assistant_block(
                    "m1",
                    serde_json::json!({"type": "text", "text": "ok"}),
                    [10, 20, 30, 4],
                ),
                assistant_block(
                    "m1",
                    serde_json::json!({"type": "tool_use", "name": "Bash", "input": {}}),
                    [10, 20, 30, 4],
                ),
                serde_json::json!({
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t", "content": "done"}
                    ]}
                }),
                assistant_block(
                    "m2",
                    serde_json::json!({"type": "text", "text": "done"}),
                    [1, 0, 0, 1],
                ),
            ],
        );

        let detail = load_session_detail(&path).expect("detail");
        assert_eq!(detail.meaningful_user_turns, vec!["show all prompts"]);
        assert_eq!(detail.all_user_turns.len(), 3);
        assert_eq!(detail.assistant_messages, 2);
        assert_eq!(detail.tool_calls, 1);
        assert_eq!(detail.tool_outputs, 1);
        assert_eq!(detail.thinking_blocks, 1);
        assert_eq!(
            detail.usage,
            Some(TokenBreakdown {
                input: 11,
                cache_write: 20,
                cache_write_1h: 0,
                cache_read: 30,
                output: 5,
            })
        );
        assert_eq!(detail.usage.map(|usage| usage.total()), Some(66));
        assert_eq!(detail.total_tokens, None);

        let _ = std::fs::remove_dir_all(root);
    }
}
