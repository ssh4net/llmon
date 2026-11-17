//! Grok CLI usage parsing. Each session is a directory
//! `sessions/<url-encoded cwd>/<session id>/` whose `updates.jsonl` records
//! the spend of a turn once, at its end (`turn_completed`), as a delta.
//!
//! Subagents are sessions of their own, in the same cwd group. A parent's turn
//! also includes the usage of the children that finished while it ran
//! (`modelCalls` is larger than `numTurns`), and those children's own files
//! count too, so the children's recorded totals are taken off the parent's
//! turn. The children's files are read next to the parent's, by the ids of the
//! `subagent_finished` records before the turn; a child whose file is gone
//! stays in the parent's turn, where it is the only copy.
//!
//! Grok prunes old sessions at start-up, and llmon never starts Grok: the
//! aggregates of deleted logs live on in the archive like those of the other
//! harnesses. Field notes are in `docs/sources.md`.

use crate::usage::{
    add_agent_run, add_token_usage, cache_day_key_for_timestamp_ms, read_timestamp_ms,
    session_cwd_identity, unterminated_tail_is_final, CachedFileScanEntry, DailyTotals,
    FileScanSummary, HarnessParserState, TokenBreakdown, UsageZone,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Instant, SystemTime};

/// Name of the log that carries the usage; the other `.jsonl` files of a
/// session (chat, events, rewind points, prompt history) have none.
pub(crate) const USAGE_LOG_NAME: &str = "updates.jsonl";
/// Largest `summary.json` that is read.
const MAX_SUMMARY_BYTES: u64 = 1024 * 1024;
/// Largest child log that is read to take its usage off a parent's turn.
const MAX_CHILD_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Finished subagents remembered between two turns.
const MAX_PENDING_CHILDREN: usize = 64;
/// The model that usage without a per-model breakdown is booked under.
const DEFAULT_MODEL: &str = "grok-4.6";

/// Incremental parser state persisted with each cache row.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParserState {
    /// Session ids of the subagents that finished since the last completed
    /// turn: the turn that follows includes their usage.
    #[serde(default)]
    finished_children: Vec<String>,
}

/// One usage record: the whole turn, or one model's part of it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Usage {
    /// Prompt tokens, cached ones included.
    input: i64,
    output: i64,
    cached: i64,
    cache_write: i64,
    calls: i64,
    api_ms: i64,
}

impl Usage {
    fn from_json(value: &Value) -> Self {
        let read = |key: &str| -> i64 {
            value
                .get(key)
                .and_then(|raw| raw.as_i64().or_else(|| raw.as_f64().map(|v| v as i64)))
                .unwrap_or(0)
                .max(0)
        };
        Self {
            input: read("inputTokens"),
            output: read("outputTokens"),
            cached: read("cachedReadTokens"),
            cache_write: read("cacheCreationTokens"),
            calls: read("modelCalls"),
            api_ms: read("apiDurationMs"),
        }
    }

    fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached = self.cached.saturating_add(other.cached);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.calls = self.calls.saturating_add(other.calls);
        self.api_ms = self.api_ms.saturating_add(other.api_ms);
    }

    /// This usage without `other`, never below zero.
    fn without(self, other: Self) -> Self {
        Self {
            input: (self.input - other.input).max(0),
            output: (self.output - other.output).max(0),
            cached: (self.cached - other.cached).max(0),
            cache_write: (self.cache_write - other.cache_write).max(0),
            calls: (self.calls - other.calls).max(0),
            api_ms: (self.api_ms - other.api_ms).max(0),
        }
    }

    /// The columns of the screens: the prompt without the cached part (the
    /// cached part is its own column), cache writes, cache reads, and output
    /// (reasoning is part of it).
    fn tokens(self) -> TokenBreakdown {
        TokenBreakdown {
            input: (self.input - self.cached).max(0),
            cache_write: self.cache_write,
            cache_write_1h: 0,
            cache_read: self.cached.min(self.input),
            output: self.output,
        }
    }
}

/// A completed turn: the model parts of its usage, and its request counts.
struct Turn {
    models: BTreeMap<String, Usage>,
    /// Requests of the main agent only; `modelCalls` also counts the
    /// subagents that finished during the turn.
    main_turns: i64,
    total_calls: i64,
}

/// The model name of a usage record: Grok's records say `grok-4.6-build`,
/// everything else `grok-4.6`.
fn model_name(raw: &str) -> String {
    let name = raw.trim().strip_suffix("-build").unwrap_or(raw.trim());
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        "unknown".to_string()
    } else {
        name.to_string()
    }
}

/// The usage of a `turn_completed` record's `update`, or `None` when it has
/// none.
fn parse_turn(update: &Value) -> Option<Turn> {
    let usage = update.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let total = Usage::from_json(usage);
    let mut models: BTreeMap<String, Usage> = BTreeMap::new();
    if let Some(by_model) = usage.get("modelUsage").and_then(Value::as_object) {
        for (model, part) in by_model {
            models
                .entry(model_name(model))
                .or_default()
                .add(Usage::from_json(part));
        }
    }
    if models.is_empty() {
        models.insert(DEFAULT_MODEL.to_string(), total);
    }
    let main_turns = usage
        .get("numTurns")
        .and_then(Value::as_i64)
        .unwrap_or(total.calls)
        .max(0);
    Some(Turn {
        models,
        main_turns,
        total_calls: total.calls,
    })
}

/// The `update` object of a record of the given `sessionUpdate` type.
fn update_of<'a>(value: &'a Value, kind: &str) -> Option<&'a Value> {
    let update = value.get("params")?.get("update")?;
    (update.get("sessionUpdate")?.as_str()? == kind).then_some(update)
}

pub(crate) fn parse_file_summary(
    path: &Path,
    max_jsonl_line_bytes: usize,
    existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    let empty = || FileScanSummary::empty(HarnessParserState::Grok(ParserState::default()));
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(empty()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() || meta.len() == 0 {
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
    let session_cwd = resolve_session_cwd(path);
    let owner_matches_cache =
        existing.and_then(|entry| entry.session_cwd.as_deref()) == session_cwd.as_deref();

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
        .and_then(|entry| entry.parser_state.as_grok())
        .cloned()
        .unwrap_or_default();

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut fully_parsed = true;
    let tail_is_final = unterminated_tail_is_final(current_modified_epoch);

    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            fully_parsed = false;
            break;
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
            // Grok may still be writing this record: leave it for the next
            // refresh instead of consuming a truncated line.
            fully_parsed = false;
            break;
        }
        file_offset = file_offset.saturating_add(bytes_read as u64);
        if line.len() > max_jsonl_line_bytes {
            continue;
        }
        // Prompt text can contain these words: a hit is only a candidate.
        if !line.contains("turn_completed") && !line.contains("subagent_finished") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(update) = update_of(&value, "subagent_finished") {
            if let Some(child) = update.get("child_session_id").and_then(Value::as_str) {
                if state.finished_children.len() < MAX_PENDING_CHILDREN {
                    state.finished_children.push(child.to_string());
                }
            }
            continue;
        }
        let Some(update) = update_of(&value, "turn_completed") else {
            continue;
        };
        let children = std::mem::take(&mut state.finished_children);
        let Some(mut turn) = parse_turn(update) else {
            continue;
        };
        let Some(timestamp_ms) = read_timestamp_ms(&value) else {
            continue;
        };
        if turn.total_calls > turn.main_turns {
            for child in &children {
                for (model, recorded) in child_recorded(path, child, max_jsonl_line_bytes) {
                    if let Some(part) = turn.models.get_mut(&model) {
                        *part = part.without(recorded);
                    }
                }
            }
        }
        let mut api_ms = 0_i64;
        for (model, usage) in &turn.models {
            api_ms = api_ms.saturating_add(usage.api_ms);
            let tokens = usage.tokens();
            if tokens.total() > 0 {
                add_token_usage(
                    &mut daily,
                    &mut model_totals_by_day,
                    timestamp_ms,
                    model,
                    tokens,
                );
            }
        }
        add_agent_run(&mut daily, timestamp_ms);
        add_agent_ms(&mut daily, timestamp_ms, api_ms);
    }

    Ok(FileScanSummary {
        session_cwd,
        parser_state: HarnessParserState::Grok(state),
        file_offset: file_offset.min(file_len),
        fully_parsed: fully_parsed && file_offset >= file_len,
        daily,
        model_totals_by_day,
        deferred: false,
    })
}

/// The agent time of a turn: the time its requests spent in the API.
fn add_agent_ms(daily: &mut HashMap<String, DailyTotals>, timestamp_ms: i64, delta_ms: i64) {
    if delta_ms <= 0 {
        return;
    }
    for zone in [UsageZone::Local, UsageZone::Utc] {
        if let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) {
            let totals = daily.entry(day_key).or_default();
            totals.agent_ms = totals.agent_ms.saturating_add(delta_ms);
        }
    }
}

/// Everything a child session recorded in its own log, by model. Empty when
/// the id is not a plain session id or the log is missing or unreadable.
fn child_recorded(
    parent_log: &Path,
    child_id: &str,
    max_jsonl_line_bytes: usize,
) -> BTreeMap<String, Usage> {
    let mut totals: BTreeMap<String, Usage> = BTreeMap::new();
    let plain = !child_id.is_empty()
        && child_id.len() <= 128
        && child_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    // <group>/<parent id>/updates.jsonl -> <group>/<child id>/updates.jsonl
    let Some(group) = parent_log.parent().and_then(Path::parent) else {
        return totals;
    };
    if !plain {
        return totals;
    }
    let path = group.join(child_id).join(USAGE_LOG_NAME);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return totals;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_CHILD_LOG_BYTES {
        return totals;
    }
    let Ok(file) = File::open(&path) else {
        return totals;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > max_jsonl_line_bytes || !line.contains("turn_completed") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(turn) = update_of(&value, "turn_completed").and_then(parse_turn) else {
            continue;
        };
        for (model, usage) in turn.models {
            totals.entry(model).or_default().add(usage);
        }
    }
    totals
}

/// The directory a session was started in: `info.cwd` of its `summary.json`,
/// else the percent-decoded name of its group directory.
pub(crate) fn resolve_session_cwd(log_path: &Path) -> Option<String> {
    let session_dir = log_path.parent()?;
    if let Some(cwd) = summary_cwd(&session_dir.join("summary.json")) {
        return session_cwd_identity(&cwd);
    }
    let group = session_dir.parent()?.file_name()?.to_str()?;
    session_cwd_identity(&percent_decode(group)?)
}

fn summary_cwd(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_SUMMARY_BYTES {
        return None;
    }
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(MAX_SUMMARY_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("info")?.get("cwd")?.as_str().map(str::to_string)
}

/// `%2Fhome%2Fuser` -> `/home/user`; `None` for a malformed escape.
pub(crate) fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;
    use crate::usage::{compute_snapshot, LocalUsageSnapshot, ScanLimits};
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
    const GROUP: &str = "%2Ftmp%2Fllmon-grok-project";
    const CWD: &str = "/tmp/llmon-grok-project";

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
        let dir = std::env::temp_dir().join(format!("llmon-grok-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// `<grok dir>/sessions/<group>/<id>/` with a summary.json.
    fn make_session(grok_dir: &Path, id: &str) -> PathBuf {
        let dir = grok_dir.join("sessions").join(GROUP).join(id);
        std::fs::create_dir_all(&dir).expect("create session dir");
        std::fs::write(
            dir.join("summary.json"),
            serde_json::json!({"info": {"id": id, "cwd": CWD}}).to_string(),
        )
        .expect("write summary");
        dir
    }

    fn model_usage(
        input: i64,
        output: i64,
        cached: i64,
        calls: i64,
        api_ms: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "inputTokens": input, "outputTokens": output, "cachedReadTokens": cached,
            "cacheCreationTokens": 0, "reasoningTokens": 0, "totalTokens": input + output,
            "modelCalls": calls, "apiDurationMs": api_ms
        })
    }

    /// A `turn_completed` record. `rolled_up` is the usage of children that
    /// finished during the turn, already inside the numbers.
    fn turn_line(
        timestamp: i64,
        prompt_id: &str,
        usage: serde_json::Value,
        main_turns: i64,
    ) -> serde_json::Value {
        let mut total = usage.clone();
        total["numTurns"] = serde_json::json!(main_turns);
        total["modelUsage"] = serde_json::json!({ "grok-4.6-build": usage });
        serde_json::json!({
            "timestamp": timestamp,
            "method": "_x.ai/session/update",
            "params": {
                "_meta": {},
                "sessionId": "synthetic",
                "update": {
                    "sessionUpdate": "turn_completed",
                    "prompt_id": prompt_id,
                    "stop_reason": "end_turn",
                    "usage": total
                }
            }
        })
    }

    fn finished_line(timestamp: i64, child: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_finished",
                "child_session_id": child, "status": "completed"
            }}
        })
    }

    fn noise_line(timestamp: i64) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp, "method": "session/update",
            "params": {"update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"text": "mentions turn_completed in prose"}
            }}
        })
    }

    fn write_lines(path: &Path, lines: &[serde_json::Value]) {
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(path, text).expect("write log");
    }

    fn append_lines(path: &Path, lines: &[serde_json::Value]) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open log");
        for line in lines {
            writeln!(file, "{line}").expect("append line");
        }
    }

    fn test_limits() -> ScanLimits {
        ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 50,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        }
    }

    fn snapshot(grok_dir: &Path, cache: Option<&Path>) -> LocalUsageSnapshot {
        compute_snapshot(Harness::Grok, 30, grok_dir, None, test_limits(), cache).expect("snapshot")
    }

    fn total_tokens(snapshot: &LocalUsageSnapshot) -> TokenBreakdown {
        let mut total = TokenBreakdown::default();
        for day in snapshot.days_for_zone(UsageZone::Utc) {
            total.add(TokenBreakdown {
                input: day.input_tokens,
                cache_write: day.cache_write_tokens,
                cache_write_1h: 0,
                cache_read: day.cache_read_tokens,
                output: day.output_tokens,
            });
        }
        total
    }

    #[test]
    fn a_turn_counts_once_with_its_cache_split_and_model() {
        let root = make_temp_dir("turn");
        let dir = make_session(&root, "sess-a");
        let now = Utc::now().timestamp();
        write_lines(
            &dir.join(USAGE_LOG_NAME),
            &[
                noise_line(now),
                turn_line(now, "p1", model_usage(120_000, 800, 90_000, 3, 41_000), 3),
                turn_line(now + 5, "p2", model_usage(1_000, 200, 0, 1, 2_000), 1),
            ],
        );
        let snapshot = snapshot(&root, None);
        let total = total_tokens(&snapshot);
        // Input without the cached part, cache reads, and output.
        assert_eq!(total.input, 30_000 + 1_000);
        assert_eq!(total.cache_read, 90_000);
        assert_eq!(total.output, 1_000);
        assert_eq!(total.cache_write, 0);
        // The model is named without Grok's `-build` suffix.
        let names: Vec<&str> = snapshot
            .model_daily_for_zone(UsageZone::Utc)
            .iter()
            .map(|model| model.model.as_str())
            .collect();
        assert_eq!(names, vec!["grok-4.6"]);
        // Two turns are two runs; the API time of both is the agent time.
        let days = snapshot.days_for_zone(UsageZone::Utc);
        assert_eq!(days.iter().map(|day| day.agent_runs).sum::<i64>(), 2);
        assert_eq!(
            days.iter().map(|day| day.agent_time_ms).sum::<i64>(),
            43_000
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_session_belongs_to_the_cwd_of_its_summary() {
        let root = make_temp_dir("owner");
        let dir = make_session(&root, "sess-a");
        // (A Unix path has no drive, so it is not an absolute path on Windows
        // and neither side has a cwd there.)
        assert_eq!(
            resolve_session_cwd(&dir.join(USAGE_LOG_NAME)),
            session_cwd_identity(CWD)
        );
        // Without a summary the decoded group name is used.
        std::fs::remove_file(dir.join("summary.json")).expect("remove summary");
        assert_eq!(
            resolve_session_cwd(&dir.join(USAGE_LOG_NAME)),
            session_cwd_identity(CWD)
        );
        assert_eq!(percent_decode("%2Fa%2Fb").as_deref(), Some("/a/b"));
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("%zz"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_parent_turn_gives_up_the_usage_its_finished_children_count_themselves() {
        let root = make_temp_dir("rollup");
        let now = Utc::now().timestamp();
        // Child A (6 calls) and child B (4 calls) have their own logs.
        let child_a = make_session(&root, "child-a");
        write_lines(
            &child_a.join(USAGE_LOG_NAME),
            &[turn_line(
                now,
                "c1",
                model_usage(300_000, 2_500, 250_000, 6, 100),
                6,
            )],
        );
        let child_b = make_session(&root, "child-b");
        write_lines(
            &child_b.join(USAGE_LOG_NAME),
            &[turn_line(
                now,
                "c2",
                model_usage(200_000, 1_500, 150_000, 4, 100),
                4,
            )],
        );
        // The parent's turn: its own 5 calls plus both children (15 in all).
        let parent = make_session(&root, "parent");
        write_lines(
            &parent.join(USAGE_LOG_NAME),
            &[
                finished_line(now, "child-a"),
                finished_line(now, "child-b"),
                turn_line(
                    now + 1,
                    "p1",
                    model_usage(900_000, 7_000, 750_000, 15, 300),
                    5,
                ),
            ],
        );
        let total = total_tokens(&snapshot(&root, None));
        // The parent's turn has 900k prompt tokens, 500k of them the
        // children's. Together with the children's own logs the total is the
        // parent's 900k, counted once.
        assert_eq!(total.input + total.cache_read, 900_000);
        assert_eq!(total.cache_read, 750_000);
        assert_eq!(total.output, 7_000);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_child_that_finished_after_the_turn_or_whose_log_is_gone_is_not_taken_off() {
        let root = make_temp_dir("late");
        let now = Utc::now().timestamp();
        let parent = make_session(&root, "parent");
        // The child's log is missing: the parent's turn is the only copy.
        write_lines(
            &parent.join(USAGE_LOG_NAME),
            &[
                finished_line(now, "child-gone"),
                finished_line(now, "../escape"),
                turn_line(now + 1, "p1", model_usage(1_000, 100, 0, 8, 10), 5),
            ],
        );
        let total = total_tokens(&snapshot(&root, None));
        assert_eq!(total.input, 1_000);
        assert_eq!(total.output, 100);

        // A child that finishes after the turn is counted from its own log
        // and is not subtracted from a later turn.
        let late = make_session(&root, "child-late");
        write_lines(
            &late.join(USAGE_LOG_NAME),
            &[turn_line(now + 2, "c1", model_usage(500, 50, 0, 4, 10), 4)],
        );
        append_lines(
            &parent.join(USAGE_LOG_NAME),
            &[
                turn_line(now + 3, "p2", model_usage(200, 20, 0, 2, 10), 2),
                finished_line(now + 4, "child-late"),
            ],
        );
        let total = total_tokens(&snapshot(&root, None));
        assert_eq!(total.input, 1_000 + 500 + 200);
        assert_eq!(total.output, 100 + 50 + 20);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn appended_turns_resume_and_a_partial_last_line_waits() {
        let root = make_temp_dir("resume");
        let dir = make_session(&root, "sess-a");
        let log = dir.join(USAGE_LOG_NAME);
        let cache = root.join("cache.db");
        let now = Utc::now().timestamp();
        write_lines(
            &log,
            &[turn_line(now, "p1", model_usage(100, 10, 0, 1, 5), 1)],
        );
        assert_eq!(total_tokens(&snapshot(&root, Some(&cache))).input, 100);

        // A turn being written has no newline yet: it is left for later.
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&log)
                .expect("open");
            let line = turn_line(now + 1, "p2", model_usage(40, 4, 0, 1, 5), 1).to_string();
            write!(file, "{line}").expect("write partial");
        }
        assert_eq!(total_tokens(&snapshot(&root, Some(&cache))).input, 100);
        append_lines(&log, &[]);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&log)
                .expect("open");
            writeln!(file).expect("finish line");
        }
        append_lines(
            &log,
            &[turn_line(now + 2, "p3", model_usage(1, 1, 0, 1, 5), 1)],
        );
        assert_eq!(total_tokens(&snapshot(&root, Some(&cache))).input, 141);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_the_usage_log_of_a_session_is_scanned() {
        let root = make_temp_dir("files");
        let dir = make_session(&root, "sess-a");
        let now = Utc::now().timestamp();
        let line = turn_line(now, "p1", model_usage(100, 10, 0, 1, 5), 1);
        write_lines(&dir.join(USAGE_LOG_NAME), std::slice::from_ref(&line));
        // The same record in another log of the session or the group is not
        // read again.
        write_lines(&dir.join("chat_history.jsonl"), std::slice::from_ref(&line));
        write_lines(&dir.join("events.jsonl"), std::slice::from_ref(&line));
        let group = root.join("sessions").join(GROUP);
        write_lines(&group.join("prompt_history.jsonl"), &[line]);
        assert_eq!(total_tokens(&snapshot(&root, None)).input, 100);
        let _ = std::fs::remove_dir_all(root);
    }
}
