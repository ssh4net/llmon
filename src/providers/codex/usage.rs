//! Codex session log parsing: token deltas from cumulative `token_count`
//! events, fork replay handling, and the immutable session owner.

use crate::usage::{
    add_agent_run, add_model_tokens_limited, cache_day_key_for_timestamp_ms, is_uuid_like,
    parse_timestamp_value_ms, read_timestamp_ms, session_cwd_identity, track_activity,
    unterminated_tail_is_final, CachedFileScanEntry, DailyTotals, FileScanSummary,
    HarnessParserState, ScanCacheStore, SessionFileCandidate, TokenBreakdown, UsageZone,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

pub(crate) const PROJECT_IDENTITY_LINE_LIMIT: usize = 128;
const MAX_OWNER_IDENTITY_LINE_BYTES: usize = 512 * 1024;
const FORK_REPLAY_END_GAP_MS: i64 = 1_000;
const FORK_REPLAY_NO_TOKEN_GRACE_MS: i64 = 2_000;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct UsageTotals {
    pub(crate) input: i64,
    pub(crate) cached: i64,
    pub(crate) output: i64,
}

impl UsageTotals {
    fn any_positive(self) -> bool {
        self.input > 0 || self.cached > 0 || self.output > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParserState {
    #[serde(default)]
    previous_totals: Option<UsageTotals>,
    #[serde(default)]
    current_model: Option<String>,
    #[serde(default)]
    last_activity_ms: Option<i64>,
    #[serde(default)]
    first_session_meta_seen: bool,
    #[serde(default)]
    fork_replay: ForkReplayState,
    #[serde(default)]
    pub(crate) fork_parent_id: Option<String>,
    #[serde(default)]
    pub(crate) fork_baseline: Option<UsageTotals>,
    #[serde(default)]
    fork_live_started: bool,
    #[serde(default)]
    owner_source: Option<SessionOwnerSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionOwnerSource {
    SessionMeta,
    TurnContextFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionOwner {
    pub(crate) cwd: String,
    pub(crate) session_id: Option<String>,
    pub(crate) source: SessionOwnerSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub(crate) struct ForkReplayState {
    #[serde(default)]
    pub(crate) active: bool,
    #[serde(default)]
    pub(crate) done: bool,
    #[serde(default)]
    pub(crate) start_ms: Option<i64>,
    #[serde(default)]
    pub(crate) last_event_ms: Option<i64>,
    #[serde(default)]
    pub(crate) token_events: u32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ForkResolution {
    parent_id: Option<String>,
    baseline: Option<UsageTotals>,
}

impl ForkResolution {
    fn is_fork(&self) -> bool {
        self.parent_id.is_some()
    }

    fn unresolved(&self) -> bool {
        self.is_fork() && self.baseline.is_none()
    }
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let tail = stem;
    if tail.len() < 36 {
        return None;
    }
    let candidate = &tail[tail.len() - 36..];
    let valid = candidate.chars().enumerate().all(|(idx, ch)| match idx {
        8 | 13 | 18 | 23 => ch == '-',
        _ => ch.is_ascii_hexdigit(),
    });
    valid.then(|| candidate.to_string())
}

fn read_fork_metadata(path: &Path, max_jsonl_line_bytes: usize) -> Option<(String, i64)> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).ok()?;
    if bytes == 0 || line.len() > max_jsonl_line_bytes {
        return None;
    }
    let value = serde_json::from_str::<Value>(&line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?.as_object()?;
    let parent_id = payload.get("forked_from_id")?.as_str()?.to_string();
    let timestamp_ms = read_timestamp_ms(&value)
        .or_else(|| payload.get("timestamp").and_then(parse_timestamp_value_ms))?;
    Some((parent_id, timestamp_ms))
}

/// Finds only requested archived parents for fork baseline recovery. Archived
/// sessions deliberately never become scan candidates, cache rows, or progress
/// totals: they provide historical baselines for active fork children only.
fn find_archived_parent_paths(
    archived_sessions_root: &Path,
    parent_ids: &HashSet<String>,
) -> HashMap<String, PathBuf> {
    if parent_ids.is_empty() {
        return HashMap::new();
    }

    let mut found = HashMap::new();
    let mut stack = vec![archived_sessions_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file()
                || meta.len() == 0
                || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
            {
                continue;
            }
            let Some(parent_id) = session_id_from_path(&path) else {
                continue;
            };
            if parent_ids.contains(&parent_id) {
                found.entry(parent_id).or_insert(path);
                if found.len() == parent_ids.len() {
                    return found;
                }
            }
        }
    }

    found
}

pub(crate) fn resolve_fork_baselines(
    candidates: &[SessionFileCandidate],
    candidate_paths: &[String],
    planned_indices: &[usize],
    cache: &ScanCacheStore,
    archived_sessions_root: &Path,
    max_jsonl_line_bytes: usize,
) -> HashMap<String, ForkResolution> {
    let id_to_path: HashMap<String, &Path> = candidates
        .iter()
        .filter_map(|candidate| {
            session_id_from_path(&candidate.path).map(|id| (id, candidate.path.as_path()))
        })
        .collect();
    let mut resolutions = HashMap::<String, ForkResolution>::new();
    let mut requests = HashMap::<String, Vec<(String, i64)>>::new();

    for idx in planned_indices {
        let candidate = &candidates[*idx];
        let key = candidate_paths[*idx].clone();
        let Some((parent_id, fork_timestamp_ms)) =
            read_fork_metadata(&candidate.path, max_jsonl_line_bytes)
        else {
            continue;
        };
        let cached_baseline = cache
            .entries
            .get(&key)
            .and_then(|entry| entry.parser_state.as_codex())
            .filter(|state| state.fork_parent_id.as_deref() == Some(parent_id.as_str()))
            .and_then(|state| state.fork_baseline);
        resolutions.insert(
            key.clone(),
            ForkResolution {
                parent_id: Some(parent_id.clone()),
                baseline: cached_baseline,
            },
        );
        if cached_baseline.is_none() {
            requests
                .entry(parent_id)
                .or_default()
                .push((key, fork_timestamp_ms));
        }
    }

    let missing_parent_ids: HashSet<String> = requests
        .keys()
        .filter(|parent_id| !id_to_path.contains_key(parent_id.as_str()))
        .cloned()
        .collect();
    let archived_parent_paths =
        find_archived_parent_paths(archived_sessions_root, &missing_parent_ids);

    for (parent_id, mut parent_requests) in requests {
        let parent_path = id_to_path.get(&parent_id).copied().or_else(|| {
            archived_parent_paths
                .get(&parent_id)
                .map(|path| path.as_path())
        });
        let Some(parent_path) = parent_path else {
            continue;
        };
        parent_requests.sort_by_key(|(_, timestamp_ms)| *timestamp_ms);
        let resolved = scan_parent_baselines(parent_path, &parent_requests, max_jsonl_line_bytes);
        for (child_path, baseline) in resolved {
            if let Some(resolution) = resolutions.get_mut(&child_path) {
                resolution.baseline = baseline;
            }
        }
    }

    resolutions
}

fn scan_parent_baselines(
    parent_path: &Path,
    requests: &[(String, i64)],
    max_jsonl_line_bytes: usize,
) -> Vec<(String, Option<UsageTotals>)> {
    let mut out = Vec::with_capacity(requests.len());
    let Ok(file) = File::open(parent_path) else {
        return requests
            .iter()
            .map(|(child, _)| (child.clone(), None))
            .collect();
    };
    let mut reader = BufReader::new(file);
    let mut request_idx = 0usize;
    let mut totals: Option<UsageTotals> = None;
    let mut line = String::new();
    let mut reached_eof = false;

    loop {
        line.clear();
        let Ok(bytes) = reader.read_line(&mut line) else {
            break;
        };
        if bytes == 0 {
            reached_eof = true;
            break;
        }
        if line.len() > max_jsonl_line_bytes || !line.contains("token_count") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(timestamp_ms) = read_timestamp_ms(&value) else {
            continue;
        };
        while request_idx < requests.len() && requests[request_idx].1 < timestamp_ms {
            out.push((
                requests[request_idx].0.clone(),
                Some(totals.unwrap_or_default()),
            ));
            request_idx += 1;
        }
        let payload = value.get("payload").and_then(Value::as_object);
        if payload
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            != Some("token_count")
        {
            continue;
        }
        let info = payload
            .and_then(|payload| payload.get("info"))
            .and_then(Value::as_object);
        let Some(info) = info else {
            continue;
        };
        if let Some(total) = find_usage_map(info, &["total_token_usage", "totalTokenUsage"]) {
            totals = Some(UsageTotals {
                input: read_i64(total, &["input_tokens", "inputTokens"]),
                cached: read_i64(
                    total,
                    &[
                        "cached_input_tokens",
                        "cache_read_input_tokens",
                        "cachedInputTokens",
                        "cacheReadInputTokens",
                    ],
                ),
                output: read_i64(total, &["output_tokens", "outputTokens"]),
            });
        } else if let Some(last) = find_usage_map(info, &["last_token_usage", "lastTokenUsage"]) {
            let current = totals.get_or_insert_with(UsageTotals::default);
            current.input += read_i64(last, &["input_tokens", "inputTokens"]);
            current.cached += read_i64(
                last,
                &[
                    "cached_input_tokens",
                    "cache_read_input_tokens",
                    "cachedInputTokens",
                    "cacheReadInputTokens",
                ],
            );
            current.output += read_i64(last, &["output_tokens", "outputTokens"]);
        }
    }

    while request_idx < requests.len() {
        out.push((
            requests[request_idx].0.clone(),
            reached_eof.then_some(totals.unwrap_or_default()),
        ));
        request_idx += 1;
    }
    out
}

pub(crate) fn parse_file_summary(
    path: &Path,
    max_jsonl_line_bytes: usize,
    existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
    fork_resolution: ForkResolution,
) -> Result<FileScanSummary> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => {
            return Ok(FileScanSummary::empty(HarnessParserState::Codex(
                ParserState::default(),
            )))
        }
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(FileScanSummary::empty(HarnessParserState::Codex(
            ParserState::default(),
        )));
    }
    if meta.len() == 0 {
        return Ok(FileScanSummary::empty(HarnessParserState::Codex(
            ParserState::default(),
        )));
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            return Ok(FileScanSummary::empty(HarnessParserState::Codex(
                ParserState::default(),
            )))
        }
    };
    let file_len = meta.len();
    let current_modified_epoch = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    // Resolve ownership before consuming token deltas. This is intentionally
    // separate from mutable turn/settings metadata so every event in the file
    // is attributed to the same session owner.
    let resolved_owner = resolve_session_owner(path).ok().flatten();
    let session_cwd = resolved_owner.as_ref().map(|owner| owner.cwd.clone());
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
    let mut file_offset: u64 = if can_resume {
        existing.map(|entry| entry.file_offset).unwrap_or(0)
    } else {
        0
    };
    if file_offset > 0 {
        let _ = file.seek(SeekFrom::Start(file_offset));
    }

    let mut daily: HashMap<String, DailyTotals> = if can_resume {
        existing
            .map(|entry| entry.daily.clone())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut model_totals_by_day: HashMap<String, HashMap<String, TokenBreakdown>> = if can_resume {
        existing
            .map(|entry| entry.model_totals_by_day.clone())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut parser_state = if can_resume {
        existing
            .and_then(|entry| entry.parser_state.as_codex())
            .cloned()
            .unwrap_or_default()
    } else {
        ParserState::default()
    };
    parser_state.owner_source = resolved_owner.as_ref().map(|owner| owner.source);
    if fork_resolution.is_fork() {
        parser_state.fork_parent_id = fork_resolution.parent_id.clone();
        parser_state.fork_baseline = fork_resolution.baseline;
    }
    if fork_resolution.unresolved() {
        return Ok(FileScanSummary {
            deferred: true,
            ..FileScanSummary::empty(HarnessParserState::Codex(parser_state))
        });
    }
    let mut reader = BufReader::new(file);
    let mut previous_totals: Option<UsageTotals> = parser_state.previous_totals;
    let mut current_model: Option<String> = parser_state.current_model.clone();
    let mut last_activity_ms: Option<i64> = parser_state.last_activity_ms;
    let mut first_session_meta_seen = parser_state.first_session_meta_seen;
    let mut fork_replay = parser_state.fork_replay;
    let uses_parent_baseline = fork_resolution.is_fork();
    let parent_baseline = fork_resolution.baseline;
    let mut fork_live_started = parser_state.fork_live_started;
    let mut seen_runs: HashSet<i64> = HashSet::new();
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
            // Codex may still be writing this record. Consuming it now would
            // lose the rest of the line on the next refresh. A tail that has
            // stopped changing was cut off and is read like any other line.
            fully_parsed = false;
            break;
        }
        file_offset = file_offset.saturating_add(bytes_read as u64);
        if line.len() > max_jsonl_line_bytes {
            continue;
        }

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let entry_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");

        let started_fork_replay = if entry_type == "session_meta" {
            maybe_start_fork_replay(&value, &mut first_session_meta_seen, &mut fork_replay)
        } else {
            false
        };

        let event_timestamp_ms = read_timestamp_ms(&value);
        let skip_fork_replay = if uses_parent_baseline {
            !fork_live_started
                && (started_fork_replay
                    || fork_replay_should_skip_event(&mut fork_replay, event_timestamp_ms))
        } else {
            started_fork_replay
                || fork_replay_should_skip_event(&mut fork_replay, event_timestamp_ms)
        };

        if entry_type == "turn_context" {
            if uses_parent_baseline || !skip_fork_replay {
                if let Some(model) = extract_model_from_turn_context(&value) {
                    current_model = Some(model);
                }
            }
            continue;
        }

        if entry_type == "session_meta" {
            continue;
        }

        if entry_type == "event_msg" || entry_type.is_empty() {
            let payload = value.get("payload").and_then(|value| value.as_object());
            let payload_type = payload
                .and_then(|payload| payload.get("type"))
                .and_then(|value| value.as_str());

            if skip_fork_replay && payload_type != Some("token_count") {
                continue;
            }

            if payload_type == Some("agent_message") {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    if seen_runs.insert(timestamp_ms) {
                        add_agent_run(&mut daily, timestamp_ms);
                    }
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
                continue;
            }

            if payload_type == Some("agent_reasoning") {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
                continue;
            }

            if payload_type != Some("token_count") {
                continue;
            }

            let info = payload
                .and_then(|payload| payload.get("info"))
                .and_then(|v| v.as_object());
            let (input, cached, output, used_total) = if let Some(info) = info {
                if let Some(total) = find_usage_map(info, &["total_token_usage", "totalTokenUsage"])
                {
                    (
                        read_i64(total, &["input_tokens", "inputTokens"]),
                        read_i64(
                            total,
                            &[
                                "cached_input_tokens",
                                "cache_read_input_tokens",
                                "cachedInputTokens",
                                "cacheReadInputTokens",
                            ],
                        ),
                        read_i64(total, &["output_tokens", "outputTokens"]),
                        true,
                    )
                } else if let Some(last) =
                    find_usage_map(info, &["last_token_usage", "lastTokenUsage"])
                {
                    (
                        read_i64(last, &["input_tokens", "inputTokens"]),
                        read_i64(
                            last,
                            &[
                                "cached_input_tokens",
                                "cache_read_input_tokens",
                                "cachedInputTokens",
                                "cacheReadInputTokens",
                            ],
                        ),
                        read_i64(last, &["output_tokens", "outputTokens"]),
                        false,
                    )
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let mut delta = UsageTotals {
                input,
                cached,
                output,
            };

            if used_total {
                let prev = previous_totals.unwrap_or_default();
                let current = UsageTotals {
                    input,
                    cached,
                    output,
                };
                delta = if let Some(baseline) = parent_baseline {
                    UsageTotals {
                        input: (input - prev.input.max(baseline.input)).max(0),
                        cached: (cached - prev.cached.max(baseline.cached)).max(0),
                        output: (output - prev.output.max(baseline.output)).max(0),
                    }
                } else {
                    UsageTotals {
                        input: (input - prev.input).max(0),
                        cached: (cached - prev.cached).max(0),
                        output: (output - prev.output).max(0),
                    }
                };
                previous_totals = Some(current);
            } else {
                let prev = previous_totals.unwrap_or_default();
                let mut next = prev;
                next.input += delta.input;
                next.cached += delta.cached;
                next.output += delta.output;
                if let Some(baseline) = parent_baseline {
                    delta = UsageTotals {
                        input: (next.input - prev.input.max(baseline.input)).max(0),
                        cached: (next.cached - prev.cached.max(baseline.cached)).max(0),
                        output: (next.output - prev.output.max(baseline.output)).max(0),
                    };
                }
                previous_totals = Some(next);
            }

            if uses_parent_baseline && delta.any_positive() {
                fork_live_started = true;
            }
            if (uses_parent_baseline && !fork_live_started)
                || (!uses_parent_baseline && skip_fork_replay)
            {
                note_fork_replay_token(&mut fork_replay);
                continue;
            }

            if delta.input == 0 && delta.cached == 0 && delta.output == 0 {
                continue;
            }

            let timestamp_ms = event_timestamp_ms;
            if let Some(timestamp_ms) = timestamp_ms {
                let model = current_model
                    .clone()
                    .or_else(|| extract_model_from_token_count(&value))
                    .unwrap_or_else(|| "unknown".to_string());
                for zone in [UsageZone::Local, UsageZone::Utc] {
                    let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) else {
                        continue;
                    };
                    // Codex input includes cached input; split it so the
                    // breakdown fields add up to the total.
                    let cached_clamped = delta.cached.min(delta.input);
                    let tokens = TokenBreakdown {
                        input: delta.input - cached_clamped,
                        cache_write: 0,
                        cache_write_1h: 0,
                        cache_read: cached_clamped,
                        output: delta.output,
                    };
                    daily.entry(day_key.clone()).or_default().tokens.add(tokens);

                    let per_day_models = model_totals_by_day.entry(day_key).or_default();
                    add_model_tokens_limited(per_day_models, model.clone(), tokens);
                }
            }

            if let Some(timestamp_ms) = timestamp_ms {
                track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
            }
            continue;
        }

        if skip_fork_replay {
            continue;
        }

        if entry_type == "response_item" {
            let payload = value.get("payload").and_then(|value| value.as_object());
            let payload_type = payload
                .and_then(|payload| payload.get("type"))
                .and_then(|value| value.as_str());
            let role = payload
                .and_then(|payload| payload.get("role"))
                .and_then(|value| value.as_str())
                .unwrap_or("");

            if role == "assistant" {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    if seen_runs.insert(timestamp_ms) {
                        add_agent_run(&mut daily, timestamp_ms);
                    }
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
            } else if payload_type != Some("message") {
                if let Some(timestamp_ms) = event_timestamp_ms {
                    track_activity(&mut daily, &mut last_activity_ms, timestamp_ms);
                }
            }
        }
    }

    parser_state.previous_totals = previous_totals;
    parser_state.current_model = current_model;
    parser_state.last_activity_ms = last_activity_ms;
    parser_state.first_session_meta_seen = first_session_meta_seen;
    parser_state.fork_replay = fork_replay;
    parser_state.fork_live_started = fork_live_started;

    Ok(FileScanSummary {
        session_cwd,
        parser_state: HarnessParserState::Codex(parser_state),
        file_offset: file_offset.min(file_len),
        fully_parsed: fully_parsed && file_offset >= file_len,
        daily,
        model_totals_by_day,
        deferred: false,
    })
}

fn extract_model_from_turn_context(value: &Value) -> Option<String> {
    let payload = value.get("payload").and_then(|value| value.as_object())?;
    if let Some(model) = payload.get("model").and_then(|value| value.as_str()) {
        return Some(model.to_string());
    }
    let info = payload.get("info").and_then(|value| value.as_object())?;
    info.get("model")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn extract_model_from_token_count(value: &Value) -> Option<String> {
    let payload = value.get("payload").and_then(|value| value.as_object())?;
    let info = payload.get("info").and_then(|value| value.as_object());
    let model = info
        .and_then(|info| {
            info.get("model")
                .or_else(|| info.get("model_name"))
                .and_then(|value| value.as_str())
        })
        .or_else(|| payload.get("model").and_then(|value| value.as_str()))
        .or_else(|| value.get("model").and_then(|value| value.as_str()));
    model.map(|value| value.to_string())
}

fn find_usage_map<'a>(
    info: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    keys.iter()
        .find_map(|key| info.get(*key).and_then(|value| value.as_object()))
}

fn read_i64(map: &serde_json::Map<String, Value>, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|value| value as i64))
        })
        .unwrap_or(0)
}

fn maybe_start_fork_replay(
    value: &Value,
    first_session_meta_seen: &mut bool,
    replay: &mut ForkReplayState,
) -> bool {
    if *first_session_meta_seen {
        return false;
    }
    *first_session_meta_seen = true;

    let payload = value.get("payload").and_then(Value::as_object);
    let Some(payload) = payload else {
        return false;
    };
    if payload
        .get("forked_from_id")
        .and_then(Value::as_str)
        .is_none()
    {
        return false;
    }

    // The outer timestamp records when this JSONL event was emitted. The payload
    // timestamp can be earlier because preparing a large fork replay takes time;
    // using it as last_event_ms can falsely look like the end-of-replay gap.
    let start_ms = read_timestamp_ms(value)
        .or_else(|| payload.get("timestamp").and_then(parse_timestamp_value_ms));
    *replay = ForkReplayState {
        active: true,
        done: false,
        start_ms,
        last_event_ms: start_ms,
        token_events: 0,
    };
    true
}

pub(crate) fn fork_replay_should_skip_event(
    replay: &mut ForkReplayState,
    event_timestamp_ms: Option<i64>,
) -> bool {
    if !replay.active || replay.done {
        return false;
    }

    let Some(timestamp_ms) = event_timestamp_ms else {
        return true;
    };
    let start_ms = replay.start_ms.unwrap_or(timestamp_ms);
    let elapsed_ms = timestamp_ms - start_ms;
    let previous_event_ms = replay.last_event_ms;
    let gap_ms = previous_event_ms
        .map(|last_ms| timestamp_ms - last_ms)
        .unwrap_or(0);
    let monotonic_event_ms = previous_event_ms
        .map(|last_ms| last_ms.max(timestamp_ms))
        .unwrap_or(timestamp_ms);

    if gap_ms >= FORK_REPLAY_END_GAP_MS
        || (replay.token_events == 0 && elapsed_ms >= FORK_REPLAY_NO_TOKEN_GRACE_MS)
    {
        replay.active = false;
        replay.done = true;
        replay.last_event_ms = Some(monotonic_event_ms);
        return false;
    }

    replay.last_event_ms = Some(monotonic_event_ms);
    true
}

fn note_fork_replay_token(replay: &mut ForkReplayState) {
    if replay.active && !replay.done {
        replay.token_events = replay.token_events.saturating_add(1);
    }
}

/// Resolve the one immutable owner for a Codex session.
///
/// The normal path accepts the first valid matching `session_meta.cwd` inside
/// the 128-line header. If that header is absent or damaged, recovery streams
/// the remaining file for session-meta only, then uses the first turn-context
/// cwd only when EOF proves no usable session-meta exists. Settings, tool
/// workdirs, and permission roots are deliberately excluded.
pub(crate) fn resolve_session_owner(path: &Path) -> Result<Option<SessionOwner>> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut probe = OwnerProbe::new(session_id_from_rollout_path(path));
    let mut lines_seen = 0usize;

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            return Ok(probe.turn_context_fallback());
        }
        lines_seen += 1;
        if line.len() <= MAX_OWNER_IDENTITY_LINE_BYTES {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if let Some(owner) = probe.observe(&value) {
                    return Ok(Some(owner));
                }
            }
        }
        if lines_seen >= PROJECT_IDENTITY_LINE_LIMIT {
            break;
        }
    }

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            return Ok(probe.turn_context_fallback());
        }
        if line.len() > MAX_OWNER_IDENTITY_LINE_BYTES {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if let Some(owner) = probe.observe(&value) {
                return Ok(Some(owner));
            }
        }
    }
}

struct OwnerProbe {
    expected_session_id: Option<String>,
    fallback_turn_context_cwd: Option<String>,
}

impl OwnerProbe {
    fn new(expected_session_id: Option<String>) -> Self {
        Self {
            expected_session_id,
            fallback_turn_context_cwd: None,
        }
    }

    fn observe(&mut self, value: &Value) -> Option<SessionOwner> {
        let entry_type = value.get("type").and_then(Value::as_str)?;
        let payload = value.get("payload")?.as_object()?;
        match entry_type {
            "session_meta" => {
                let session_id = payload.get("id")?.as_str()?.trim();
                if session_id.is_empty() {
                    return None;
                }
                if let Some(expected) = self.expected_session_id.as_deref() {
                    if expected != session_id {
                        return None;
                    }
                } else {
                    self.expected_session_id = Some(session_id.to_string());
                }
                let cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(session_cwd_identity)?;
                Some(SessionOwner {
                    cwd,
                    session_id: Some(session_id.to_string()),
                    source: SessionOwnerSource::SessionMeta,
                })
            }
            "turn_context" => {
                if self.fallback_turn_context_cwd.is_none() {
                    self.fallback_turn_context_cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .and_then(session_cwd_identity);
                }
                None
            }
            _ => None,
        }
    }

    fn turn_context_fallback(self) -> Option<SessionOwner> {
        let Self {
            expected_session_id,
            fallback_turn_context_cwd,
        } = self;
        fallback_turn_context_cwd.map(|cwd| SessionOwner {
            cwd,
            session_id: expected_session_id,
            source: SessionOwnerSource::TurnContextFallback,
        })
    }
}

fn session_id_from_rollout_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.get(stem.len().checked_sub(36)?..)?;
    if is_uuid_like(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}
