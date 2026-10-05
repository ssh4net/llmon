//! Claude Code transcript parsing: per-response token usage deduplicated by
//! `(message.id, requestId)`, runs per prompt, and agent time corrected by
//! the recorded turn durations.

use crate::usage::{
    add_agent_run, add_model_tokens_limited, cache_day_key_for_timestamp_ms, read_timestamp_ms,
    unterminated_tail_is_final, CachedFileScanEntry, DailyTotals, FileScanSummary,
    HarnessParserState, TokenBreakdown, UsageZone, MAX_ACTIVITY_GAP_MS,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::{Instant, SystemTime};

/// Incremental parser state persisted with each cache row so appended
/// transcripts resume at `file_offset` without replaying earlier lines.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParserState {
    #[serde(default)]
    last_activity_ms: Option<i64>,
    /// `message.id` + `requestId` of the last counted response. Claude Code
    /// writes one line per content block and repeats the usage on each; those
    /// lines are contiguous, so remembering the last key deduplicates.
    #[serde(default)]
    last_usage_key: Option<String>,
    /// Usage counted so far for `last_usage_key`. An earlier line of a
    /// response can carry a lower output count than a later one, so the
    /// response counts the largest value of each field.
    #[serde(default)]
    last_usage: TokenBreakdown,
    /// `promptId` of the current turn; a new value starts a new run.
    #[serde(default)]
    last_prompt_id: Option<String>,
    /// Gap-estimated agent time already counted for the current turn. A later
    /// `turn_duration` record replaces the estimate with the exact duration.
    #[serde(default)]
    turn_estimate_ms: i64,
}

pub(crate) fn parse_file_summary(
    path: &Path,
    max_jsonl_line_bytes: usize,
    existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    let empty = || FileScanSummary::empty(HarnessParserState::Claude(ParserState::default()));
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(empty()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(empty());
    }
    if meta.len() == 0 {
        return Ok(empty());
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(empty()),
    };
    let file_len = meta.len();
    let current_modified_epoch = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    // The owner (launch cwd) is resolved before parsing so every record in the
    // file is attributed to the same project.
    let session_cwd = super::resolve_session_owner(path)
        .ok()
        .flatten()
        .map(|owner| owner.cwd);
    let cached_owner = existing.and_then(|entry| entry.session_cwd.as_deref());
    let owner_matches_cache = cached_owner == session_cwd.as_deref();

    let can_resume = existing
        .filter(|_| owner_matches_cache)
        .filter(|entry| entry.file_offset > 0 && entry.file_offset <= file_len)
        .filter(|entry| {
            if entry.size < file_len {
                return true;
            }
            entry.size == file_len
                && !entry.fully_parsed
                && entry.modified_epoch_secs == current_modified_epoch
        })
        .is_some();
    let resumed = existing.filter(|_| can_resume);
    let mut file_offset: u64 = resumed.map(|entry| entry.file_offset).unwrap_or(0);
    if file_offset > 0 {
        file.seek(SeekFrom::Start(file_offset))
            .with_context(|| format!("Unable to seek {}", path.display()))?;
    }
    let mut daily: HashMap<String, DailyTotals> =
        resumed.map(|entry| entry.daily.clone()).unwrap_or_default();
    let mut model_totals_by_day: HashMap<String, HashMap<String, TokenBreakdown>> = resumed
        .map(|entry| entry.model_totals_by_day.clone())
        .unwrap_or_default();
    let mut state = resumed
        .and_then(|entry| entry.parser_state.as_claude())
        .cloned()
        .unwrap_or_default();

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut fully_parsed = true;
    let tail_is_final = unterminated_tail_is_final(current_modified_epoch);

    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                fully_parsed = false;
                break;
            }
        }

        line.clear();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        if !line.ends_with('\n') && !tail_is_final {
            // Claude Code may still be writing this record. Leave it for the
            // next refresh instead of consuming a truncated line. A tail that
            // has stopped changing was cut off and is read like any other line.
            fully_parsed = false;
            break;
        }
        file_offset = file_offset.saturating_add(bytes_read as u64);
        if line.len() > max_jsonl_line_bytes {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Records without a timestamp (for example `cost-state`) carry no
        // usage or activity.
        let Some(timestamp_ms) = read_timestamp_ms(&value) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "assistant" => {
                track_turn_activity(&mut daily, &mut state, timestamp_ms);
                let Some((key, model, usage)) = extract_assistant_usage(&value) else {
                    continue;
                };
                let counted = if state.last_usage_key.as_deref() == Some(key.as_str()) {
                    state.last_usage.excess_of(usage)
                } else {
                    state.last_usage_key = Some(key);
                    state.last_usage = TokenBreakdown::default();
                    usage
                };
                if counted.total() == 0 {
                    continue;
                }
                state.last_usage.add(counted);
                add_token_usage(
                    &mut daily,
                    &mut model_totals_by_day,
                    timestamp_ms,
                    &model,
                    counted,
                );
            }
            "user" => {
                if let Some(prompt_id) = value.get("promptId").and_then(Value::as_str) {
                    if state.last_prompt_id.as_deref() != Some(prompt_id) {
                        state.last_prompt_id = Some(prompt_id.to_string());
                        state.turn_estimate_ms = 0;
                        // A new turn does not bridge the idle gap before it.
                        state.last_activity_ms = None;
                        add_agent_run(&mut daily, timestamp_ms);
                    }
                }
                track_turn_activity(&mut daily, &mut state, timestamp_ms);
            }
            "system" => {
                if value.get("subtype").and_then(Value::as_str) == Some("turn_duration") {
                    if let Some(duration_ms) = value.get("durationMs").and_then(Value::as_i64) {
                        let correction = duration_ms.max(0) - state.turn_estimate_ms;
                        add_agent_ms(&mut daily, timestamp_ms, correction);
                        state.turn_estimate_ms = duration_ms.max(0);
                    }
                    // Records after the turn end (hooks, summaries) are idle time.
                    state.last_activity_ms = None;
                } else {
                    track_turn_activity(&mut daily, &mut state, timestamp_ms);
                }
            }
            _ => {}
        }
    }

    Ok(FileScanSummary {
        session_cwd,
        parser_state: HarnessParserState::Claude(state),
        file_offset: file_offset.min(file_len),
        fully_parsed: fully_parsed && file_offset >= file_len,
        daily,
        model_totals_by_day,
        deferred: false,
    })
}

/// Returns the dedupe key, model, and usage of an `assistant` record.
fn extract_assistant_usage(value: &Value) -> Option<(String, String, TokenBreakdown)> {
    let message = value.get("message")?.as_object()?;
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let usage = message.get("usage")?.as_object()?;
    let read = |key: &str| -> i64 {
        usage
            .get(key)
            .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|v| v as i64)))
            .unwrap_or(0)
            .max(0)
    };
    let cache_write = read("cache_creation_input_tokens");
    let cache_write_1h = usage
        .get("cache_creation")
        .and_then(|split| split.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, cache_write);
    let tokens = TokenBreakdown {
        input: read("input_tokens"),
        cache_write,
        cache_write_1h,
        cache_read: read("cache_read_input_tokens"),
        output: read("output_tokens"),
    };
    if tokens.total() == 0 {
        return None;
    }
    let message_id = message.get("id").and_then(Value::as_str).unwrap_or("");
    let request_id = value.get("requestId").and_then(Value::as_str).unwrap_or("");
    let key = if message_id.is_empty() && request_id.is_empty() {
        // No identity: fall back to the record uuid so the line counts once.
        value
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        format!("{message_id}:{request_id}")
    };
    Some((key, model.to_string(), tokens))
}

fn add_token_usage(
    daily: &mut HashMap<String, DailyTotals>,
    model_totals_by_day: &mut HashMap<String, HashMap<String, TokenBreakdown>>,
    timestamp_ms: i64,
    model: &str,
    usage: TokenBreakdown,
) {
    for zone in [UsageZone::Local, UsageZone::Utc] {
        let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) else {
            continue;
        };
        daily.entry(day_key.clone()).or_default().tokens.add(usage);
        add_model_tokens_limited(
            model_totals_by_day.entry(day_key).or_default(),
            model.to_string(),
            usage,
        );
    }
}

fn track_turn_activity(
    daily: &mut HashMap<String, DailyTotals>,
    state: &mut ParserState,
    timestamp_ms: i64,
) {
    if let Some(previous) = state.last_activity_ms {
        let delta = timestamp_ms - previous;
        if delta > 0 && delta <= MAX_ACTIVITY_GAP_MS {
            add_agent_ms(daily, timestamp_ms, delta);
            state.turn_estimate_ms = state.turn_estimate_ms.saturating_add(delta);
        }
    }
    state.last_activity_ms = Some(timestamp_ms);
}

/// Adds (or, for a turn-duration correction, subtracts) agent time; a day's
/// total never goes below zero.
fn add_agent_ms(daily: &mut HashMap<String, DailyTotals>, timestamp_ms: i64, delta_ms: i64) {
    if delta_ms == 0 {
        return;
    }
    for zone in [UsageZone::Local, UsageZone::Utc] {
        if let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) {
            let totals = daily.entry(day_key).or_default();
            totals.agent_ms = totals.agent_ms.saturating_add(delta_ms).max(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;
    use crate::usage::{compute_snapshot, LocalUsageSnapshot, ScanLimits};
    use chrono::{Duration, TimeZone, Utc};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
    const TEST_PROJECT_CWD: &str = "/tmp/llmon-test-project";

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("llmon-claude-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_claude_dir(root: &Path) -> (PathBuf, PathBuf) {
        let claude_dir = root.join("claude");
        let sessions_root = claude_dir.join("projects").join("-tmp-llmon-test-project");
        std::fs::create_dir_all(&sessions_root).expect("create projects dir");
        (claude_dir, sessions_root)
    }

    fn rfc3339(timestamp_ms: i64) -> String {
        Utc.timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339()
    }

    fn append_json_line(path: &Path, line: serde_json::Value) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open jsonl file for append");
        use std::io::Write as _;
        writeln!(file, "{line}").expect("append json line");
    }

    fn assistant_line(
        timestamp_ms: i64,
        cwd: &str,
        message_id: &str,
        usage: [i64; 4],
    ) -> serde_json::Value {
        let [input, cache_write, cache_read, output] = usage;
        serde_json::json!({
            "type": "assistant",
            "timestamp": rfc3339(timestamp_ms),
            "cwd": cwd,
            "sessionId": "00000000-0000-4000-8000-000000000001",
            "requestId": format!("req-{message_id}"),
            "message": {
                "id": message_id,
                "model": "claude-test-1",
                "role": "assistant",
                "content": [],
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": cache_write,
                    "cache_read_input_tokens": cache_read,
                    "output_tokens": output
                }
            }
        })
    }

    /// Appends one API response with the given usage columns
    /// (input, cache write, cache read, output).
    fn append_usage_line(path: &Path, timestamp_ms: i64, cwd: &str, usage: [i64; 4]) {
        let message_id = format!("msg-{}", TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        append_json_line(path, assistant_line(timestamp_ms, cwd, &message_id, usage));
    }

    fn write_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        let _ = std::fs::remove_file(path);
        append_token_file(path, timestamp_ms, input_tokens, output_tokens);
    }

    fn append_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        append_usage_line(
            path,
            timestamp_ms,
            TEST_PROJECT_CWD,
            [input_tokens, 0, 0, output_tokens],
        );
    }

    fn append_user_prompt(path: &Path, timestamp_ms: i64, prompt_id: &str) {
        append_json_line(
            path,
            serde_json::json!({
                "type": "user",
                "timestamp": rfc3339(timestamp_ms),
                "cwd": TEST_PROJECT_CWD,
                "promptId": prompt_id,
                "message": {"role": "user", "content": "synthetic prompt"}
            }),
        );
    }

    fn append_tool_result(path: &Path, timestamp_ms: i64, prompt_id: &str) {
        append_json_line(
            path,
            serde_json::json!({
                "type": "user",
                "timestamp": rfc3339(timestamp_ms),
                "cwd": TEST_PROJECT_CWD,
                "promptId": prompt_id,
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]
                }
            }),
        );
    }

    fn append_turn_duration(path: &Path, timestamp_ms: i64, duration_ms: i64) {
        append_json_line(
            path,
            serde_json::json!({
                "type": "system",
                "subtype": "turn_duration",
                "timestamp": rfc3339(timestamp_ms),
                "cwd": TEST_PROJECT_CWD,
                "durationMs": duration_ms
            }),
        );
    }

    fn test_limits() -> ScanLimits {
        ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        }
    }

    fn snapshot(claude_dir: &Path, cache_db_path: Option<&Path>) -> LocalUsageSnapshot {
        compute_snapshot(
            Harness::Claude,
            30,
            claude_dir,
            None,
            test_limits(),
            cache_db_path,
        )
        .expect("snapshot")
    }

    #[test]
    fn duplicate_content_block_lines_count_usage_once() {
        let root = make_temp_dir("dedupe");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let now_ms = Utc::now().timestamp_millis();
        // One response written as three content-block lines, then a second response.
        for offset in 0..3 {
            append_json_line(
                &path,
                assistant_line(now_ms + offset, TEST_PROJECT_CWD, "msg-a", [10, 20, 30, 40]),
            );
        }
        append_json_line(
            &path,
            assistant_line(now_ms + 10, TEST_PROJECT_CWD, "msg-b", [1, 2, 3, 4]),
        );

        let snapshot = snapshot(&claude_dir, None);
        let today = snapshot.utc_days.last().expect("today");
        assert_eq!(today.input_tokens, 11);
        assert_eq!(today.cache_write_tokens, 22);
        assert_eq!(today.cache_read_tokens, 33);
        assert_eq!(today.output_tokens, 44);
        assert_eq!(today.total_tokens, 110);
        assert_eq!(snapshot.utc_totals.cache_hit_rate_percent, 50.0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn repeated_lines_count_the_largest_usage_of_a_response() {
        let root = make_temp_dir("dedupe-largest");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        // An early content-block line can carry a partial output count.
        append_json_line(
            &path,
            assistant_line(now_ms, TEST_PROJECT_CWD, "msg-a", [2, 100, 1_000, 8]),
        );
        let first = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(first.utc_totals.last30_days_tokens, 1_110);

        // The rest of the response arrives after the refresh, with the final
        // output count, then a line repeating a lower count.
        append_json_line(
            &path,
            assistant_line(now_ms + 1, TEST_PROJECT_CWD, "msg-a", [2, 100, 1_000, 50]),
        );
        append_json_line(
            &path,
            assistant_line(now_ms + 2, TEST_PROJECT_CWD, "msg-a", [2, 100, 1_000, 8]),
        );
        let second = snapshot(&claude_dir, Some(&cache_db_path));
        let today = second.utc_days.last().expect("today");
        assert_eq!(today.input_tokens, 2);
        assert_eq!(today.cache_write_tokens, 100);
        assert_eq!(today.cache_read_tokens, 1_000);
        assert_eq!(today.output_tokens, 50);
        assert_eq!(second.utc_totals.last30_days_tokens, 1_152);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dedupe_survives_incremental_resume_between_duplicate_lines() {
        let root = make_temp_dir("dedupe-resume");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        append_json_line(
            &path,
            assistant_line(now_ms, TEST_PROJECT_CWD, "msg-a", [5, 0, 0, 5]),
        );
        let first = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(first.utc_totals.last30_days_tokens, 10);

        // The next content block of the same response arrives after the refresh.
        append_json_line(
            &path,
            assistant_line(now_ms + 1, TEST_PROJECT_CWD, "msg-a", [5, 0, 0, 5]),
        );
        let second = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(second.utc_totals.last30_days_tokens, 10);

        // The row and its parser state are stored under the Claude harness.
        let conn = rusqlite::Connection::open(&cache_db_path).expect("open cache db");
        let harnesses: Vec<String> = conn
            .prepare("SELECT harness FROM file_cache;")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();
        assert_eq!(harnesses, vec!["claude".to_string()]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn synthetic_and_unterminated_lines_are_not_counted() {
        let root = make_temp_dir("synthetic");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        let mut synthetic = assistant_line(now_ms, TEST_PROJECT_CWD, "msg-s", [100, 0, 0, 100]);
        synthetic["message"]["model"] = serde_json::json!("<synthetic>");
        append_json_line(&path, synthetic);
        append_token_file(&path, now_ms + 1, 7, 3);
        // A record Claude Code is still writing (no trailing newline).
        let partial = assistant_line(now_ms + 2, TEST_PROJECT_CWD, "msg-p", [50, 0, 0, 50]);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open for partial write");
            write!(file, "{partial}").expect("write partial line");
        }

        let first = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(first.utc_totals.last30_days_tokens, 10);
        assert_eq!(
            first.scan_pending_files, 1,
            "partial tail keeps the file pending"
        );

        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open to finish line");
            writeln!(file).expect("finish partial line");
        }
        let second = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(second.utc_totals.last30_days_tokens, 110);
        assert_eq!(second.scan_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn settled_unterminated_last_record_is_counted() {
        let root = make_temp_dir("settled-tail");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        append_token_file(&path, now_ms, 7, 3);
        // The last record lost its newline and the log stopped changing.
        let last = assistant_line(now_ms + 1, TEST_PROJECT_CWD, "msg-last", [50, 0, 0, 50]);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open for last record");
            write!(file, "{last}").expect("write last record");
            file.set_modified(SystemTime::now() - std::time::Duration::from_secs(60 * 60))
                .expect("age the log");
        }

        let snapshot = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 110);
        assert_eq!(snapshot.scan_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn runs_count_distinct_prompt_ids_and_time_uses_turn_duration() {
        let root = make_temp_dir("runs-time");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let start_ms = Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();

        // Turn 1: records span 60 s, the exact duration is 90 s.
        append_user_prompt(&path, start_ms, "p1");
        append_token_file(&path, start_ms + 30_000, 1, 1);
        append_tool_result(&path, start_ms + 45_000, "p1");
        append_token_file(&path, start_ms + 60_000, 1, 1);
        append_turn_duration(&path, start_ms + 61_000, 90_000);
        // Turn 2 after a long idle gap, without a turn_duration record: 20 s estimate.
        append_user_prompt(&path, start_ms + 3_600_000 / 2, "p2");
        append_token_file(&path, start_ms + 3_600_000 / 2 + 20_000, 1, 1);

        let snapshot = snapshot(&claude_dir, None);
        let runs: i64 = snapshot.utc_days.iter().map(|day| day.agent_runs).sum();
        let time_ms: i64 = snapshot.utc_days.iter().map(|day| day.agent_time_ms).sum();
        assert_eq!(runs, 2);
        assert_eq!(time_ms, 110_000);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn subagent_transcripts_are_scanned_with_their_own_owner() {
        let root = make_temp_dir("subagents");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let session_dir = sessions_root
            .join("00000000-0000-4000-8000-000000000001")
            .join("subagents");
        std::fs::create_dir_all(&session_dir).expect("create subagents dir");
        let now_ms = Utc::now().timestamp_millis();
        write_token_file(&sessions_root.join("main.jsonl"), now_ms, 100, 10);
        write_token_file(&session_dir.join("agent-1.jsonl"), now_ms + 1, 30, 5);

        let snapshot = snapshot(&claude_dir, None);
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 145);
        assert_eq!(snapshot.scan_total_files, 2);
        assert_eq!(snapshot.project_activity.len(), 1);
        assert_eq!(snapshot.project_activity[0].display_path, TEST_PROJECT_CWD);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owner_is_first_record_cwd_and_ignores_later_cwd_changes() {
        let root = make_temp_dir("owner");
        let (_claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("00000000-0000-4000-8000-00000000abcd.jsonl");
        append_json_line(
            &path,
            serde_json::json!({"type": "mode", "mode": "default"}),
        );
        append_usage_line(&path, 1_000, "/work/first", [1, 0, 0, 1]);
        append_usage_line(&path, 2_000, "/work/first/nested", [1, 0, 0, 1]);

        let owner = super::super::resolve_session_owner(&path)
            .expect("resolve owner")
            .expect("owner");
        assert_eq!(owner.cwd, "/work/first");
        assert_eq!(
            owner.session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn one_hour_cache_writes_are_recorded_per_model() {
        let root = make_temp_dir("one-hour-writes");
        let (_claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let now_ms = Utc::now().timestamp_millis();
        let mut line = assistant_line(now_ms, TEST_PROJECT_CWD, "msg-1", [1, 100, 0, 1]);
        line["message"]["usage"]["cache_creation"] =
            serde_json::json!({"ephemeral_5m_input_tokens": 40, "ephemeral_1h_input_tokens": 60});
        append_json_line(&path, line);
        // A cost-state record has no timestamp and no usage.
        append_json_line(
            &path,
            serde_json::json!({"type": "cost-state", "sessionId": "s", "totalCostUSD": 1.5}),
        );
        append_usage_line(&path, now_ms + 1, TEST_PROJECT_CWD, [0, 50, 0, 0]);

        let summary = parse_file_summary(&path, 512 * 1024, None, None).expect("parse");
        let mut total = TokenBreakdown::default();
        for (day_key, models) in &summary.model_totals_by_day {
            if day_key.starts_with("U:") {
                total.add(models["claude-test-1"]);
            }
        }
        assert_eq!(total.cache_write, 150);
        assert_eq!(total.cache_write_1h, 60);
        assert_eq!(total.total(), 152);
        assert_eq!(summary.session_cwd.as_deref(), Some(TEST_PROJECT_CWD));
        assert!(summary.fully_parsed);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deleted_transcripts_keep_counting_from_the_archive() {
        let root = make_temp_dir("archived");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        let old_path = sessions_root.join("old.jsonl");
        write_token_file(&old_path, now_ms - 1_000, 100, 10);
        write_token_file(&sessions_root.join("new.jsonl"), now_ms, 20, 2);
        let first = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(first.utc_totals.last30_days_tokens, 132);

        // Claude Code's cleanup removes the old transcript.
        std::fs::remove_file(&old_path).expect("delete transcript");
        let second = snapshot(&claude_dir, Some(&cache_db_path));
        assert_eq!(second.utc_totals.last30_days_tokens, 132);
        assert_eq!(second.scan_total_files, 1);
        assert_eq!(second.scan_pending_files, 0);
        assert_eq!(second.project_usage[0].total_tokens, 132);

        let _ = std::fs::remove_dir_all(root);
    }
}
