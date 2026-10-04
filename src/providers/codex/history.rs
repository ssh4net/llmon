//! Codex session history: list metadata (title, start time, model, git
//! context) and the turn/tool/token detail of one session log.

use crate::providers::codex::usage::{
    resolve_session_owner, SessionOwner, PROJECT_IDENTITY_LINE_LIMIT,
};
use crate::read::scan::{
    format_timestamp_label, parse_rfc3339_to_epoch_ms, system_time_to_epoch_ms,
    truncate_single_line, SessionDetail, SessionSummary, MAX_TITLE_CHARS, MAX_TURN_PREVIEW_CHARS,
    UNRESOLVED_SESSION_OWNER,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
struct SessionSummaryBuilder {
    file_path: PathBuf,
    session_id: Option<String>,
    first_user_text: Option<String>,
    meaningful_title: Option<String>,
    started_at_raw: Option<String>,
    started_at_sort_key_ms: i64,
    git_branch: Option<String>,
    git_commit: Option<String>,
    repo_url: Option<String>,
    model_provider: Option<String>,
    model: Option<String>,
}

pub(crate) fn load_session_detail(path: &Path) -> Result<SessionDetail> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut detail = SessionDetail::default();

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            break;
        }

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").and_then(Value::as_object);

        match entry_type {
            "response_item" => {
                if let Some(payload) = payload {
                    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                    match payload_type {
                        "message" => {
                            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                            if role == "user" {
                                let texts = extract_message_texts(payload, "input_text");
                                for text in texts {
                                    if is_meaningful_user_text(&text) {
                                        detail.meaningful_user_turns.push(truncate_single_line(
                                            &text,
                                            MAX_TURN_PREVIEW_CHARS,
                                        ));
                                    }
                                    detail
                                        .all_user_turns
                                        .push(truncate_single_line(&text, MAX_TURN_PREVIEW_CHARS));
                                }
                                detail.input_images += count_message_parts(payload, "input_image");
                            } else if role == "assistant"
                                && has_message_part(payload, "output_text")
                            {
                                detail.assistant_messages += 1;
                            }
                        }
                        "function_call" => {
                            detail.tool_calls += 1;
                        }
                        "function_call_output" => {
                            detail.tool_outputs += 1;
                        }
                        "reasoning"
                            if payload
                                .get("encrypted_content")
                                .and_then(Value::as_str)
                                .is_some() =>
                        {
                            detail.reasoning_encrypted = true;
                        }
                        _ => {}
                    }
                }
            }
            "event_msg" => {
                if let Some(payload) = payload {
                    if payload.get("type").and_then(Value::as_str) == Some("token_count") {
                        if let Some(info) = payload.get("info") {
                            if let Some(total_usage) =
                                find_nested_map(info, &["total_token_usage", "totalTokenUsage"])
                            {
                                detail.total_tokens = read_i64(total_usage.get("total_tokens"))
                                    .or_else(|| read_i64(total_usage.get("totalTokens")));
                                detail.input_tokens = read_i64(total_usage.get("input_tokens"))
                                    .or_else(|| read_i64(total_usage.get("inputTokens")));
                                detail.output_tokens = read_i64(total_usage.get("output_tokens"))
                                    .or_else(|| read_i64(total_usage.get("outputTokens")));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(detail)
}

pub(crate) fn scan_session_summary(path: &Path) -> Result<SessionSummary> {
    let owner = resolve_session_owner(path).unwrap_or(None);
    let resolved_session_id = owner.as_ref().and_then(|owner| owner.session_id.as_deref());
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

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match entry_type {
            "session_meta" => extract_session_meta(&mut builder, &value, resolved_session_id),
            "turn_context" => extract_turn_context(&mut builder, &value),
            "response_item" => extract_title_candidate(&mut builder, &value),
            _ => {}
        }

        if lines_seen >= PROJECT_IDENTITY_LINE_LIMIT {
            break;
        }
    }

    builder.finish(owner)
}

fn extract_session_meta(
    builder: &mut SessionSummaryBuilder,
    value: &Value,
    expected_session_id: Option<&str>,
) {
    let payload = match value.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return,
    };

    let session_id = payload.get("id").and_then(Value::as_str);
    if expected_session_id.is_some_and(|expected| session_id != Some(expected)) {
        return;
    }

    if builder.session_id.is_none() {
        builder.session_id = session_id.map(ToOwned::to_owned);
    }

    if builder.model_provider.is_none() {
        builder.model_provider = payload
            .get("model_provider")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    if builder.started_at_raw.is_none() {
        if let Some(timestamp) = payload.get("timestamp").and_then(Value::as_str) {
            builder.started_at_raw = Some(timestamp.to_string());
            builder.started_at_sort_key_ms = parse_rfc3339_to_epoch_ms(timestamp).unwrap_or(0);
        }
    }

    let git = match payload.get("git").and_then(Value::as_object) {
        Some(git) => git,
        None => return,
    };

    if builder.git_branch.is_none() {
        builder.git_branch = git
            .get("branch")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if builder.git_commit.is_none() {
        builder.git_commit = git
            .get("commit_hash")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if builder.repo_url.is_none() {
        builder.repo_url = git
            .get("repository_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

fn extract_turn_context(builder: &mut SessionSummaryBuilder, value: &Value) {
    let payload = match value.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return,
    };

    if builder.model.is_none() {
        builder.model = payload
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

fn extract_title_candidate(builder: &mut SessionSummaryBuilder, value: &Value) {
    let payload = match value.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return,
    };

    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return;
    }
    if payload.get("role").and_then(Value::as_str) != Some("user") {
        return;
    }

    let texts = extract_message_texts(payload, "input_text");
    for text in texts {
        if builder.first_user_text.is_none() {
            builder.first_user_text = Some(truncate_single_line(&text, MAX_TITLE_CHARS));
        }
        if builder.meaningful_title.is_none() && is_meaningful_user_text(&text) {
            builder.meaningful_title = Some(truncate_single_line(&text, MAX_TITLE_CHARS));
            break;
        }
    }
}

impl SessionSummaryBuilder {
    fn finish(self, owner: Option<SessionOwner>) -> Result<SessionSummary> {
        let cwd = owner
            .map(|owner| owner.cwd)
            .unwrap_or_else(|| UNRESOLVED_SESSION_OWNER.to_string());
        let session_id = self
            .session_id
            .unwrap_or_else(|| self.file_path.display().to_string());
        let title = self
            .meaningful_title
            .or(self.first_user_text)
            .unwrap_or_else(|| format!("Session {session_id}"));

        let (started_at_raw, started_at_label, started_at_sort_key_ms) =
            if let Some(raw) = self.started_at_raw {
                let label = format_timestamp_label(&raw);
                (Some(raw), label, self.started_at_sort_key_ms)
            } else {
                let file_time = std::fs::metadata(&self.file_path)
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(system_time_to_epoch_ms)
                    .unwrap_or(0);
                (None, "--".to_string(), file_time)
            };

        Ok(SessionSummary {
            file_path: self.file_path,
            session_id,
            cwd,
            title,
            started_at_raw,
            started_at_label,
            started_at_sort_key_ms,
            git_branch: self.git_branch,
            git_commit: self.git_commit,
            repo_url: self.repo_url,
            model_provider: self.model_provider,
            model: self.model,
        })
    }
}

fn extract_message_texts(payload: &serde_json::Map<String, Value>, part_type: &str) -> Vec<String> {
    let mut texts = Vec::new();
    let Some(items) = payload.get("content").and_then(Value::as_array) else {
        return texts;
    };
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some(part_type) {
            continue;
        }
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            texts.push(text.to_string());
        }
    }
    texts
}

fn count_message_parts(payload: &serde_json::Map<String, Value>, part_type: &str) -> usize {
    let Some(items) = payload.get("content").and_then(Value::as_array) else {
        return 0;
    };
    let mut count = 0usize;
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some(part_type) {
            count += 1;
        }
    }
    count
}

fn has_message_part(payload: &serde_json::Map<String, Value>, part_type: &str) -> bool {
    count_message_parts(payload, part_type) > 0
}

fn find_nested_map<'a>(
    value: &'a Value,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    for key in keys {
        if let Some(map) = value.get(*key).and_then(Value::as_object) {
            return Some(map);
        }
    }
    None
}

fn read_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|raw| i64::try_from(raw).ok()))
    })
}

fn is_meaningful_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("# AGENTS.md instructions") {
        return false;
    }
    if trimmed.starts_with("<environment_context>") {
        return false;
    }
    if trimmed.starts_with("<skill>") {
        return false;
    }
    true
}
