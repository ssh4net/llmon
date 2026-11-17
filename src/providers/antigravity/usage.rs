//! Antigravity CLI usage parsing. Each conversation is a SQLite file
//! `conversations/<id>.db` of protobuf blobs (no published schema; field notes
//! are in `docs/sources.md`). Its model calls come from two places:
//!
//! - `gen_metadata`: one row per call, with the usage at field `1/4`;
//! - `steps` of type 23 (checkpoint, a context summary the CLI asks the model
//!   for): the same usage message at field `28/2`, which `gen_metadata` lacks.
//!
//! Steps of type 15 repeat the usage of their call and only give it a time:
//! they are never counted. A changed file is read again in full (the
//! conversations are small next to their prompt text, which is never read).
//! The files are opened read-only, and llmon never starts `agy`: a start
//! updates the program and rewrites the login.

use super::proto::Message;
use crate::usage::{
    add_agent_run, add_token_usage, cache_day_key_for_timestamp_ms, session_cwd_identity,
    CachedFileScanEntry, DailyTotals, FileScanSummary, HarnessParserState, TokenBreakdown,
    UsageZone,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Step types of the `steps` table.
const STEP_USER_INPUT: i64 = 14;
const STEP_PLANNER_RESPONSE: i64 = 15;
const STEP_CHECKPOINT: i64 = 23;

/// Largest blob row that is decoded; the rows hold the prompt text, and a
/// usage message is a few hundred bytes at the start of the first field.
const MAX_ROW_BYTES: usize = 64 * 1024 * 1024;
/// How long a read waits for a writer before the file is tried next time.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// No state is kept between reads: a changed file is read in full.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParserState {}

/// The usage message of a call.
#[derive(Debug, Clone, Default, PartialEq)]
struct CallUsage {
    model: u64,
    /// Prompt tokens that were not read from the cache.
    input: i64,
    /// Output tokens, thinking included.
    output: i64,
    cache_write: i64,
    cache_read: i64,
    response_id: String,
}

impl CallUsage {
    fn from_message(usage: &Message<'_>) -> Self {
        let read = |number: u32| i64::try_from(usage.varint(number)).unwrap_or(i64::MAX);
        Self {
            model: usage.varint(1),
            input: read(2),
            output: read(3),
            cache_write: read(4),
            cache_read: read(5),
            response_id: usage.string(11).unwrap_or_default().to_string(),
        }
    }

    /// A row that only names a model (an aborted call, or the companion of a
    /// checkpoint) has no tokens and is not a call.
    fn counts(&self) -> bool {
        self.input > 0 || self.output > 0 || self.cache_write > 0 || self.cache_read > 0
    }

    fn tokens(&self) -> TokenBreakdown {
        TokenBreakdown {
            input: self.input.max(0),
            cache_write: self.cache_write.max(0),
            cache_write_1h: 0,
            cache_read: self.cache_read.max(0),
            output: self.output.max(0),
        }
    }
}

/// The model of a call: by its enum, else by the backend model string with
/// its variant suffix cut off, else the enum number.
fn model_name(model: u64, backend: Option<&str>) -> String {
    match model {
        1417 => return "gemini-4-argon".to_string(),
        1318 => return "gemini-3.8-flash".to_string(),
        1402 => return "claude-opus-5-5".to_string(),
        _ => {}
    }
    if let Some(backend) = backend.map(str::trim).filter(|name| !name.is_empty()) {
        let mut name = backend;
        for _ in 0..2 {
            for suffix in ["-trial", "-low", "-medium", "-high", "-a", "-n"] {
                if let Some(stripped) = name.strip_suffix(suffix) {
                    name = stripped;
                    break;
                }
            }
        }
        if name.len() <= 128 && !name.is_empty() && !name.chars().any(char::is_control) {
            return name.to_string();
        }
    }
    format!("agy-model-{model}")
}

/// A protobuf `Timestamp` {1 seconds, 2 nanos} in milliseconds.
fn timestamp_ms(message: &Message<'_>) -> Option<i64> {
    let seconds = i64::try_from(message.varint(1)).ok().filter(|s| *s > 0)?;
    let nanos = i64::try_from(message.varint(2))
        .unwrap_or(0)
        .clamp(0, 999_999_999);
    Some(
        seconds
            .saturating_mul(1000)
            .saturating_add(nanos / 1_000_000),
    )
}

/// A protobuf `Duration` {1 seconds, 2 nanos} in milliseconds.
fn duration_ms(message: &Message<'_>) -> i64 {
    let seconds = i64::try_from(message.varint(1))
        .unwrap_or(0)
        .clamp(0, 86_400);
    let nanos = i64::try_from(message.varint(2))
        .unwrap_or(0)
        .clamp(0, 999_999_999);
    seconds * 1000 + nanos / 1_000_000
}

/// What a step of interest says.
struct Step {
    kind: i64,
    /// Start of the call or turn, in milliseconds.
    at_ms: Option<i64>,
    /// `gen_metadata` index the step is linked to (field `20/3`, omitted
    /// when it is 0).
    gen_idx: i64,
    /// The usage of a checkpoint (field `28/2`).
    checkpoint: Option<CallUsage>,
}

fn read_step(kind: i64, metadata: &[u8]) -> Option<Step> {
    let message = Message::parse(metadata)?;
    let gen_idx = message
        .message(20)
        .map(|link| i64::try_from(link.varint(3)).unwrap_or(0))
        .unwrap_or(0);
    let checkpoint = (kind == STEP_CHECKPOINT)
        .then(|| {
            message
                .message(28)
                .and_then(|checkpoint| checkpoint.message(2))
                .map(|usage| CallUsage::from_message(&usage))
        })
        .flatten();
    Some(Step {
        kind,
        at_ms: message.message(1).and_then(|at| timestamp_ms(&at)),
        gen_idx,
        checkpoint,
    })
}

/// A `Connection` for reading. A conversation with a write-ahead log (a live
/// one, whose latest rows are not in the main file yet) is opened normally,
/// read-only; without one the file is opened `immutable`, which reads it
/// without creating the `-wal` and `-shm` files next to the user's data.
fn open_read_only(path: &Path) -> Result<Connection> {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let has_wal = std::fs::metadata(&wal).is_ok_and(|meta| meta.len() > 0);
    let connection = if has_wal {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    } else {
        let uri = format!("file://{}?immutable=1&mode=ro", uri_path(path));
        Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
    }
    .with_context(|| format!("Unable to open {}", path.display()))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}

/// The path of a SQLite URI (`file://` + this): forward slashes, a leading
/// slash before a Windows drive (`/C:/...`), and everything else but plain
/// path characters escaped.
fn uri_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = if text.as_bytes().get(1) == Some(&b':') {
        format!("/{text}")
    } else {
        text
    };
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The workspace of a conversation: `file:///...` at field 7 of its
/// `trajectory_metadata_blob` row, else the first URI of field 1.
fn read_workspace(connection: &Connection) -> Option<String> {
    let blob: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let message = Message::parse(&blob)?;
    let uri = message
        .string(7)
        .map(str::to_string)
        .or_else(|| message.message(1)?.string(1).map(str::to_string))?;
    let path = uri.strip_prefix("file://")?;
    session_cwd_identity(&crate::providers::grok::usage::percent_decode(path)?)
}

/// Creation time of the conversation (field `2/1`, seconds), for calls that no
/// step dates.
fn read_created_ms(connection: &Connection) -> Option<i64> {
    let blob: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    timestamp_ms(&Message::parse(&blob)?.message(2)?)
}

pub(crate) fn parse_file_summary(
    path: &Path,
    _max_jsonl_line_bytes: usize,
    _existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    let empty = || FileScanSummary::empty(HarnessParserState::Antigravity(ParserState::default()));
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return Ok(empty()),
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() || !file_type.is_file() || meta.len() == 0 {
        return Ok(empty());
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        // Not read this time; the row stays as it was.
        return Ok(empty());
    }
    let file_mtime_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(0));

    let connection = open_read_only(path)?;
    let workspace = read_workspace(&connection);
    let created_ms = read_created_ms(&connection);

    // Steps: the time of each call, the checkpoints, and the user's inputs.
    let mut gen_times: HashMap<i64, i64> = HashMap::new();
    let mut checkpoints: Vec<Step> = Vec::new();
    let mut inputs: Vec<i64> = Vec::new();
    if let Ok(mut statement) = connection.prepare(
        "SELECT step_type, metadata FROM steps WHERE step_type IN (14, 15, 23) ORDER BY idx",
    ) {
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        });
        for (kind, metadata) in rows.into_iter().flatten().flatten() {
            let Some(metadata) = metadata.filter(|blob| blob.len() <= MAX_ROW_BYTES) else {
                continue;
            };
            let Some(step) = read_step(kind, &metadata) else {
                continue;
            };
            match step.kind {
                STEP_PLANNER_RESPONSE => {
                    if let Some(at_ms) = step.at_ms {
                        gen_times.entry(step.gen_idx).or_insert(at_ms);
                    }
                }
                STEP_CHECKPOINT => checkpoints.push(step),
                STEP_USER_INPUT => inputs.extend(step.at_ms),
                _ => {}
            }
        }
    }

    // Calls: (day time, model, usage, API time). Rows are read in order.
    struct Call {
        at_ms: i64,
        model: String,
        usage: CallUsage,
        api_ms: i64,
    }
    let mut calls: Vec<Call> = Vec::new();
    let mut counted_gens: BTreeMap<i64, String> = BTreeMap::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let fallback_ms = created_ms.or(file_mtime_ms).unwrap_or(0);
    let mut gen_rows: Vec<(i64, Vec<u8>)> = Vec::new();
    if let Ok(mut statement) = connection.prepare("SELECT idx, data FROM gen_metadata ORDER BY idx")
    {
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        });
        for (idx, data) in rows.into_iter().flatten().flatten() {
            if let Some(data) = data.filter(|blob| blob.len() <= MAX_ROW_BYTES) {
                gen_rows.push((idx, data));
            }
        }
    }
    for (idx, data) in &gen_rows {
        // A row that does not parse is skipped, never the file.
        let Some(row) = Message::parse(data) else {
            continue;
        };
        let Some(chat) = row.message(1) else {
            continue;
        };
        let Some(usage_message) = chat.message(4) else {
            continue;
        };
        let usage = CallUsage::from_message(&usage_message);
        if !usage.counts() {
            continue;
        }
        let key = if usage.response_id.is_empty() {
            format!("gen:{idx}")
        } else {
            usage.response_id.clone()
        };
        if !seen_ids.insert(key) {
            continue;
        }
        let model_enum = if usage.model != 0 {
            usage.model
        } else {
            chat.varint(3)
        };
        let model = model_name(model_enum, chat.string(19));
        // The call's start: its planner step, else the nearest dated call.
        let at_ms = gen_times
            .get(idx)
            .or_else(|| gen_times.get(&(idx - 1)))
            .or_else(|| gen_times.get(&(idx + 1)))
            .copied()
            .unwrap_or(fallback_ms);
        let api_ms = chat.message(11).map_or(0, |d| duration_ms(&d))
            + chat.message(12).map_or(0, |d| duration_ms(&d));
        counted_gens.insert(*idx, model.clone());
        calls.push(Call {
            at_ms,
            model,
            usage,
            api_ms,
        });
    }
    // Checkpoints have no model: the call before them is the model's.
    for step in checkpoints {
        let Some(usage) = step.checkpoint.filter(CallUsage::counts) else {
            continue;
        };
        let key = if usage.response_id.is_empty() {
            format!("ckpt:{}", step.gen_idx)
        } else {
            usage.response_id.clone()
        };
        if !seen_ids.insert(key) {
            continue;
        }
        let model = counted_gens
            .range(..step.gen_idx)
            .next_back()
            .or_else(|| counted_gens.iter().next())
            .map_or_else(|| "unknown".to_string(), |(_, model)| model.clone());
        calls.push(Call {
            at_ms: step.at_ms.unwrap_or(fallback_ms),
            model,
            usage,
            api_ms: 0,
        });
    }

    let mut daily: HashMap<String, DailyTotals> = HashMap::new();
    let mut model_totals_by_day: HashMap<String, HashMap<String, TokenBreakdown>> = HashMap::new();
    for call in &calls {
        if call.at_ms <= 0 {
            continue;
        }
        add_token_usage(
            &mut daily,
            &mut model_totals_by_day,
            call.at_ms,
            &call.model,
            call.usage.tokens(),
        );
        if call.api_ms > 0 {
            for zone in [UsageZone::Local, UsageZone::Utc] {
                if let Some(day_key) = cache_day_key_for_timestamp_ms(call.at_ms, zone) {
                    let totals = daily.entry(day_key).or_default();
                    totals.agent_ms = totals.agent_ms.saturating_add(call.api_ms);
                }
            }
        }
    }
    for at_ms in inputs {
        add_agent_run(&mut daily, at_ms);
    }

    Ok(FileScanSummary {
        session_cwd: workspace,
        parser_state: HarnessParserState::Antigravity(ParserState::default()),
        // Read in full: the shared scanner clamps this to the file's size.
        file_offset: u64::MAX,
        fully_parsed: true,
        daily,
        model_totals_by_day,
        deferred: false,
    })
}

#[cfg(test)]
mod tests {
    use super::super::proto::encode::{bytes_field, varint_field};
    use super::*;
    use crate::harness::Harness;
    use crate::usage::{compute_snapshot, LocalUsageSnapshot, ScanLimits};
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
    const WORKSPACE: &str = "/tmp/llmon-agy-project";

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
        let dir = std::env::temp_dir().join(format!("llmon-agy-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn timestamp(seconds: i64) -> Vec<u8> {
        [
            varint_field(1, seconds as u64),
            varint_field(2, 123_000_000),
        ]
        .concat()
    }

    /// A usage message; `model` 0 leaves the enum out (checkpoints).
    fn usage_message(
        model: u64,
        input: u64,
        output: u64,
        cache_read: u64,
        response_id: &str,
    ) -> Vec<u8> {
        let mut fields = Vec::new();
        if model != 0 {
            fields.push(varint_field(1, model));
        }
        fields.push(varint_field(2, input));
        fields.push(varint_field(3, output));
        if cache_read != 0 {
            fields.push(varint_field(5, cache_read));
        }
        fields.push(varint_field(6, 24));
        fields.push(bytes_field(11, response_id.as_bytes()));
        fields.concat()
    }

    /// One `gen_metadata.data` row: the usage under field 1/4, with the call's
    /// durations and a long prompt text that must not matter.
    fn gen_row(usage: Vec<u8>) -> Vec<u8> {
        let chat = [
            varint_field(3, 1417),
            bytes_field(4, &usage),
            bytes_field(
                11,
                &[varint_field(1, 2), varint_field(2, 500_000_000)].concat(),
            ),
            bytes_field(12, &[varint_field(1, 7)].concat()),
            bytes_field(19, b"gemini-4-argon-a"),
        ]
        .concat();
        [bytes_field(1, &chat), bytes_field(2, &vec![b'x'; 5000])].concat()
    }

    /// `steps.metadata` of a step: its time and the generation it links to.
    fn step_metadata(seconds: i64, gen_idx: u64, checkpoint: Option<Vec<u8>>) -> Vec<u8> {
        let mut link = vec![bytes_field(1, b"step-uuid"), varint_field(2, 5)];
        if gen_idx != 0 {
            link.push(varint_field(3, gen_idx));
        }
        let mut fields = vec![
            bytes_field(1, &timestamp(seconds)),
            bytes_field(20, &link.concat()),
        ];
        if let Some(usage) = checkpoint {
            fields.push(bytes_field(28, &bytes_field(2, &usage)));
        }
        fields.concat()
    }

    fn make_conversation(dir: &Path, id: &str) -> Connection {
        let path = dir.join("conversations");
        std::fs::create_dir_all(&path).expect("conversations dir");
        let connection = Connection::open(path.join(format!("{id}.db"))).expect("create db");
        connection
            .execute_batch(
                "CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB, size INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER NOT NULL DEFAULT 0,
                                     status INTEGER NOT NULL DEFAULT 0, metadata BLOB);",
            )
            .expect("schema");
        let meta = [
            bytes_field(7, format!("file://{WORKSPACE}").as_bytes()),
            bytes_field(2, &timestamp(1_700_000_000)),
        ]
        .concat();
        connection
            .execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                [meta],
            )
            .expect("meta");
        connection
    }

    fn add_gen(connection: &Connection, idx: i64, row: &[u8]) {
        connection
            .execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
                rusqlite::params![idx, row, row.len() as i64],
            )
            .expect("gen");
    }

    fn add_step(connection: &Connection, idx: i64, kind: i64, metadata: &[u8]) {
        connection
            .execute(
                "INSERT INTO steps (idx, step_type, status, metadata) VALUES (?1, ?2, 3, ?3)",
                rusqlite::params![idx, kind, metadata],
            )
            .expect("step");
    }

    fn test_limits() -> ScanLimits {
        ScanLimits {
            max_session_file_bytes: 64 * 1024 * 1024,
            max_session_total_bytes: 256 * 1024 * 1024,
            max_session_files_scanned: 50,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        }
    }

    fn snapshot(agy_dir: &Path, cache: Option<&Path>) -> LocalUsageSnapshot {
        compute_snapshot(
            Harness::Antigravity,
            30,
            agy_dir,
            None,
            test_limits(),
            cache,
        )
        .expect("snapshot")
    }

    fn totals(snapshot: &LocalUsageSnapshot) -> TokenBreakdown {
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
    fn calls_and_checkpoints_are_counted_once_and_dated_by_their_steps() {
        let root = make_temp_dir("calls");
        let now = Utc::now().timestamp();
        let connection = make_conversation(&root, "conv-a");
        // Two calls (gen 0 and 1), each with its planner step; a checkpoint
        // call that only the step has; an aborted row with no tokens; and a
        // planner step that repeats the usage of its call.
        add_gen(
            &connection,
            0,
            &gen_row(usage_message(1417, 1_200, 350, 8_000, "resp-0")),
        );
        add_gen(
            &connection,
            1,
            &gen_row(usage_message(1417, 300, 40, 9_000, "resp-1")),
        );
        add_gen(&connection, 2, &gen_row(varint_field(1, 1417)));
        add_step(
            &connection,
            1,
            STEP_USER_INPUT,
            &step_metadata(now - 100, 0, None),
        );
        add_step(
            &connection,
            2,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now - 90, 0, None),
        );
        add_step(
            &connection,
            3,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now - 80, 1, None),
        );
        add_step(
            &connection,
            4,
            STEP_CHECKPOINT,
            &step_metadata(
                now - 70,
                2,
                Some(usage_message(0, 3_000, 200, 7_000, "ckpt-0")),
            ),
        );
        drop(connection);

        let snapshot = snapshot(&root, None);
        let total = totals(&snapshot);
        assert_eq!(total.input, 1_200 + 300 + 3_000);
        assert_eq!(total.cache_read, 8_000 + 9_000 + 7_000);
        assert_eq!(total.output, 350 + 40 + 200);
        // One model; the checkpoint has the model of the call before it.
        let models: Vec<&str> = snapshot
            .model_daily_for_zone(UsageZone::Utc)
            .iter()
            .map(|model| model.model.as_str())
            .collect();
        assert_eq!(models, vec!["gemini-4-argon"]);
        // The user's input is a run; the time is the calls' time to first
        // token and streaming time (two calls of 2.5 s and 7 s).
        let days = snapshot.days_for_zone(UsageZone::Utc);
        assert_eq!(days.iter().map(|day| day.agent_runs).sum::<i64>(), 1);
        assert_eq!(
            days.iter().map(|day| day.agent_time_ms).sum::<i64>(),
            19_000
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_workspace_is_the_project_and_a_repeated_response_counts_once() {
        let root = make_temp_dir("owner");
        let now = Utc::now().timestamp();
        let connection = make_conversation(&root, "conv-a");
        let row = gen_row(usage_message(1417, 100, 10, 0, "same-id"));
        add_gen(&connection, 0, &row);
        add_gen(&connection, 1, &row);
        add_step(
            &connection,
            1,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now, 0, None),
        );
        // (A Unix path has no drive, so it is not an absolute path on Windows.)
        assert_eq!(read_workspace(&connection), session_cwd_identity(WORKSPACE));
        drop(connection);
        assert_eq!(totals(&snapshot(&root, None)).input, 100);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_call_without_a_dated_step_takes_a_neighbour_or_the_creation_time() {
        let root = make_temp_dir("dates");
        let now = Utc::now().timestamp();
        let connection = make_conversation(&root, "conv-a");
        add_gen(
            &connection,
            0,
            &gen_row(usage_message(1417, 10, 1, 0, "r0")),
        );
        add_gen(
            &connection,
            1,
            &gen_row(usage_message(1417, 20, 2, 0, "r1")),
        );
        // Only call 0 has a step: call 1 takes its day.
        add_step(
            &connection,
            1,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now, 0, None),
        );
        drop(connection);
        let snapshot = snapshot(&root, None);
        assert_eq!(
            snapshot
                .days_for_zone(UsageZone::Utc)
                .iter()
                .filter(|d| d.total_tokens > 0)
                .count(),
            1
        );
        assert_eq!(totals(&snapshot).input, 30);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unreadable_rows_and_missing_tables_do_not_fail_the_file() {
        let root = make_temp_dir("tolerant");
        let now = Utc::now().timestamp();
        let connection = make_conversation(&root, "conv-a");
        add_gen(&connection, 0, &[0xff, 0xff, 0xff]);
        add_gen(
            &connection,
            1,
            &gen_row(usage_message(1417, 50, 5, 0, "ok")),
        );
        add_step(
            &connection,
            1,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now, 1, None),
        );
        add_step(&connection, 2, STEP_PLANNER_RESPONSE, &[0xff]);
        drop(connection);
        assert_eq!(totals(&snapshot(&root, None)).input, 50);

        // A database without the tables is read as empty.
        let bare = root.join("conversations").join("conv-bare.db");
        Connection::open(&bare)
            .expect("create")
            .execute_batch("CREATE TABLE other (x)")
            .expect("schema");
        assert_eq!(totals(&snapshot(&root, None)).input, 50);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_changed_conversation_is_read_again_and_its_wal_is_seen() {
        let root = make_temp_dir("wal");
        let now = Utc::now().timestamp();
        let cache = root.join("cache.db");
        let connection = make_conversation(&root, "conv-a");
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        connection
            .pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no autocheckpoint");
        add_gen(
            &connection,
            0,
            &gen_row(usage_message(1417, 100, 10, 0, "r0")),
        );
        add_step(
            &connection,
            1,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now, 0, None),
        );
        assert_eq!(totals(&snapshot(&root, Some(&cache))).input, 100);

        // New rows sit in the write-ahead log, with the main file untouched.
        add_gen(&connection, 1, &gen_row(usage_message(1417, 7, 1, 0, "r1")));
        add_step(
            &connection,
            2,
            STEP_PLANNER_RESPONSE,
            &step_metadata(now, 1, None),
        );
        assert_eq!(totals(&snapshot(&root, Some(&cache))).input, 107);
        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn models_are_named_by_enum_then_backend_string() {
        assert_eq!(model_name(1417, None), "gemini-4-argon");
        assert_eq!(
            model_name(1402, Some("claude-opus-5-5-high")),
            "claude-opus-5-5"
        );
        assert_eq!(model_name(9, Some("some-model-high")), "some-model");
        assert_eq!(
            model_name(9, Some("gemini-4-argon-trial")),
            "gemini-4-argon"
        );
        assert_eq!(model_name(9, None), "agy-model-9");
        assert_eq!(model_name(9, Some("")), "agy-model-9");
    }

    #[test]
    fn uri_paths_are_escaped() {
        assert_eq!(
            uri_path(Path::new("/a/b-c_d.e/f g?h#i%")),
            "/a/b-c_d.e/f%20g%3Fh%23i%25"
        );
        // A Windows path gets forward slashes and a slash before the drive.
        assert_eq!(
            uri_path(Path::new("C:\\Users\\a b\\x.db")),
            "/C:/Users/a%20b/x.db"
        );
    }
}
