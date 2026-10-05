pub(crate) mod archive;

use crate::harness::{Harness, ALL_HARNESSES};
use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::providers::{claude, codex};
use anyhow::{Context, Result};
use archive::{ArchivedUsage, UsageArchive, USAGE_ARCHIVE_DB_FILE_NAME};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc, Weekday};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant, SystemTime};

pub(crate) const MAX_ACTIVITY_GAP_MS: i64 = 2 * 60 * 1000;
const DEFAULT_MAX_SESSION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_FILES_SCANNED: usize = 10_000;
const DEFAULT_MAX_JSONL_LINE_BYTES: usize = 512 * 1024;
const DEFAULT_SCAN_TIME_BUDGET_MS: u64 = 1500;
const MAX_DISTINCT_MODELS: usize = 5_000;
// v14: each session has one immutable owner cwd. Derived cache rows from
// earlier schemas can contain mutable-context or git-collapsed attribution.
/// Layout of the scan cache tables (`cache_meta` key `layout_version`).
const SCAN_CACHE_DB_LAYOUT_VERSION: i64 = 1;
/// Meaning of the Codex rows in the scan cache (`cache_meta` key
/// `schema_version.codex`). Bump it whenever the Codex parser or the cached
/// aggregates change; only Codex rows are rebuilt.
const CODEX_CACHE_SCHEMA_VERSION: i64 = 2;
/// Meaning of the Claude Code rows in the scan cache (`cache_meta` key
/// `schema_version.claude`). 2: a response counts the largest usage of its
/// repeated lines, not the first.
const CLAUDE_CACHE_SCHEMA_VERSION: i64 = 2;
pub const DEFAULT_SCAN_CACHE_MAX_ENTRIES: usize = 50_000;
pub const SCAN_CACHE_DB_FILE_NAME: &str = "llmon.db";
pub const ACTIVITY_TIMELINE_WEEKS: usize = 54;
pub const ACTIVITY_TIMELINE_DAYS: usize = ACTIVITY_TIMELINE_WEEKS * 7;

#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    pub max_session_file_bytes: u64,
    pub max_session_total_bytes: u64,
    pub max_session_files_scanned: usize,
    pub max_jsonl_line_bytes: usize,
    pub scan_time_budget_ms: u64,
    pub full_scan: bool,
    pub scan_cache_max_entries: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_session_file_bytes: DEFAULT_MAX_SESSION_FILE_BYTES,
            max_session_total_bytes: DEFAULT_MAX_SESSION_TOTAL_BYTES,
            max_session_files_scanned: DEFAULT_MAX_SESSION_FILES_SCANNED,
            max_jsonl_line_bytes: DEFAULT_MAX_JSONL_LINE_BYTES,
            scan_time_budget_ms: DEFAULT_SCAN_TIME_BUDGET_MS,
            full_scan: false,
            scan_cache_max_entries: DEFAULT_SCAN_CACHE_MAX_ENTRIES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMetric {
    Tokens,
    Time,
    Runs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartRange {
    Day,
    Week,
    Month,
}

impl ChartRange {
    pub fn toggled(self) -> Self {
        match self {
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Day,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageZone {
    Local,
    Utc,
}

impl UsageZone {
    pub fn toggled(self) -> Self {
        match self {
            Self::Local => Self::Utc,
            Self::Utc => Self::Local,
        }
    }
}

pub fn format_compact_kmb(value: u64, max_width: u16, formatter: DisplayFormatter<'_>) -> String {
    // Examples (depending on width):
    //  1234 -> 1.23K / 1.2K / 1K
    //  12_345_678 -> 12.35M / 12.3M / 12M
    //  999 -> 999
    if max_width == 0 {
        return String::new();
    }
    if value < 1000 {
        let s = formatter.format_u64(value);
        return if s.len() <= max_width as usize {
            s
        } else {
            // Worst-case truncate from the left.
            s[s.len().saturating_sub(max_width as usize)..].to_string()
        };
    }

    let (div, suffix) = if value >= 1_000_000_000_000 {
        (1_000_000_000_000f64, "T")
    } else if value >= 1_000_000_000 {
        (1_000_000_000f64, "B")
    } else if value >= 1_000_000 {
        (1_000_000f64, "M")
    } else {
        (1_000f64, "K")
    };
    let scaled = (value as f64) / div;

    // In dense layouts, force integer suffixes (e.g. 27M instead of 27.4M).
    // Heuristic: if the label width is <= 5 cells, decimals tend to hurt readability.
    if max_width <= 5 {
        let s = format_compact_scaled(scaled, suffix, 0, formatter);
        return if s.len() <= max_width as usize {
            s
        } else {
            // truncate
            s[..max_width as usize].to_string()
        };
    }

    // Prefer 2 decimals, then reduce precision if it doesn't fit.
    for decimals in [2usize, 1usize, 0usize] {
        let s = format_compact_scaled(scaled, suffix, decimals, formatter);
        if s.len() <= max_width as usize {
            return s;
        }
    }

    // Final fallback: "1K/M/B/T"
    let s = formatter.localize_decimal(&format!("{:.0}{suffix}", scaled.max(0.0).round()));
    if s.len() <= max_width as usize {
        return s;
    }

    // Last resort: truncate.
    s[..max_width as usize].to_string()
}

fn format_compact_scaled(
    value: f64,
    suffix: &str,
    decimals: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    // Format with fixed decimals, then trim trailing zeros and a trailing dot.
    let mut s = format!("{:.*}", decimals, value.max(0.0));
    if decimals > 0 {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s.push_str(suffix);
    formatter.localize_decimal(&s)
}

/// Token usage split into the categories every harness reports. `input` is
/// uncached prompt input only, so the four fields add up to the total.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBreakdown {
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub cache_write: i64,
    /// Part of `cache_write` written with the one-hour TTL, which is billed
    /// higher than five-minute writes. Not part of the total on its own.
    #[serde(default)]
    pub cache_write_1h: i64,
    #[serde(default)]
    pub cache_read: i64,
    #[serde(default)]
    pub output: i64,
}

impl TokenBreakdown {
    pub fn total(self) -> i64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }

    pub fn add(&mut self, other: TokenBreakdown) {
        self.input = self.input.saturating_add(other.input);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.cache_write_1h = self.cache_write_1h.saturating_add(other.cache_write_1h);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.output = self.output.saturating_add(other.output);
    }

    /// The amount by which each field of `other` exceeds this one (zero where
    /// it does not).
    pub fn excess_of(self, other: TokenBreakdown) -> TokenBreakdown {
        let excess = |mine: i64, theirs: i64| theirs.saturating_sub(mine).max(0);
        TokenBreakdown {
            input: excess(self.input, other.input),
            cache_write: excess(self.cache_write, other.cache_write),
            cache_write_1h: excess(self.cache_write_1h, other.cache_write_1h),
            cache_read: excess(self.cache_read, other.cache_read),
            output: excess(self.output, other.output),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UsageDay {
    pub day: String,
    /// Uncached input tokens.
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    /// Sum of the four token categories.
    pub total_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
}

impl UsageDay {
    fn from_totals(day: String, totals: DailyTotals) -> Self {
        Self {
            day,
            input_tokens: totals.tokens.input,
            cache_write_tokens: totals.tokens.cache_write,
            cache_read_tokens: totals.tokens.cache_read,
            output_tokens: totals.tokens.output,
            total_tokens: totals.tokens.total(),
            agent_time_ms: totals.agent_ms,
            agent_runs: totals.agent_runs,
        }
    }

    pub fn short_label(&self, formatter: DisplayFormatter<'_>) -> String {
        format_day_short(&self.day, formatter)
    }

    /// All prompt tokens: uncached input plus cache writes and reads.
    pub fn prompt_tokens(&self) -> i64 {
        self.input_tokens
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.cache_read_tokens)
    }
}

fn format_day_short(day: &str, formatter: DisplayFormatter<'_>) -> String {
    // Expect YYYY-MM-DD
    if day.len() == 10 {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
            return formatter.format_short_date(date);
        }
    }
    day.to_string()
}

#[derive(Debug, Clone)]
pub struct UsageTotalsTokens {
    pub last7_days_tokens: i64,
    pub last30_days_tokens: i64,
    pub average_daily_tokens: i64,
    pub cache_hit_rate_percent: f64,
    pub peak_day: Option<String>,
    pub peak_day_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct LocalUsageModel {
    pub model: String,
    pub tokens: i64,
    pub share_percent: f64,
}

#[derive(Debug, Clone)]
pub struct ProjectActivity {
    pub display_path: String,
    pub days: Vec<UsageDay>,
    pub last_activity_day: Option<String>,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
}

#[derive(Debug, Clone)]
pub struct ProjectUsageSummary {
    pub display_path: String,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
    pub indexed_files: usize,
}

#[derive(Debug, Clone)]
pub struct LocalUsageSnapshot {
    pub days: Vec<UsageDay>,
    pub totals: UsageTotalsTokens,
    pub top_models: Vec<LocalUsageModel>,
    pub utc_days: Vec<UsageDay>,
    pub utc_totals: UsageTotalsTokens,
    pub utc_top_models: Vec<LocalUsageModel>,
    pub activity_first_weekday: Weekday,
    pub project_activity: Vec<ProjectActivity>,
    pub project_usage: Vec<ProjectUsageSummary>,
    // Number of session files that were identified as belonging to the selected workspace filter.
    // When no workspace filter is used, this is 0.
    pub matched_session_files: u32,
    pub scan_total_files: usize,
    pub scan_indexed_files: usize,
    pub scan_pending_files: usize,
    pub scan_processed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct UsageTotalsView {
    pub last7_primary_label: String,
    pub last30_primary_label: String,
    pub avg_primary_label: String,
    pub cache_label: String,
    pub total_label: String,
    pub runs_label: String,
    pub peak_day_label: String,
    pub peak_sub_label: String,
}

impl LocalUsageSnapshot {
    pub fn project_usage_for_path(&self, path: &str) -> Option<&ProjectUsageSummary> {
        let key = normalize_project_key(path);
        self.project_usage
            .iter()
            .find(|project| normalize_project_key(&project.display_path) == key)
    }

    pub fn days_for_zone(&self, zone: UsageZone) -> &[UsageDay] {
        match zone {
            UsageZone::Local => &self.days,
            UsageZone::Utc => &self.utc_days,
        }
    }

    pub fn totals_for_zone(&self, zone: UsageZone) -> &UsageTotalsTokens {
        match zone {
            UsageZone::Local => &self.totals,
            UsageZone::Utc => &self.utc_totals,
        }
    }

    pub fn top_models_for_zone(&self, zone: UsageZone) -> &[LocalUsageModel] {
        match zone {
            UsageZone::Local => &self.top_models,
            UsageZone::Utc => &self.utc_top_models,
        }
    }

    pub fn last7_days_for_zone(&self, zone: UsageZone) -> Vec<UsageDay> {
        self.last_n_days_for_zone(zone, 7)
    }

    pub fn last_n_days_for_zone(&self, zone: UsageZone, n: usize) -> Vec<UsageDay> {
        self.days_for_zone(zone)
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn totals_view_for_zone(
        &self,
        metric: UsageMetric,
        formatter: DisplayFormatter<'_>,
        zone: UsageZone,
    ) -> UsageTotalsView {
        let totals = self.totals_for_zone(zone);
        let last7 = self.last7_days_for_zone(zone);
        let last30 = self.last_n_days_for_zone(zone, 30);
        let last7_agent_ms: i64 = last7.iter().map(|d| d.agent_time_ms).sum();
        let last30_agent_ms: i64 = last30.iter().map(|d| d.agent_time_ms).sum();
        let last7_runs: i64 = last7.iter().map(|d| d.agent_runs).sum();
        let last30_runs: i64 = last30.iter().map(|d| d.agent_runs).sum();

        let (peak_day, _peak_value, peak_sub) = match metric {
            UsageMetric::Tokens => {
                let peak = last30
                    .iter()
                    .max_by_key(|d| d.total_tokens)
                    .filter(|d| d.total_tokens > 0);
                let peak_day = peak
                    .map(|d| d.short_label(formatter))
                    .unwrap_or_else(|| "--".to_string());
                let peak_tokens = peak.map(|d| d.total_tokens).unwrap_or(0);
                let sub = format!("{} tokens", format_tokens_compact(peak_tokens, formatter));
                (peak_day, peak_tokens, sub)
            }
            UsageMetric::Time => {
                let peak = last30
                    .iter()
                    .max_by_key(|d| d.agent_time_ms)
                    .filter(|d| d.agent_time_ms > 0);
                let peak_day = peak
                    .map(|d| d.short_label(formatter))
                    .unwrap_or_else(|| "--".to_string());
                let peak_ms = peak.map(|d| d.agent_time_ms).unwrap_or(0);
                let sub = format!("{} agent time", format_duration_compact(peak_ms));
                (peak_day, peak_ms, sub)
            }
            UsageMetric::Runs => {
                let peak = last30
                    .iter()
                    .max_by_key(|d| d.agent_runs)
                    .filter(|d| d.agent_runs > 0);
                let peak_day = peak
                    .map(|d| d.short_label(formatter))
                    .unwrap_or_else(|| "--".to_string());
                let peak_runs = peak.map(|d| d.agent_runs).unwrap_or(0);
                let sub = format!("{} runs", format_count(peak_runs, formatter));
                (peak_day, peak_runs, sub)
            }
        };

        match metric {
            UsageMetric::Tokens => UsageTotalsView {
                last7_primary_label: format!(
                    "{} tokens",
                    format_tokens_compact(totals.last7_days_tokens, formatter)
                ),
                last30_primary_label: format!(
                    "{} tokens",
                    format_tokens_compact(totals.last30_days_tokens, formatter)
                ),
                avg_primary_label: format_tokens_compact(totals.average_daily_tokens, formatter),
                cache_label: format!(
                    "{}%",
                    formatter.format_one_decimal(totals.cache_hit_rate_percent)
                ),
                total_label: format_tokens_overview(totals.last30_days_tokens, formatter),
                runs_label: format!("{} runs", format_count(last7_runs, formatter)),
                peak_day_label: self
                    .totals_for_zone(zone)
                    .peak_day
                    .as_deref()
                    .map(|day| format_day_short(day, formatter))
                    .unwrap_or(peak_day),
                peak_sub_label: format!(
                    "{} tokens",
                    format_tokens_compact(totals.peak_day_tokens, formatter)
                ),
            },
            UsageMetric::Time => {
                let avg_ms = if last7.is_empty() {
                    0
                } else {
                    (last7_agent_ms as f64 / last7.len() as f64).round() as i64
                };
                UsageTotalsView {
                    last7_primary_label: format_duration_compact(last7_agent_ms),
                    last30_primary_label: format_duration_compact(last30_agent_ms),
                    avg_primary_label: format_duration_compact(avg_ms),
                    cache_label: "--".to_string(),
                    total_label: format_duration(last30_agent_ms),
                    runs_label: format!("{} runs", format_count(last7_runs, formatter)),
                    peak_day_label: peak_day,
                    peak_sub_label: peak_sub,
                }
            }
            UsageMetric::Runs => {
                let avg_7 = if last7.is_empty() {
                    0
                } else {
                    (last7_runs as f64 / last7.len() as f64).round() as i64
                };
                UsageTotalsView {
                    last7_primary_label: format!("{} runs", format_count(last7_runs, formatter)),
                    last30_primary_label: format!("{} runs", format_count(last30_runs, formatter)),
                    avg_primary_label: format_count(avg_7, formatter),
                    cache_label: "--".to_string(),
                    total_label: format_count(last30_runs, formatter),
                    runs_label: format!("{} runs", format_count(last7_runs, formatter)),
                    peak_day_label: peak_day,
                    peak_sub_label: peak_sub,
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct DailyTotals {
    #[serde(default)]
    pub(crate) tokens: TokenBreakdown,
    #[serde(default)]
    pub(crate) agent_ms: i64,
    #[serde(default)]
    pub(crate) agent_runs: i64,
}

impl DailyTotals {
    fn add(&mut self, other: DailyTotals) {
        self.tokens.add(other.tokens);
        self.agent_ms = self.agent_ms.saturating_add(other.agent_ms);
        self.agent_runs = self.agent_runs.saturating_add(other.agent_runs);
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScanCacheStore {
    pub(crate) entries: HashMap<String, CachedFileScanEntry>,
}

/// Incremental parser state of one cached file. Each harness owns its
/// state; the shared scanner only stores and returns it.
#[derive(Debug, Clone)]
pub(crate) enum HarnessParserState {
    Codex(codex::usage::ParserState),
    Claude(claude::usage::ParserState),
}

impl HarnessParserState {
    pub(crate) fn as_codex(&self) -> Option<&codex::usage::ParserState> {
        match self {
            HarnessParserState::Codex(state) => Some(state),
            HarnessParserState::Claude(_) => None,
        }
    }

    pub(crate) fn as_claude(&self) -> Option<&claude::usage::ParserState> {
        match self {
            HarnessParserState::Claude(state) => Some(state),
            HarnessParserState::Codex(_) => None,
        }
    }

    fn from_json(harness: Harness, json: &str) -> serde_json::Result<Self> {
        match harness {
            Harness::Codex => serde_json::from_str(json).map(HarnessParserState::Codex),
            Harness::Claude => serde_json::from_str(json).map(HarnessParserState::Claude),
        }
    }

    fn to_json(&self) -> serde_json::Result<String> {
        match self {
            HarnessParserState::Codex(state) => serde_json::to_string(state),
            HarnessParserState::Claude(state) => serde_json::to_string(state),
        }
    }
}

/// Inputs a harness prepares once per refresh before parsing files.
enum HarnessParsePlan {
    /// Parent baselines of forked Codex sessions, by candidate path.
    Codex(HashMap<String, codex::usage::ForkResolution>),
    Claude,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedFileScanEntry {
    pub(crate) size: u64,
    pub(crate) modified_epoch_secs: Option<u64>,
    pub(crate) file_offset: u64,
    pub(crate) fully_parsed: bool,
    pub(crate) session_cwd: Option<String>,
    pub(crate) parser_state: HarnessParserState,
    pub(crate) daily: HashMap<String, DailyTotals>,
    pub(crate) model_totals_by_day: HashMap<String, HashMap<String, TokenBreakdown>>,
    pub(crate) updated_at: i64,
}

impl CachedFileScanEntry {
    fn usage(&self) -> FileUsageRef<'_> {
        FileUsageRef {
            session_cwd: self.session_cwd.as_deref(),
            daily: &self.daily,
            model_totals_by_day: &self.model_totals_by_day,
        }
    }
}

/// The aggregates one log file contributes, from the scan cache or from the
/// usage archive once the file is gone.
#[derive(Clone, Copy)]
pub(crate) struct FileUsageRef<'a> {
    pub(crate) session_cwd: Option<&'a str>,
    pub(crate) daily: &'a HashMap<String, DailyTotals>,
    pub(crate) model_totals_by_day: &'a HashMap<String, HashMap<String, TokenBreakdown>>,
}

/// Result of parsing one log file, possibly resumed from a cached entry.
#[derive(Debug, Clone)]
pub(crate) struct FileScanSummary {
    pub(crate) session_cwd: Option<String>,
    pub(crate) parser_state: HarnessParserState,
    pub(crate) file_offset: u64,
    pub(crate) fully_parsed: bool,
    pub(crate) daily: HashMap<String, DailyTotals>,
    pub(crate) model_totals_by_day: HashMap<String, HashMap<String, TokenBreakdown>>,
    /// The file cannot be parsed yet (for example a Codex fork whose parent
    /// baseline is unknown). Its row restarts from offset 0 next refresh.
    pub(crate) deferred: bool,
}

impl FileScanSummary {
    pub(crate) fn empty(parser_state: HarnessParserState) -> Self {
        Self {
            session_cwd: None,
            parser_state,
            file_offset: 0,
            fully_parsed: false,
            daily: HashMap::new(),
            model_totals_by_day: HashMap::new(),
            deferred: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ProjectActivityBuilder {
    display_path: String,
    daily: HashMap<String, DailyTotals>,
}

#[derive(Debug, Default)]
struct ProjectUsageBuilder {
    display_path: String,
    total_tokens: i64,
    cache_read_tokens: i64,
    agent_time_ms: i64,
    agent_runs: i64,
    indexed_files: usize,
}

#[derive(Debug)]
struct ScanCacheDb {
    path: PathBuf,
    conn: Connection,
}

/// All snapshot aggregates as JSON, for `--dump-usage`. The field set is a
/// regression contract: refactors must keep the output identical for the
/// same logs.
pub fn snapshot_dump_json(snapshot: &LocalUsageSnapshot) -> Value {
    fn day_json(day: &UsageDay) -> Value {
        serde_json::json!({
            "day": day.day,
            "input": day.prompt_tokens(),
            "cached": day.cache_read_tokens,
            "total": day.total_tokens,
            "agent_ms": day.agent_time_ms,
            "runs": day.agent_runs,
        })
    }
    fn totals_json(totals: &UsageTotalsTokens) -> Value {
        serde_json::json!({
            "last7": totals.last7_days_tokens,
            "last30": totals.last30_days_tokens,
            "average_daily": totals.average_daily_tokens,
            "cache_hit_rate_percent": totals.cache_hit_rate_percent,
            "peak_day": totals.peak_day,
            "peak_day_tokens": totals.peak_day_tokens,
        })
    }
    fn models_json(models: &[LocalUsageModel]) -> Value {
        models
            .iter()
            .map(|model| {
                serde_json::json!({
                    "model": model.model,
                    "tokens": model.tokens,
                    "share_percent": model.share_percent,
                })
            })
            .collect()
    }
    let active_days = |days: &[UsageDay]| -> Value {
        days.iter()
            .filter(|day| day.total_tokens != 0 || day.agent_time_ms != 0 || day.agent_runs != 0)
            .map(day_json)
            .collect()
    };
    serde_json::json!({
        "days": active_days(&snapshot.days),
        "day_count": snapshot.days.len(),
        "totals": totals_json(&snapshot.totals),
        "top_models": models_json(&snapshot.top_models),
        "utc_days": active_days(&snapshot.utc_days),
        "utc_day_count": snapshot.utc_days.len(),
        "utc_totals": totals_json(&snapshot.utc_totals),
        "utc_top_models": models_json(&snapshot.utc_top_models),
        "project_activity": snapshot.project_activity.iter().map(|project| serde_json::json!({
            "path": project.display_path,
            "days": active_days(&project.days),
            "last_activity_day": project.last_activity_day,
            "total": project.total_tokens,
            "cached": project.cache_read_tokens,
            "agent_ms": project.agent_time_ms,
            "runs": project.agent_runs,
        })).collect::<Vec<_>>(),
        "project_usage": snapshot.project_usage.iter().map(|project| serde_json::json!({
            "path": project.display_path,
            "total": project.total_tokens,
            "cached": project.cache_read_tokens,
            "agent_ms": project.agent_time_ms,
            "runs": project.agent_runs,
            "indexed_files": project.indexed_files,
        })).collect::<Vec<_>>(),
        "matched_session_files": snapshot.matched_session_files,
        "scan_total_files": snapshot.scan_total_files,
        "scan_indexed_files": snapshot.scan_indexed_files,
        "scan_pending_files": snapshot.scan_pending_files,
        "scan_processed_bytes": snapshot.scan_processed_bytes,
    })
}

pub fn compute_snapshot(
    harness: Harness,
    days: u32,
    harness_home: &Path,
    workspace_path: Option<&Path>,
    limits: ScanLimits,
    scan_cache_db_path: Option<&Path>,
) -> Result<LocalUsageSnapshot> {
    let days = days.clamp(1, 90);

    let sessions_root = match harness {
        Harness::Codex => codex::sessions_root(harness_home),
        Harness::Claude => claude::projects_root(harness_home),
    };
    // The configured window controls summary cards and model shares. Charts are
    // expanded to the complete indexed history after cached rows are applied.
    let summary_day_keys = make_day_keys_for_zone(days, UsageZone::Local);
    let utc_summary_day_keys = make_day_keys_for_zone(days, UsageZone::Utc);
    let summary_day_filter: HashSet<String> = summary_day_keys.iter().cloned().collect();
    let utc_summary_day_filter: HashSet<String> = utc_summary_day_keys.iter().cloned().collect();
    let activity_first_weekday = system_first_weekday();
    let activity_day_keys = make_activity_day_keys(activity_first_weekday);
    let mut scan_day_keys = activity_day_keys.clone();
    for day_key in &summary_day_keys {
        if !scan_day_keys.contains(day_key) {
            scan_day_keys.push(day_key.clone());
        }
    }
    let mut daily: HashMap<String, DailyTotals> = scan_day_keys
        .iter()
        .map(|key| (key.clone(), DailyTotals::default()))
        .collect();
    let mut utc_daily: HashMap<String, DailyTotals> = utc_summary_day_keys
        .iter()
        .map(|key| (key.clone(), DailyTotals::default()))
        .collect();
    let mut model_totals: HashMap<String, TokenBreakdown> = HashMap::new();
    let mut utc_model_totals: HashMap<String, TokenBreakdown> = HashMap::new();
    let mut project_activity: HashMap<String, ProjectActivityBuilder> = HashMap::new();

    if !sessions_root.exists() {
        return Ok(build_snapshot(
            summary_day_keys,
            daily,
            model_totals,
            utc_summary_day_keys,
            utc_daily,
            utc_model_totals,
            0,
            activity_first_weekday,
            activity_day_keys,
            project_activity,
            Vec::new(),
            0,
            0,
            0,
            0,
        ));
    }

    // Build a full candidate list (metadata-only).
    let mut candidates = collect_session_file_candidates(&sessions_root);
    // Newest activity first.
    candidates.sort_by(|a, b| {
        b.modified_epoch_secs
            .cmp(&a.modified_epoch_secs)
            .then_with(|| a.len.cmp(&b.len))
    });

    let mut matched_session_files: u32 = 0;
    let mut scan_cache_db = scan_cache_db_path
        .map(open_or_init_scan_cache_db)
        .transpose()?;
    let candidate_paths: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.path.to_string_lossy().to_string())
        .collect();
    let (mut scan_cache_store, mut removed_cache_paths) = if let Some(db) = scan_cache_db.as_ref() {
        load_scan_cache_store(db, harness)?
    } else {
        (ScanCacheStore::default(), HashSet::new())
    };

    // Rows of logs that were deleted or moved away keep counting from the
    // usage archive. Rows of files that still exist but are no longer valid
    // candidates are dropped. Rows outside this sessions root (another
    // harness home) are kept but not counted.
    let mut archived_usage: Vec<ArchivedUsage> = Vec::new();
    if let Some(db) = scan_cache_db.as_ref() {
        let valid_paths: HashSet<&str> =
            candidate_paths.iter().map(|value| value.as_str()).collect();
        let mut deleted_paths: Vec<String> = Vec::new();
        scan_cache_store.entries.retain(|file_path, _| {
            if valid_paths.contains(file_path.as_str()) {
                return true;
            }
            let path = Path::new(file_path);
            if !path.starts_with(&sessions_root) {
                return true;
            }
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    deleted_paths.push(file_path.clone());
                    true
                }
                _ => {
                    removed_cache_paths.insert(file_path.clone());
                    false
                }
            }
        });
        deleted_paths.sort();
        // The archive is best effort: on failure the cache rows stay where
        // they are and are archived on a later refresh.
        if let Ok(archive) = UsageArchive::open(&db.path.with_file_name(USAGE_ARCHIVE_DB_FILE_NAME))
        {
            archived_usage = sync_usage_archive(
                archive,
                harness,
                &sessions_root,
                &valid_paths,
                &deleted_paths,
                &mut scan_cache_store,
                &mut removed_cache_paths,
            );
        }
    }

    let force_reparse_all = limits.full_scan && limits.scan_time_budget_ms == 0;
    // Index every local session eventually. The normal scan budgets keep each
    // refresh bounded; `full_scan` still processes the whole backlog at once.
    let eligible_indices: Vec<usize> = (0..candidates.len()).collect();
    let mut work_indices: Vec<usize> = eligible_indices
        .iter()
        .copied()
        .filter(|idx| {
            if force_reparse_all {
                return true;
            }
            let key = &candidate_paths[*idx];
            scan_cache_store
                .entries
                .get(key)
                .is_none_or(|entry| !cache_entry_matches_candidate(entry, &candidates[*idx]))
        })
        .collect();

    // Changed or partially parsed files are latency-sensitive. Uncached files form a
    // deterministic backlog: once one is cached it drops out of this list, so later
    // refreshes naturally advance instead of rescanning the same newest files forever.
    work_indices.sort_by_key(|idx| {
        let key = &candidate_paths[*idx];
        match scan_cache_store.entries.get(key) {
            // Appended live sessions first, then brand-new sessions. Large or
            // unresolved partial files rotate only after current usage has had
            // a chance to enter the cache.
            Some(entry) if entry.fully_parsed => (0_u8, entry.updated_at),
            None => (1_u8, i64::MIN),
            Some(entry) => (2_u8, entry.updated_at),
        }
    });

    let mut planned_indices: Vec<usize> = Vec::new();
    let mut planned_total_bytes: u64 = 0;
    for idx in work_indices {
        if limits.full_scan {
            planned_indices.push(idx);
            continue;
        }
        if planned_indices.len() >= limits.max_session_files_scanned
            || planned_total_bytes >= limits.max_session_total_bytes
        {
            break;
        }
        let candidate = &candidates[idx];
        let already_read = scan_cache_store
            .entries
            .get(&candidate_paths[idx])
            .map(|entry| entry.file_offset.min(candidate.len))
            .unwrap_or(0);
        let remaining = candidate.len.saturating_sub(already_read).max(1);
        let candidate_weight = remaining.min(limits.max_session_file_bytes.max(1));
        if !planned_indices.is_empty()
            && planned_total_bytes.saturating_add(candidate_weight) > limits.max_session_total_bytes
        {
            continue;
        }
        planned_indices.push(idx);
        planned_total_bytes = planned_total_bytes.saturating_add(candidate_weight);
    }
    let planned_set: HashSet<usize> = planned_indices.iter().copied().collect();
    let parse_plan = match harness {
        Harness::Codex => HarnessParsePlan::Codex(codex::usage::resolve_fork_baselines(
            &candidates,
            &candidate_paths,
            &planned_indices,
            &scan_cache_store,
            &harness_home.join("archived_sessions"),
            limits.max_jsonl_line_bytes,
        )),
        Harness::Claude => HarnessParsePlan::Claude,
    };
    // Parent baseline discovery is bounded by the planned fork set and runs in
    // the background usage worker. Start the incremental file-parse budget only
    // after it, otherwise a large parent can consume every refresh before even
    // one cache row advances.
    let scan_deadline = if limits.scan_time_budget_ms == 0 {
        None
    } else {
        Instant::now().checked_add(StdDuration::from_millis(limits.scan_time_budget_ms))
    };
    let mut dirty_cache_paths: HashSet<String> = HashSet::new();
    let mut uncached_indexed_files = 0usize;

    if scan_cache_db.is_some() {
        for (idx, candidate) in candidates.iter().enumerate() {
            let candidate_key = &candidate_paths[idx];
            let cached_entry = scan_cache_store.entries.get(candidate_key).cloned();
            let cached_entry_matches = if force_reparse_all {
                None
            } else {
                cached_entry
                    .as_ref()
                    .filter(|entry| cache_entry_matches_candidate(entry, candidate))
                    .cloned()
            };

            if planned_set.contains(&idx) {
                let entry = if let Some(entry) = cached_entry_matches {
                    entry
                } else {
                    if let Some(deadline) = scan_deadline {
                        if Instant::now() >= deadline {
                            if let Some(stale) = cached_entry {
                                apply_file_usage(
                                    stale.usage(),
                                    workspace_path,
                                    &mut daily,
                                    &mut model_totals,
                                    &summary_day_filter,
                                    &mut utc_daily,
                                    &mut utc_model_totals,
                                    &utc_summary_day_filter,
                                    &mut project_activity,
                                    &mut matched_session_files,
                                );
                            }
                            continue;
                        }
                    }

                    let parsed = match parse_candidate(
                        &parse_plan,
                        &candidate.path,
                        candidate_key,
                        limits.max_jsonl_line_bytes,
                        cached_entry.as_ref(),
                        scan_deadline,
                    ) {
                        Ok(parsed) => parsed,
                        Err(_) => {
                            if let Some(stale) = cached_entry {
                                apply_file_usage(
                                    stale.usage(),
                                    workspace_path,
                                    &mut daily,
                                    &mut model_totals,
                                    &summary_day_filter,
                                    &mut utc_daily,
                                    &mut utc_model_totals,
                                    &utc_summary_day_filter,
                                    &mut project_activity,
                                    &mut matched_session_files,
                                );
                            }
                            continue;
                        }
                    };
                    let entry = CachedFileScanEntry {
                        size: candidate.len,
                        modified_epoch_secs: candidate.modified_epoch_secs,
                        file_offset: if parsed.deferred {
                            0
                        } else {
                            parsed.file_offset.min(candidate.len)
                        },
                        fully_parsed: parsed.fully_parsed && !parsed.deferred,
                        session_cwd: parsed.session_cwd,
                        parser_state: parsed.parser_state,
                        daily: parsed.daily,
                        model_totals_by_day: parsed.model_totals_by_day,
                        updated_at: unix_time_seconds(),
                    };
                    scan_cache_store
                        .entries
                        .insert(candidate_key.clone(), entry.clone());
                    dirty_cache_paths.insert(candidate_key.clone());
                    entry
                };
                apply_file_usage(
                    entry.usage(),
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &summary_day_filter,
                    &mut utc_daily,
                    &mut utc_model_totals,
                    &utc_summary_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
                continue;
            }

            if let Some(entry) = cached_entry_matches {
                apply_file_usage(
                    entry.usage(),
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &summary_day_filter,
                    &mut utc_daily,
                    &mut utc_model_totals,
                    &utc_summary_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
                continue;
            }

            if let Some(stale) = cached_entry {
                apply_file_usage(
                    stale.usage(),
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &summary_day_filter,
                    &mut utc_daily,
                    &mut utc_model_totals,
                    &utc_summary_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
            }
        }
    } else {
        for idx in planned_indices {
            let candidate = &candidates[idx];
            let parsed = parse_candidate(
                &parse_plan,
                &candidate.path,
                &candidate_paths[idx],
                limits.max_jsonl_line_bytes,
                None,
                None,
            )?;
            let entry = CachedFileScanEntry {
                size: candidate.len,
                modified_epoch_secs: candidate.modified_epoch_secs,
                file_offset: if parsed.deferred {
                    0
                } else {
                    parsed.file_offset.min(candidate.len)
                },
                fully_parsed: parsed.fully_parsed && !parsed.deferred,
                session_cwd: parsed.session_cwd,
                parser_state: parsed.parser_state,
                daily: parsed.daily,
                model_totals_by_day: parsed.model_totals_by_day,
                updated_at: unix_time_seconds(),
            };
            if cache_entry_matches_candidate(&entry, candidate) {
                uncached_indexed_files = uncached_indexed_files.saturating_add(1);
            }
            apply_file_usage(
                entry.usage(),
                workspace_path,
                &mut daily,
                &mut model_totals,
                &summary_day_filter,
                &mut utc_daily,
                &mut utc_model_totals,
                &utc_summary_day_filter,
                &mut project_activity,
                &mut matched_session_files,
            );
            scan_cache_store
                .entries
                .insert(candidate_paths[idx].clone(), entry);
        }
    }

    if let Some(scan_cache_db) = scan_cache_db.as_mut() {
        if !dirty_cache_paths.is_empty() || !removed_cache_paths.is_empty() {
            let _ = persist_scan_cache_changes(
                scan_cache_db,
                harness,
                &scan_cache_store,
                &removed_cache_paths,
                &dirty_cache_paths,
            );
        }
        let _ = trim_scan_cache_db_entries_to_limit(
            scan_cache_db,
            harness,
            limits.scan_cache_max_entries.max(1),
        );
    }

    let scan_total_files = eligible_indices.len();
    let scan_indexed_files = if scan_cache_db.is_some() {
        eligible_indices
            .iter()
            .filter(|idx| {
                scan_cache_store
                    .entries
                    .get(&candidate_paths[**idx])
                    .is_some_and(|entry| cache_entry_matches_candidate(entry, &candidates[**idx]))
            })
            .count()
    } else {
        uncached_indexed_files
    };
    let scan_pending_files = scan_total_files.saturating_sub(scan_indexed_files);
    let scan_processed_bytes = eligible_indices
        .iter()
        .filter_map(|idx| {
            scan_cache_store
                .entries
                .get(&candidate_paths[*idx])
                .map(|entry| entry.file_offset.min(candidates[*idx].len))
        })
        .fold(0_u64, u64::saturating_add);

    for archived in &archived_usage {
        apply_file_usage(
            archived.usage(),
            workspace_path,
            &mut daily,
            &mut model_totals,
            &summary_day_filter,
            &mut utc_daily,
            &mut utc_model_totals,
            &utc_summary_day_filter,
            &mut project_activity,
            &mut matched_session_files,
        );
    }

    let chart_day_keys = make_complete_chart_day_keys(&daily, UsageZone::Local, &summary_day_keys);
    let utc_chart_day_keys =
        make_complete_chart_day_keys(&utc_daily, UsageZone::Utc, &utc_summary_day_keys);
    let project_usage = build_project_usage_summaries(
        &candidates,
        &candidate_paths,
        &archived_usage,
        &scan_cache_store,
    );

    Ok(build_snapshot(
        chart_day_keys,
        daily,
        model_totals,
        utc_chart_day_keys,
        utc_daily,
        utc_model_totals,
        matched_session_files,
        activity_first_weekday,
        activity_day_keys,
        project_activity,
        project_usage,
        scan_total_files,
        scan_indexed_files,
        scan_pending_files,
        scan_processed_bytes,
    ))
}

/// Moves cache rows of deleted logs into the archive, drops archive rows of
/// logs that exist again, and returns the archived usage to count.
fn sync_usage_archive(
    mut archive: UsageArchive,
    harness: Harness,
    sessions_root: &Path,
    candidate_paths: &HashSet<&str>,
    deleted_paths: &[String],
    cache: &mut ScanCacheStore,
    removed_cache_paths: &mut HashSet<String>,
) -> Vec<ArchivedUsage> {
    let deleted_rows: Vec<(&str, &CachedFileScanEntry)> = deleted_paths
        .iter()
        .filter_map(|path| cache.entries.get(path).map(|entry| (path.as_str(), entry)))
        .collect();
    if archive
        .archive(harness, &deleted_rows, unix_time_seconds())
        .is_ok()
    {
        for path in deleted_paths {
            cache.entries.remove(path);
            removed_cache_paths.insert(path.clone());
        }
    }

    let Ok(rows) = archive.load(harness) else {
        return Vec::new();
    };
    let restored: Vec<&str> = rows
        .iter()
        .filter(|row| candidate_paths.contains(row.file_path.as_str()))
        .map(|row| row.file_path.as_str())
        .collect();
    let _ = archive.forget(harness, &restored);
    rows.into_iter()
        .filter(|row| {
            Path::new(&row.file_path).starts_with(sessions_root)
                && !candidate_paths.contains(row.file_path.as_str())
        })
        .collect()
}

fn parse_candidate(
    plan: &HarnessParsePlan,
    path: &Path,
    key: &str,
    max_jsonl_line_bytes: usize,
    existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    match plan {
        HarnessParsePlan::Codex(fork_resolutions) => codex::usage::parse_file_summary(
            path,
            max_jsonl_line_bytes,
            existing,
            deadline,
            fork_resolutions.get(key).cloned().unwrap_or_default(),
        ),
        HarnessParsePlan::Claude => {
            claude::usage::parse_file_summary(path, max_jsonl_line_bytes, existing, deadline)
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionFileCandidate {
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
    pub(crate) modified_epoch_secs: Option<u64>,
}

fn collect_session_file_candidates(sessions_root: &Path) -> Vec<SessionFileCandidate> {
    let mut out: Vec<SessionFileCandidate> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![sessions_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Never follow symlinks or read special files (FIFOs/devices) from an untrusted sessions tree.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let len = meta.len();
            if len == 0 {
                continue;
            }

            let modified = meta.modified().ok();
            let modified_epoch_secs = modified
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            out.push(SessionFileCandidate {
                path,
                len,
                modified_epoch_secs,
            });
        }
    }

    out
}

fn cache_entry_matches_candidate(
    entry: &CachedFileScanEntry,
    candidate: &SessionFileCandidate,
) -> bool {
    entry.fully_parsed
        && entry.size == candidate.len
        && entry.modified_epoch_secs == candidate.modified_epoch_secs
        && entry.file_offset >= candidate.len
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    day_keys: Vec<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals: HashMap<String, TokenBreakdown>,
    utc_day_keys: Vec<String>,
    utc_daily: HashMap<String, DailyTotals>,
    utc_model_totals: HashMap<String, TokenBreakdown>,
    matched_session_files: u32,
    activity_first_weekday: Weekday,
    activity_day_keys: Vec<String>,
    project_activity: HashMap<String, ProjectActivityBuilder>,
    project_usage: Vec<ProjectUsageSummary>,
    scan_total_files: usize,
    scan_indexed_files: usize,
    scan_pending_files: usize,
    scan_processed_bytes: u64,
) -> LocalUsageSnapshot {
    let (days, totals, top_models) = build_zone_snapshot(day_keys, daily, model_totals);
    let (utc_days, utc_totals, utc_top_models) =
        build_zone_snapshot(utc_day_keys, utc_daily, utc_model_totals);

    LocalUsageSnapshot {
        days,
        totals,
        top_models,
        utc_days,
        utc_totals,
        utc_top_models,
        activity_first_weekday,
        project_activity: build_project_activity(activity_day_keys, project_activity),
        project_usage,
        matched_session_files,
        scan_total_files,
        scan_indexed_files,
        scan_pending_files,
        scan_processed_bytes,
    }
}

fn build_zone_snapshot(
    day_keys: Vec<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals: HashMap<String, TokenBreakdown>,
) -> (Vec<UsageDay>, UsageTotalsTokens, Vec<LocalUsageModel>) {
    let mut days: Vec<UsageDay> = Vec::with_capacity(day_keys.len());

    for day_key in &day_keys {
        let totals = daily.get(day_key).copied().unwrap_or_default();
        days.push(UsageDay::from_totals(day_key.clone(), totals));
    }

    let last30 = days.iter().rev().take(30).cloned().collect::<Vec<_>>();
    let last7 = days.iter().rev().take(7).cloned().collect::<Vec<_>>();
    let total_tokens: i64 = last30.iter().map(|day| day.total_tokens).sum();
    let last7_tokens: i64 = last7.iter().map(|day| day.total_tokens).sum();
    let last7_prompt: i64 = last7.iter().map(UsageDay::prompt_tokens).sum();
    let last7_cache_read: i64 = last7.iter().map(|day| day.cache_read_tokens).sum();

    let average_daily_tokens = if last7.is_empty() {
        0
    } else {
        ((last7_tokens as f64) / (last7.len() as f64)).round() as i64
    };

    let cache_hit_rate_percent = if last7_prompt > 0 {
        ((last7_cache_read as f64) / (last7_prompt as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let peak = last30
        .iter()
        .max_by_key(|day| day.total_tokens)
        .filter(|day| day.total_tokens > 0);
    let peak_day = peak.map(|day| day.day.clone());
    let peak_day_tokens = peak.map(|day| day.total_tokens).unwrap_or(0);

    let mut top_models: Vec<LocalUsageModel> = model_totals
        .into_iter()
        .map(|(model, tokens)| (model, tokens.total()))
        .filter(|(model, tokens)| model != "unknown" && *tokens > 0)
        .map(|(model, tokens)| LocalUsageModel {
            model,
            tokens,
            share_percent: if total_tokens > 0 {
                ((tokens as f64) / (total_tokens as f64) * 1000.0).round() / 10.0
            } else {
                0.0
            },
        })
        .collect();
    top_models.sort_by_key(|model| std::cmp::Reverse(model.tokens));
    top_models.truncate(4);

    (
        days,
        UsageTotalsTokens {
            last7_days_tokens: last7_tokens,
            last30_days_tokens: total_tokens,
            average_daily_tokens,
            cache_hit_rate_percent,
            peak_day,
            peak_day_tokens,
        },
        top_models,
    )
}

fn build_project_usage_summaries(
    candidates: &[SessionFileCandidate],
    candidate_paths: &[String],
    archived: &[ArchivedUsage],
    cache: &ScanCacheStore,
) -> Vec<ProjectUsageSummary> {
    let mut projects: HashMap<String, ProjectUsageBuilder> = HashMap::new();

    let indexed = candidates
        .iter()
        .zip(candidate_paths)
        .filter_map(|(candidate, path)| {
            cache
                .entries
                .get(path)
                .filter(|entry| cache_entry_matches_candidate(entry, candidate))
                .map(CachedFileScanEntry::usage)
        });
    for usage in indexed.chain(archived.iter().map(ArchivedUsage::usage)) {
        for cwd in entry_project_paths(usage) {
            let key = normalize_project_key(&cwd);
            if key.is_empty() {
                continue;
            }
            let project = projects.entry(key).or_default();
            prefer_project_display_path(&mut project.display_path, &cwd);
            project.indexed_files = project.indexed_files.saturating_add(1);
            for (cache_key, totals) in usage.daily {
                let Some((UsageZone::Local, _)) = split_cache_day_key(cache_key) else {
                    continue;
                };
                project.total_tokens = project.total_tokens.saturating_add(totals.tokens.total());
                project.cache_read_tokens = project
                    .cache_read_tokens
                    .saturating_add(totals.tokens.cache_read);
                project.agent_time_ms = project.agent_time_ms.saturating_add(totals.agent_ms);
                project.agent_runs = project.agent_runs.saturating_add(totals.agent_runs);
            }
        }
    }

    let mut out = projects
        .into_values()
        .map(|project| ProjectUsageSummary {
            display_path: project.display_path,
            total_tokens: project.total_tokens,
            cache_read_tokens: project.cache_read_tokens,
            agent_time_ms: project.agent_time_ms,
            agent_runs: project.agent_runs,
            indexed_files: project.indexed_files,
        })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    out
}

fn entry_project_paths(usage: FileUsageRef<'_>) -> Vec<String> {
    fallback_project_identity(usage.session_cwd)
        .into_iter()
        .collect()
}

fn prefer_project_display_path(current: &mut String, candidate: &str) {
    if current.is_empty()
        || candidate.len() < current.len()
        || (candidate.len() == current.len() && candidate < current.as_str())
    {
        *current = candidate.to_string();
    }
}

fn build_project_activity(
    day_keys: Vec<String>,
    projects: HashMap<String, ProjectActivityBuilder>,
) -> Vec<ProjectActivity> {
    let mut out: Vec<ProjectActivity> = Vec::with_capacity(projects.len());

    for (_, project) in projects {
        let mut days: Vec<UsageDay> = Vec::with_capacity(day_keys.len());
        let mut total_tokens = 0i64;
        let mut cache_read_tokens = 0i64;
        let mut agent_time_ms = 0i64;
        let mut agent_runs = 0i64;
        let mut last_activity_day: Option<String> = None;

        for day_key in &day_keys {
            let totals = project.daily.get(day_key).copied().unwrap_or_default();
            total_tokens += totals.tokens.total();
            cache_read_tokens += totals.tokens.cache_read;
            agent_time_ms += totals.agent_ms;
            agent_runs += totals.agent_runs;
            if daily_has_activity(totals) {
                last_activity_day = Some(day_key.clone());
            }
            days.push(UsageDay::from_totals(day_key.clone(), totals));
        }

        if last_activity_day.is_none() {
            continue;
        }

        out.push(ProjectActivity {
            display_path: project.display_path,
            days,
            last_activity_day,
            total_tokens,
            cache_read_tokens,
            agent_time_ms,
            agent_runs,
        });
    }

    out.sort_by(|left, right| {
        right
            .last_activity_day
            .cmp(&left.last_activity_day)
            .then_with(|| left.display_path.cmp(&right.display_path))
    });
    out
}

fn daily_has_activity(totals: DailyTotals) -> bool {
    totals.tokens.total() > 0 || totals.agent_ms > 0 || totals.agent_runs > 0
}

fn fallback_project_identity(session_cwd: Option<&str>) -> Option<String> {
    session_cwd.and_then(session_cwd_identity)
}

pub(crate) fn add_model_tokens_limited(
    model_totals: &mut HashMap<String, TokenBreakdown>,
    model: String,
    tokens: TokenBreakdown,
) {
    if tokens.total() <= 0 {
        return;
    }
    if model_totals.len() <= MAX_DISTINCT_MODELS || model_totals.contains_key(&model) {
        model_totals.entry(model).or_default().add(tokens);
    } else {
        model_totals
            .entry("other".to_string())
            .or_default()
            .add(tokens);
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_file_usage(
    usage: FileUsageRef<'_>,
    workspace_path: Option<&Path>,
    daily: &mut HashMap<String, DailyTotals>,
    model_totals: &mut HashMap<String, TokenBreakdown>,
    chart_day_filter: &HashSet<String>,
    utc_daily: &mut HashMap<String, DailyTotals>,
    utc_model_totals: &mut HashMap<String, TokenBreakdown>,
    utc_chart_day_filter: &HashSet<String>,
    project_activity: &mut HashMap<String, ProjectActivityBuilder>,
    matched_session_files: &mut u32,
) {
    let matches_workspace = match workspace_path {
        None => true,
        Some(filter) => entry_project_paths(usage)
            .iter()
            .any(|project| path_matches_workspace(project, filter)),
    };
    if !matches_workspace {
        return;
    }

    if workspace_path.is_some() {
        *matched_session_files = matched_session_files.saturating_add(1);
    }

    for (cache_key, totals) in usage.daily {
        let Some((zone, day_key)) = split_cache_day_key(cache_key) else {
            continue;
        };
        let target = match zone {
            UsageZone::Local => &mut *daily,
            UsageZone::Utc => &mut *utc_daily,
        };
        target.entry(day_key.to_string()).or_default().add(*totals);
    }

    for (cache_key, per_day_models) in usage.model_totals_by_day {
        let Some((zone, day_key)) = split_cache_day_key(cache_key) else {
            continue;
        };
        let (filter, target) = match zone {
            UsageZone::Local => (chart_day_filter, &mut *model_totals),
            UsageZone::Utc => (utc_chart_day_filter, &mut *utc_model_totals),
        };
        if !filter.contains(day_key) {
            continue;
        }
        for (model, tokens) in per_day_models {
            add_model_tokens_limited(target, model.clone(), *tokens);
        }
    }

    for project in entry_project_paths(usage) {
        apply_project_activity(&project, usage.daily, daily, project_activity);
    }
}

fn apply_project_activity(
    cwd: &str,
    entry_daily: &HashMap<String, DailyTotals>,
    day_filter: &HashMap<String, DailyTotals>,
    project_activity: &mut HashMap<String, ProjectActivityBuilder>,
) {
    if cwd.trim().is_empty() {
        return;
    }
    let key = normalize_project_key(cwd);
    if key.is_empty() {
        return;
    }

    let builder = project_activity.entry(key).or_default();
    if builder.display_path.is_empty()
        || cwd.len() < builder.display_path.len()
        || (cwd.len() == builder.display_path.len() && cwd < builder.display_path.as_str())
    {
        builder.display_path = cwd.to_string();
    }

    for (cache_key, totals) in entry_daily {
        let Some((UsageZone::Local, day_key)) = split_cache_day_key(cache_key) else {
            continue;
        };
        if !day_filter.contains_key(day_key) {
            continue;
        }
        builder
            .daily
            .entry(day_key.to_string())
            .or_default()
            .add(*totals);
    }
}

/// Prepares the private directory of an SQLite file and refuses symlinks or
/// special files at the database, WAL, and SHM paths.
fn prepare_private_db_path(path: &Path, label: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::storage::ensure_private_dir(parent)?;
        ensure_directory_not_symlink(parent, &format!("{label} parent directory"))?;
    }
    ensure_regular_file_or_missing(path, &format!("{label} file"))?;
    let mut wal_path: OsString = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let mut shm_path: OsString = path.as_os_str().to_os_string();
    shm_path.push("-shm");
    ensure_regular_file_or_missing(Path::new(&wal_path), &format!("{label} WAL file"))?;
    ensure_regular_file_or_missing(Path::new(&shm_path), &format!("{label} SHM file"))?;
    Ok(())
}

fn open_or_init_scan_cache_db(path: &Path) -> Result<ScanCacheDb> {
    prepare_private_db_path(path, "cache database")?;

    let mut conn = Connection::open(path)
        .with_context(|| format!("Unable to open cache database {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("Unable to set WAL journal mode for {}", path.display()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .with_context(|| format!("Unable to set synchronous mode for {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .with_context(|| format!("Unable to enable foreign keys for {}", path.display()))?;

    init_scan_cache_tables(&mut conn)
        .with_context(|| format!("Unable to initialize cache database {}", path.display()))?;

    let db = ScanCacheDb {
        path: path.to_path_buf(),
        conn,
    };
    enforce_private_db_files(&db.path, "cache database")?;
    Ok(db)
}

fn harness_cache_schema_version(harness: Harness) -> i64 {
    match harness {
        Harness::Codex => CODEX_CACHE_SCHEMA_VERSION,
        Harness::Claude => CLAUDE_CACHE_SCHEMA_VERSION,
    }
}

/// Creates the cache tables and applies version changes. Cached rows are
/// derived from the raw logs, so a database without a layout version (for
/// example a copied comon.db) is reset, and a harness whose schema version
/// changed loses only its own rows.
fn init_scan_cache_tables(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS cache_meta (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        ",
    )?;
    let layout_version: Option<i64> = tx
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'layout_version';",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match layout_version {
        Some(SCAN_CACHE_DB_LAYOUT_VERSION) => {}
        Some(other) => anyhow::bail!(
            "Unsupported scan cache layout version: {other} (expected {SCAN_CACHE_DB_LAYOUT_VERSION})"
        ),
        None => {
            tx.execute_batch(
                "
                DROP TABLE IF EXISTS file_cache;
                DELETE FROM cache_meta;
                ",
            )?;
            tx.execute(
                "INSERT INTO cache_meta(key, value) VALUES('layout_version', ?1);",
                params![SCAN_CACHE_DB_LAYOUT_VERSION],
            )?;
        }
    }
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS file_cache (
            harness TEXT NOT NULL,
            file_path TEXT NOT NULL,
            file_size INTEGER NOT NULL,
            file_mtime INTEGER,
            file_offset INTEGER NOT NULL DEFAULT 0,
            fully_parsed INTEGER NOT NULL DEFAULT 1,
            session_cwd TEXT,
            parser_state_json TEXT NOT NULL DEFAULT '{}',
            daily_json TEXT NOT NULL,
            model_daily_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (harness, file_path)
        );
        CREATE INDEX IF NOT EXISTS idx_file_cache_harness_updated_at
            ON file_cache(harness, updated_at);
        ",
    )?;
    for harness in ALL_HARNESSES {
        let key = format!("schema_version.{}", harness.key());
        let expected = harness_cache_schema_version(harness);
        let stored: Option<i64> = tx
            .query_row(
                "SELECT value FROM cache_meta WHERE key = ?1;",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        if stored != Some(expected) {
            tx.execute(
                "DELETE FROM file_cache WHERE harness = ?1;",
                params![harness.key()],
            )?;
            tx.execute(
                "
                INSERT INTO cache_meta(key, value) VALUES(?1, ?2)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value;
                ",
                params![key, expected],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn load_scan_cache_store(
    db: &ScanCacheDb,
    harness: Harness,
) -> Result<(ScanCacheStore, HashSet<String>)> {
    let mut store = ScanCacheStore::default();
    let mut invalid_paths: HashSet<String> = HashSet::new();
    let mut stmt = db
        .conn
        .prepare(
            "
            SELECT
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                session_cwd,
                parser_state_json,
                daily_json,
                model_daily_json,
                updated_at
            FROM file_cache
            WHERE harness = ?1;
            ",
        )
        .with_context(|| format!("Unable to query cache entries from {}", db.path.display()))?;
    let mut rows = stmt
        .query(params![harness.key()])
        .with_context(|| format!("Unable to iterate cache entries from {}", db.path.display()))?;
    while let Some(row) = rows
        .next()
        .with_context(|| format!("Unable to read cache row from {}", db.path.display()))?
    {
        let file_path: String = row.get(0).with_context(|| {
            format!(
                "Unable to read file_path from cache row in {}",
                db.path.display()
            )
        })?;
        let file_size_raw: i64 = row.get(1).with_context(|| {
            format!(
                "Unable to read file_size from cache row in {}",
                db.path.display()
            )
        })?;
        let file_mtime_raw: Option<i64> = row.get(2).with_context(|| {
            format!(
                "Unable to read file_mtime from cache row in {}",
                db.path.display()
            )
        })?;
        let file_offset_raw: i64 = row.get(3).with_context(|| {
            format!(
                "Unable to read file_offset from cache row in {}",
                db.path.display()
            )
        })?;
        let fully_parsed_raw: i64 = row.get(4).with_context(|| {
            format!(
                "Unable to read fully_parsed from cache row in {}",
                db.path.display()
            )
        })?;
        let session_cwd: Option<String> = row.get(5).with_context(|| {
            format!(
                "Unable to read session_cwd from cache row in {}",
                db.path.display()
            )
        })?;
        let parser_state_json: String = row.get(6).with_context(|| {
            format!(
                "Unable to read parser_state_json from cache row in {}",
                db.path.display()
            )
        })?;
        let daily_json: String = row.get(7).with_context(|| {
            format!(
                "Unable to read daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let model_daily_json: String = row.get(8).with_context(|| {
            format!(
                "Unable to read model_daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let updated_at: i64 = row.get(9).with_context(|| {
            format!(
                "Unable to read updated_at from cache row in {}",
                db.path.display()
            )
        })?;

        let Ok(file_size) = u64::try_from(file_size_raw.max(0)) else {
            invalid_paths.insert(file_path);
            continue;
        };
        let file_mtime = file_mtime_raw.and_then(|value| u64::try_from(value).ok());
        let file_offset = match u64::try_from(file_offset_raw.max(0)) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let parser_state = match HarnessParserState::from_json(harness, &parser_state_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let daily = match serde_json::from_str::<HashMap<String, DailyTotals>>(&daily_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let model_totals_by_day = match serde_json::from_str::<
            HashMap<String, HashMap<String, TokenBreakdown>>,
        >(&model_daily_json)
        {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };

        store.entries.insert(
            file_path,
            CachedFileScanEntry {
                size: file_size,
                modified_epoch_secs: file_mtime,
                file_offset,
                fully_parsed: fully_parsed_raw != 0,
                session_cwd,
                parser_state,
                daily,
                model_totals_by_day,
                updated_at,
            },
        );
    }

    Ok((store, invalid_paths))
}

#[cfg(test)]
fn prune_scan_cache_store(
    store: &mut ScanCacheStore,
    sessions_root: &Path,
    max_session_file_bytes: u64,
    max_entries: usize,
    removed_paths: &mut HashSet<String>,
) -> bool {
    let mut pruned = false;

    store.entries.retain(|entry_path, _| {
        let path = Path::new(entry_path);
        let keep = is_valid_cached_session_file(path, sessions_root, max_session_file_bytes);
        if !keep {
            removed_paths.insert(entry_path.clone());
            pruned = true;
        }
        keep
    });

    trim_scan_cache_to_limit(store, max_entries.max(1), removed_paths) || pruned
}

#[cfg(test)]
fn trim_scan_cache_to_limit(
    store: &mut ScanCacheStore,
    max_entries: usize,
    removed_paths: &mut HashSet<String>,
) -> bool {
    if store.entries.len() <= max_entries {
        return false;
    }
    let mut entries_by_age: Vec<(String, i64)> = store
        .entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.updated_at))
        .collect();
    entries_by_age
        .sort_unstable_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

    let remove_count = entries_by_age.len().saturating_sub(max_entries);
    for (key, _) in entries_by_age.into_iter().take(remove_count) {
        store.entries.remove(&key);
        removed_paths.insert(key);
    }
    true
}

fn persist_scan_cache_changes(
    db: &mut ScanCacheDb,
    harness: Harness,
    store: &ScanCacheStore,
    removed_paths: &HashSet<String>,
    dirty_paths: &HashSet<String>,
) -> Result<()> {
    if removed_paths.is_empty() && dirty_paths.is_empty() {
        return Ok(());
    }

    let tx = db
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .with_context(|| {
            format!(
                "Unable to start cache transaction for {}",
                db.path.display()
            )
        })?;

    let mut delete_stmt = tx
        .prepare("DELETE FROM file_cache WHERE harness = ?1 AND file_path = ?2;")
        .with_context(|| {
            format!(
                "Unable to prepare delete statement for {}",
                db.path.display()
            )
        })?;
    let mut upsert_stmt = tx
        .prepare(
            "
            INSERT INTO file_cache(
                harness,
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                session_cwd,
                parser_state_json,
                daily_json,
                model_daily_json,
                updated_at
            )
            VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(harness, file_path) DO UPDATE SET
                file_size=excluded.file_size,
                file_mtime=excluded.file_mtime,
                file_offset=excluded.file_offset,
                fully_parsed=excluded.fully_parsed,
                session_cwd=excluded.session_cwd,
                parser_state_json=excluded.parser_state_json,
                daily_json=excluded.daily_json,
                model_daily_json=excluded.model_daily_json,
                updated_at=excluded.updated_at;
            ",
        )
        .with_context(|| {
            format!(
                "Unable to prepare upsert statement for {}",
                db.path.display()
            )
        })?;

    let mut removed_sorted: Vec<&String> = removed_paths.iter().collect();
    removed_sorted.sort();
    for file_path in removed_sorted {
        delete_stmt
            .execute(params![harness.key(), file_path])
            .with_context(|| format!("Unable to delete cache entry in {}", db.path.display()))?;
    }

    let mut dirty_sorted: Vec<&String> = dirty_paths.iter().collect();
    dirty_sorted.sort();
    for file_path in dirty_sorted {
        let Some(entry) = store.entries.get(file_path) else {
            continue;
        };
        let daily_json = serde_json::to_string(&entry.daily)
            .with_context(|| format!("Unable to serialize daily cache JSON for {}", file_path))?;
        let model_daily_json =
            serde_json::to_string(&entry.model_totals_by_day).with_context(|| {
                format!(
                    "Unable to serialize model-daily cache JSON for {}",
                    file_path
                )
            })?;
        let parser_state_json = entry.parser_state.to_json().with_context(|| {
            format!(
                "Unable to serialize parser-state cache JSON for {}",
                file_path
            )
        })?;
        let file_size = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let file_mtime = entry
            .modified_epoch_secs
            .and_then(|value| i64::try_from(value).ok());
        let file_offset = i64::try_from(entry.file_offset).unwrap_or(i64::MAX);
        let fully_parsed = if entry.fully_parsed { 1_i64 } else { 0_i64 };

        upsert_stmt
            .execute(params![
                harness.key(),
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                entry.session_cwd.as_deref(),
                parser_state_json,
                daily_json,
                model_daily_json,
                entry.updated_at
            ])
            .with_context(|| format!("Unable to upsert cache entry in {}", db.path.display()))?;
    }

    drop(delete_stmt);
    drop(upsert_stmt);
    tx.commit().with_context(|| {
        format!(
            "Unable to commit cache transaction for {}",
            db.path.display()
        )
    })?;
    enforce_private_db_files(&db.path, "cache database")?;
    Ok(())
}

fn trim_scan_cache_db_entries_to_limit(
    db: &ScanCacheDb,
    harness: Harness,
    max_entries: usize,
) -> Result<bool> {
    let max_entries = max_entries.max(1);
    let max_entries_i64 = i64::try_from(max_entries).unwrap_or(i64::MAX);
    let total_rows: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM file_cache WHERE harness = ?1;",
            params![harness.key()],
            |row| row.get(0),
        )
        .with_context(|| format!("Unable to count cache rows in {}", db.path.display()))?;
    if total_rows <= max_entries_i64 {
        return Ok(false);
    }

    let remove_count = total_rows - max_entries_i64;
    db.conn
        .execute(
            "
            DELETE FROM file_cache
            WHERE harness = ?1 AND file_path IN (
                SELECT file_path
                FROM file_cache
                WHERE harness = ?1
                ORDER BY updated_at ASC, file_path ASC
                LIMIT ?2
            );
            ",
            params![harness.key(), remove_count],
        )
        .with_context(|| format!("Unable to trim cache rows in {}", db.path.display()))?;
    enforce_private_db_files(&db.path, "cache database")?;
    Ok(true)
}

#[cfg(test)]
fn is_valid_cached_session_file(
    path: &Path,
    sessions_root: &Path,
    max_session_file_bytes: u64,
) -> bool {
    if !path.starts_with(sessions_root) {
        return false;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return false;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return false;
    }
    let len = meta.len();
    len > 0 && len <= max_session_file_bytes
}

fn enforce_private_db_files(path: &Path, label: &str) -> Result<()> {
    ensure_regular_file_or_missing(path, &format!("{label} file"))?;
    crate::storage::enforce_private_file_if_exists(path)?;
    let mut wal_path: OsString = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let mut shm_path: OsString = path.as_os_str().to_os_string();
    shm_path.push("-shm");
    ensure_regular_file_or_missing(Path::new(&wal_path), &format!("{label} WAL file"))?;
    ensure_regular_file_or_missing(Path::new(&shm_path), &format!("{label} SHM file"))?;
    let _ = crate::storage::enforce_private_file_if_exists(Path::new(&wal_path));
    let _ = crate::storage::enforce_private_file_if_exists(Path::new(&shm_path));
    Ok(())
}

fn ensure_directory_not_symlink(path: &Path, label: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Unable to inspect {} {}", label, path.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        anyhow::bail!(
            "Refusing to use {} {}: symlink is not allowed",
            label,
            path.display()
        );
    }
    if !ft.is_dir() {
        anyhow::bail!(
            "Refusing to use {} {}: expected directory",
            label,
            path.display()
        );
    }
    Ok(())
}

fn ensure_regular_file_or_missing(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                anyhow::bail!(
                    "Refusing to use {} {}: symlink is not allowed",
                    label,
                    path.display()
                );
            }
            if !ft.is_file() {
                anyhow::bail!(
                    "Refusing to use {} {}: expected regular file",
                    label,
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Unable to inspect {} {}", label, path.display()))
        }
    }
}

pub(crate) fn read_timestamp_ms(value: &Value) -> Option<i64> {
    parse_timestamp_value_ms(value.get("timestamp")?)
}

pub(crate) fn parse_timestamp_value_ms(raw: &Value) -> Option<i64> {
    if let Some(text) = raw.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .map(|value| value.timestamp_millis())
            .ok();
    }
    let numeric = raw
        .as_i64()
        .or_else(|| raw.as_f64().map(|value| value as i64))?;
    if numeric > 0 && numeric < 1_000_000_000_000 {
        return Some(numeric * 1000);
    }
    Some(numeric)
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// How long a log must stay unmodified before its unterminated last line is
/// final. Harnesses append whole records, so a tail without a newline that has
/// not changed for this long was cut off (a crash can also leave NUL padding
/// there) and will never be completed.
const UNTERMINATED_TAIL_SETTLE_SECS: u64 = 10 * 60;

/// Whether a parser should read an unterminated last line instead of leaving
/// it for the next refresh. A cut-off tail that is not valid JSON is then
/// skipped like any other bad line, so the file no longer stays pending.
pub(crate) fn unterminated_tail_is_final(modified_epoch_secs: Option<u64>) -> bool {
    modified_epoch_secs.is_some_and(|modified| {
        u64::try_from(unix_time_seconds())
            .is_ok_and(|now| now.saturating_sub(modified) >= UNTERMINATED_TAIL_SETTLE_SECS)
    })
}

pub(crate) fn track_activity(
    daily: &mut HashMap<String, DailyTotals>,
    last_activity_ms: &mut Option<i64>,
    timestamp_ms: i64,
) {
    if let Some(prev_ms) = *last_activity_ms {
        let delta = timestamp_ms - prev_ms;
        if delta > 0 && delta <= MAX_ACTIVITY_GAP_MS {
            for zone in [UsageZone::Local, UsageZone::Utc] {
                if let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) {
                    daily.entry(day_key).or_default().agent_ms += delta;
                }
            }
        }
    }
    *last_activity_ms = Some(timestamp_ms);
}

pub(crate) fn add_agent_run(daily: &mut HashMap<String, DailyTotals>, timestamp_ms: i64) {
    for zone in [UsageZone::Local, UsageZone::Utc] {
        if let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) {
            daily.entry(day_key).or_default().agent_runs += 1;
        }
    }
}

fn display_day_key_for_timestamp_ms(timestamp_ms: i64, zone: UsageZone) -> Option<String> {
    let utc = Utc.timestamp_millis_opt(timestamp_ms).single()?;
    Some(match zone {
        UsageZone::Local => utc.with_timezone(&Local).format("%Y-%m-%d").to_string(),
        UsageZone::Utc => utc.format("%Y-%m-%d").to_string(),
    })
}

pub(crate) fn cache_day_key_for_timestamp_ms(timestamp_ms: i64, zone: UsageZone) -> Option<String> {
    let day = display_day_key_for_timestamp_ms(timestamp_ms, zone)?;
    Some(match zone {
        UsageZone::Local => format!("L:{day}"),
        UsageZone::Utc => format!("U:{day}"),
    })
}

fn split_cache_day_key(value: &str) -> Option<(UsageZone, &str)> {
    if let Some(day) = value.strip_prefix("L:") {
        Some((UsageZone::Local, day))
    } else {
        value.strip_prefix("U:").map(|day| (UsageZone::Utc, day))
    }
}

fn path_matches_workspace(cwd: &str, workspace_path: &Path) -> bool {
    let Some(cwd) = session_cwd_identity(cwd) else {
        return false;
    };
    let Some(workspace) = workspace_path.to_str().and_then(session_cwd_identity) else {
        return false;
    };
    let cwd_path = Path::new(&cwd);
    let workspace_path = Path::new(&workspace);
    cwd_path == workspace_path || cwd_path.starts_with(workspace_path)
}

pub(crate) fn is_uuid_like(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Short display name of a model id: `claude-opus-5-5` -> `Opus 5.5`. Other
/// ids are returned unchanged.
pub fn model_display_name(model: &str) -> String {
    let Some(rest) = model.strip_prefix("claude-") else {
        return model.to_string();
    };
    let mut parts = rest.split('-');
    let Some(family) = parts.next().filter(|family| !family.is_empty()) else {
        return model.to_string();
    };
    // Version parts are short numbers; an 8-digit date suffix is dropped.
    let version: Vec<&str> = parts
        .take_while(|part| part.len() <= 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    let mut name = family[..1].to_ascii_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

pub(crate) fn normalize_project_key(path: &str) -> String {
    let mut normalized = session_cwd_identity(path).unwrap_or_else(|| {
        normalize_wsl_unc_path(path).unwrap_or_else(|| path.trim().replace('\\', "/"))
    });
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(windows)]
    {
        normalized.make_ascii_lowercase();
    }
    normalized
}

/// Codex-native project identity: the session working directory only.
///
/// Does not walk the filesystem for `.git`. Optional git branch/remote from
/// session logs remain display metadata elsewhere and never redefine this key.
pub(crate) fn session_cwd_identity(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 4096 || raw.chars().any(char::is_control) {
        return None;
    }
    let normalized = normalize_cross_platform_path(raw)?;
    if !normalized.is_absolute() {
        return None;
    }
    let mut display = normalized.display().to_string();
    while display.len() > 1 && (display.ends_with('/') || display.ends_with('\\')) {
        display.pop();
    }
    if display.trim().is_empty() {
        return None;
    }
    Some(display)
}

fn normalize_cross_platform_path(path: &str) -> Option<PathBuf> {
    let normalized = normalize_wsl_unc_path(path).unwrap_or_else(|| path.trim().replace('\\', "/"));
    if normalized.is_empty() {
        return None;
    }

    #[cfg(unix)]
    {
        let bytes = normalized.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/'
        {
            let drive = char::from(bytes[0]).to_ascii_lowercase();
            return Some(PathBuf::from(format!("/mnt/{drive}/{}", &normalized[3..])));
        }
    }

    Some(PathBuf::from(normalized))
}

fn normalize_wsl_unc_path(path: &str) -> Option<String> {
    let normalized = path.trim().replace('\\', "/");
    let lower = normalized.to_lowercase();
    let prefix_len = if lower.starts_with("//wsl.localhost/") {
        "//wsl.localhost/".len()
    } else if lower.starts_with("//wsl$/") {
        "//wsl$/".len()
    } else {
        return None;
    };
    let rest = normalized.get(prefix_len..)?;
    let (_, linux_path) = rest.split_once('/')?;
    Some(format!("/{linux_path}"))
}

fn make_day_keys_for_zone(days: u32, zone: UsageZone) -> Vec<String> {
    let today = match zone {
        UsageZone::Local => Local::now().date_naive(),
        UsageZone::Utc => Utc::now().date_naive(),
    };
    (0..days)
        .rev()
        .map(|offset| {
            let day = today - Duration::days(offset as i64);
            day.format("%Y-%m-%d").to_string()
        })
        .collect()
}

fn make_complete_chart_day_keys(
    daily: &HashMap<String, DailyTotals>,
    zone: UsageZone,
    fallback: &[String],
) -> Vec<String> {
    let mut active_dates = daily.iter().filter_map(|(day, totals)| {
        daily_has_activity(*totals)
            .then(|| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
            .flatten()
    });
    let Some(mut first) = active_dates.next() else {
        return fallback.to_vec();
    };
    let mut last = first;
    for date in active_dates {
        first = first.min(date);
        last = last.max(date);
    }

    let today = match zone {
        UsageZone::Local => Local::now().date_naive(),
        UsageZone::Utc => Utc::now().date_naive(),
    };
    last = last.max(today);

    let mut keys = Vec::with_capacity(
        last.signed_duration_since(first)
            .num_days()
            .max(0)
            .saturating_add(1) as usize,
    );
    let mut date = first;
    while date <= last {
        keys.push(date.format("%Y-%m-%d").to_string());
        let Some(next) = date.checked_add_signed(Duration::days(1)) else {
            break;
        };
        date = next;
    }
    keys
}

pub fn system_first_weekday() -> Weekday {
    locale_region_from_env()
        .as_deref()
        .map(first_weekday_for_region)
        .unwrap_or(Weekday::Mon)
}

fn locale_region_from_env() -> Option<String> {
    for key in ["LC_TIME", "LC_ALL", "LANG"] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        return locale_region(&value);
    }
    None
}

fn locale_region(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("c")
        || trimmed.eq_ignore_ascii_case("posix")
    {
        return None;
    }
    let base = trimmed
        .split(['.', '@'])
        .next()
        .unwrap_or(trimmed)
        .replace('-', "_");
    let mut parts = base.split('_');
    let _language = parts.next()?;
    let region = parts.next()?.trim();
    if region.len() != 2 || !region.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    Some(region.to_ascii_uppercase())
}

fn first_weekday_for_region(region: &str) -> Weekday {
    match region.to_ascii_uppercase().as_str() {
        "AE" | "AF" | "BH" | "DJ" | "DZ" | "EG" | "IQ" | "IR" | "JO" | "KW" | "LY" | "OM"
        | "QA" | "SD" | "SY" | "YE" => Weekday::Sat,
        "AG" | "AR" | "AS" | "AU" | "BD" | "BR" | "BS" | "BT" | "BW" | "BZ" | "CA" | "CN"
        | "CO" | "DM" | "DO" | "ET" | "GT" | "GU" | "HK" | "HN" | "ID" | "IL" | "IN" | "JM"
        | "JP" | "KE" | "KH" | "KR" | "LA" | "MH" | "MM" | "MO" | "MT" | "MX" | "MZ" | "NI"
        | "NP" | "PA" | "PE" | "PH" | "PK" | "PR" | "PT" | "PY" | "SA" | "SG" | "SV" | "TH"
        | "TT" | "TW" | "UM" | "US" | "VE" | "VI" | "WS" | "ZA" | "ZW" => Weekday::Sun,
        _ => Weekday::Mon,
    }
}

fn make_activity_day_keys(first_weekday: Weekday) -> Vec<String> {
    let today = Local::now().date_naive();
    let days_from_week_start = days_since_week_start(today.weekday(), first_weekday);
    let current_week_start = today - Duration::days(days_from_week_start);
    let first_day = current_week_start - Duration::weeks((ACTIVITY_TIMELINE_WEEKS - 1) as i64);
    (0..ACTIVITY_TIMELINE_DAYS)
        .map(|offset| {
            let day = first_day + Duration::days(offset as i64);
            day.format("%Y-%m-%d").to_string()
        })
        .collect()
}

fn days_since_week_start(day: Weekday, first_weekday: Weekday) -> i64 {
    let day = day.num_days_from_monday() as i64;
    let first = first_weekday.num_days_from_monday() as i64;
    (7 + day - first) % 7
}

pub fn format_count(value: i64, formatter: DisplayFormatter<'_>) -> String {
    formatter.format_count(value)
}

pub fn format_tokens_overview(value: i64, formatter: DisplayFormatter<'_>) -> String {
    match formatter.style() {
        DisplayStyle::Classic | DisplayStyle::SystemFull => format_count(value, formatter),
        DisplayStyle::SystemCompact => format_tokens_compact(value, formatter),
    }
}

pub fn format_tokens_compact(value: i64, formatter: DisplayFormatter<'_>) -> String {
    let v = value.max(0) as u64;
    if v < 1000 {
        return formatter.format_u64(v);
    }

    let (div, suffix) = if v >= 1_000_000_000_000 {
        (1_000_000_000_000f64, "T")
    } else if v >= 1_000_000_000 {
        (1_000_000_000f64, "B")
    } else if v >= 1_000_000 {
        (1_000_000f64, "M")
    } else {
        (1_000f64, "K")
    };
    let scaled = (v as f64) / div;
    format_compact_scaled(scaled, suffix, 2, formatter)
}

pub fn format_duration_compact(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

pub fn format_duration(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    secs %= 60;
    if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::codex::usage::{
        fork_replay_should_skip_event, resolve_session_owner, ForkReplayState, ParserState,
        SessionOwnerSource,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let dir = std::env::temp_dir().join(format!("llmon-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        let line = serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input_tokens,
                        "cached_input_tokens": 0,
                        "output_tokens": output_tokens
                    },
                    "model": "gpt-test"
                }
            }
        });
        std::fs::write(path, format!("{line}\n")).expect("write token file");
    }

    fn append_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        let line = serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input_tokens,
                        "cached_input_tokens": 0,
                        "output_tokens": output_tokens
                    },
                    "model": "gpt-test"
                }
            }
        });
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open token file for append");
        use std::io::Write as _;
        writeln!(file, "{line}").expect("append token line");
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

    fn append_total_token_line(
        path: &Path,
        timestamp_ms: i64,
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
    ) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": input_tokens,
                            "cached_input_tokens": cached_input_tokens,
                            "output_tokens": output_tokens
                        },
                        "model": "gpt-test"
                    }
                }
            }),
        );
    }

    fn append_agent_message_line(path: &Path, timestamp_ms: i64) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {
                    "type": "agent_message",
                    "message": "ok"
                }
            }),
        );
    }

    fn append_session_meta_line(path: &Path, timestamp_ms: i64, cwd: &str) {
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": timestamp,
                "payload": {
                    "id": cwd,
                    "timestamp": timestamp,
                    "cwd": cwd
                }
            }),
        );
    }

    fn write_forked_replay_file(path: &Path, timestamp_ms: i64) {
        let fork_timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": fork_timestamp,
                "payload": {
                    "id": "fork-child",
                    "forked_from_id": "parent",
                    "timestamp": fork_timestamp,
                    "cwd": "/tmp/forked-project"
                }
            }),
        );
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": fork_timestamp,
                "payload": {
                    "id": "parent",
                    "timestamp": fork_timestamp,
                    "cwd": "/tmp/forked-project"
                }
            }),
        );

        append_total_token_line(path, timestamp_ms + 100, 1_000, 800, 100);
        append_agent_message_line(path, timestamp_ms + 200);
        append_total_token_line(path, timestamp_ms + 400, 1_500, 1_300, 130);
        append_total_token_line(path, timestamp_ms + 1_700, 1_700, 1_400, 150);
        append_agent_message_line(path, timestamp_ms + 1_800);
    }

    fn write_delayed_fork_replay_prefix(
        path: &Path,
        payload_timestamp_ms: i64,
        outer_delay_ms: i64,
    ) -> i64 {
        let payload_timestamp = Utc
            .timestamp_millis_opt(payload_timestamp_ms)
            .single()
            .expect("valid payload timestamp")
            .to_rfc3339();
        let outer_timestamp_ms = payload_timestamp_ms + outer_delay_ms;
        let outer_timestamp = Utc
            .timestamp_millis_opt(outer_timestamp_ms)
            .single()
            .expect("valid outer timestamp")
            .to_rfc3339();
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": outer_timestamp,
                "payload": {
                    "id": "fork-child",
                    "forked_from_id": "parent",
                    "timestamp": payload_timestamp,
                    "cwd": "/tmp/forked-project"
                }
            }),
        );
        append_json_line(
            path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": Utc
                    .timestamp_millis_opt(outer_timestamp_ms + 1)
                    .single()
                    .expect("valid replay metadata timestamp")
                    .to_rfc3339(),
                "payload": {
                    "id": "parent",
                    "timestamp": payload_timestamp,
                    "cwd": "/tmp/forked-project"
                }
            }),
        );
        append_total_token_line(path, outer_timestamp_ms + 2, 1_000, 800, 100);
        for offset in 3..67 {
            append_agent_message_line(path, outer_timestamp_ms + offset);
        }
        append_total_token_line(path, outer_timestamp_ms + 100, 1_500, 1_300, 130);
        outer_timestamp_ms
    }

    fn append_fork_replay_live_tail(path: &Path, outer_timestamp_ms: i64) {
        append_total_token_line(path, outer_timestamp_ms + 4_000, 1_700, 1_400, 150);
        append_agent_message_line(path, outer_timestamp_ms + 4_001);
    }

    fn default_test_limits(full_scan: bool) -> ScanLimits {
        ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan,
            scan_cache_max_entries: 1000,
        }
    }

    #[test]
    fn locale_region_parses_common_locale_values() {
        assert_eq!(locale_region("ja_JP.UTF-8").as_deref(), Some("JP"));
        assert_eq!(locale_region("en-US").as_deref(), Some("US"));
        assert_eq!(locale_region("C"), None);
    }

    #[test]
    fn first_weekday_uses_region_defaults() {
        assert_eq!(first_weekday_for_region("JP"), Weekday::Sun);
        assert_eq!(first_weekday_for_region("US"), Weekday::Sun);
        assert_eq!(first_weekday_for_region("GB"), Weekday::Mon);
    }

    #[test]
    fn activity_day_keys_start_on_requested_weekday() {
        let sunday_keys = make_activity_day_keys(Weekday::Sun);
        let monday_keys = make_activity_day_keys(Weekday::Mon);
        let sunday = chrono::NaiveDate::parse_from_str(&sunday_keys[0], "%Y-%m-%d")
            .expect("parse sunday-first activity key");
        let monday = chrono::NaiveDate::parse_from_str(&monday_keys[0], "%Y-%m-%d")
            .expect("parse monday-first activity key");
        assert_eq!(sunday.weekday(), Weekday::Sun);
        assert_eq!(monday.weekday(), Weekday::Mon);
    }

    #[test]
    fn fork_replay_keeps_last_event_timestamp_monotonic() {
        let mut replay = ForkReplayState {
            active: true,
            done: false,
            start_ms: Some(1_000),
            last_event_ms: Some(1_000),
            token_events: 1,
        };

        assert!(fork_replay_should_skip_event(&mut replay, Some(1_100)));
        assert!(fork_replay_should_skip_event(&mut replay, Some(900)));
        assert_eq!(replay.last_event_ms, Some(1_100));
        assert!(fork_replay_should_skip_event(&mut replay, Some(1_150)));
        assert!(replay.active);
        assert!(!replay.done);
    }

    #[test]
    fn compute_snapshot_marks_missing_fork_parent_partial_without_cache() {
        let root = make_temp_dir("fork-replay-no-cache");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("forked.jsonl");
        write_forked_replay_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
        );

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        assert_eq!(snapshot.totals.last30_days_tokens, 0);
        assert_eq!(snapshot.scan_pending_files, 1);
        assert_eq!(
            snapshot.days.iter().map(|day| day.agent_runs).sum::<i64>(),
            0
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_missing_fork_parent_partial_with_cache() {
        let root = make_temp_dir("fork-replay-cache");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("forked.jsonl");
        write_forked_replay_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
        );

        let cache_db_path = root.join("llmon.db");
        let first = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 0);
        assert_eq!(first.scan_pending_files, 1);

        let cached = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("cached snapshot");
        assert_eq!(cached.totals.last30_days_tokens, 0);
        assert_eq!(cached.scan_pending_files, 1);
        assert_eq!(cached.days.iter().map(|day| day.agent_runs).sum::<i64>(), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_uses_parent_totals_as_fork_baseline() {
        let root = make_temp_dir("fork-parent-baseline");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let parent_id = "11111111-1111-4111-8111-111111111111";
        let child_id = "22222222-2222-4222-8222-222222222222";
        let parent_path = sessions_root.join(format!("rollout-parent-{parent_id}.jsonl"));
        let child_path = sessions_root.join(format!("rollout-child-{child_id}.jsonl"));
        let fork_ms = Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();
        append_session_meta_line(&parent_path, fork_ms - 1_000, "/tmp/forked-project");
        append_total_token_line(&parent_path, fork_ms - 100, 1_500, 1_300, 130);

        append_json_line(
            &child_path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": Utc.timestamp_millis_opt(fork_ms).single().unwrap().to_rfc3339(),
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": Utc.timestamp_millis_opt(fork_ms).single().unwrap().to_rfc3339(),
                    "cwd": "/tmp/forked-project"
                }
            }),
        );
        // Copied replay response: it is inside the replay burst and must not
        // become a new run in the child.
        append_agent_message_line(&child_path, fork_ms + 50);
        append_total_token_line(&child_path, fork_ms + 100, 1_500, 1_300, 130);
        // Real child response precedes its token_count event. The one-second
        // gap ends the copied replay prefix, so this must count as one run.
        append_agent_message_line(&child_path, fork_ms + 1_500);
        append_total_token_line(&child_path, fork_ms + 1_600, 1_700, 1_400, 150);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        assert_eq!(snapshot.totals.last30_days_tokens, 1_850);
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 1_850);
        assert_eq!(
            snapshot.days.iter().map(|day| day.agent_runs).sum::<i64>(),
            1
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn archived_parent_unblocks_cached_fork_without_becoming_an_indexed_session() {
        let root = make_temp_dir("archived-fork-parent");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let archived_sessions_root = codex_home.join("archived_sessions").join("2026/08/06");
        let project = root.join("project");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        std::fs::create_dir_all(&project).expect("create project");

        let parent_id = "55555555-5555-4555-8555-555555555555";
        let child_id = "66666666-6666-4666-8666-666666666666";
        let child_path = sessions_root.join(format!("rollout-child-{child_id}.jsonl"));
        let parent_path = archived_sessions_root.join(format!("rollout-parent-{parent_id}.jsonl"));
        let fork_ms = Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();

        append_json_line(
            &child_path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": Utc.timestamp_millis_opt(fork_ms).single().unwrap().to_rfc3339(),
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": Utc.timestamp_millis_opt(fork_ms).single().unwrap().to_rfc3339(),
                    "cwd": project
                }
            }),
        );
        append_agent_message_line(&child_path, fork_ms + 50);
        append_total_token_line(&child_path, fork_ms + 100, 1_500, 1_300, 130);
        append_agent_message_line(&child_path, fork_ms + 1_500);
        append_total_token_line(&child_path, fork_ms + 1_600, 1_700, 1_400, 150);

        let cache_db_path = root.join("llmon.db");
        let blocked = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("blocked snapshot");
        assert_eq!(blocked.scan_total_files, 1);
        assert_eq!(blocked.scan_pending_files, 1);
        assert_eq!(blocked.totals.last30_days_tokens, 0);

        std::fs::create_dir_all(&archived_sessions_root).expect("create archived sessions root");
        append_session_meta_line(
            &parent_path,
            fork_ms - 1_000,
            &project.display().to_string(),
        );
        append_total_token_line(&parent_path, fork_ms - 100, 1_500, 1_300, 130);

        let resolved = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("resolved snapshot");
        assert_eq!(resolved.totals.last30_days_tokens, 220);
        assert_eq!(resolved.scan_total_files, 1);
        assert_eq!(resolved.scan_indexed_files, 1);
        assert_eq!(resolved.scan_pending_files, 0);
        let usage = resolved
            .project_usage_for_path(&project.display().to_string())
            .expect("child project usage");
        assert_eq!(usage.total_tokens, 220);
        assert_eq!(usage.indexed_files, 1);

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let (store, _) = load_scan_cache_store(&db, Harness::Codex).expect("load cache store");
        assert_eq!(store.entries.len(), 1, "archived parent must not be cached");
        let child_entry = store
            .entries
            .get(&child_path.to_string_lossy().to_string())
            .expect("child cache row");
        assert!(child_entry.fully_parsed);
        assert!(child_entry.file_offset > 0);
        let child_state = child_entry
            .parser_state
            .as_codex()
            .expect("codex parser state");
        assert_eq!(child_state.fork_parent_id.as_deref(), Some(parent_id));
        let baseline = child_state.fork_baseline.expect("archived parent baseline");
        assert_eq!(baseline.input, 1_500);
        assert_eq!(baseline.cached, 1_300);
        assert_eq!(baseline.output, 130);

        let cached = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("cached snapshot");
        assert_eq!(cached.totals.last30_days_tokens, 220);
        assert_eq!(cached.scan_total_files, 1);
        assert_eq!(cached.scan_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fork_replay_keeps_usage_with_immutable_session_owners_without_double_counting() {
        let root = make_temp_dir("fork-project-context");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let project = root.join("project");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        std::fs::create_dir_all(&project).expect("create project");

        let parent_id = "33333333-3333-4333-8333-333333333333";
        let child_id = "44444444-4444-4444-8444-444444444444";
        let parent_path = sessions_root.join(format!("rollout-parent-{parent_id}.jsonl"));
        let child_path = sessions_root.join(format!("rollout-child-{child_id}.jsonl"));
        let fork_ms = Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();

        append_json_line(
            &parent_path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": Utc.timestamp_millis_opt(fork_ms - 1_000).single().unwrap().to_rfc3339(),
                "payload": {
                    "id": parent_id,
                    "timestamp": Utc.timestamp_millis_opt(fork_ms - 1_000).single().unwrap().to_rfc3339(),
                    "cwd": "/outside/launcher"
                }
            }),
        );
        append_json_line(
            &parent_path,
            serde_json::json!({
                "type": "turn_context",
                "timestamp": Utc.timestamp_millis_opt(fork_ms - 200).single().unwrap().to_rfc3339(),
                "payload": {
                    "cwd": project
                }
            }),
        );
        append_total_token_line(&parent_path, fork_ms - 100, 1_500, 1_300, 130);

        append_json_line(
            &child_path,
            serde_json::json!({
                "type": "session_meta",
                "timestamp": Utc.timestamp_millis_opt(fork_ms).single().unwrap().to_rfc3339(),
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": Utc.timestamp_millis_opt(fork_ms).single().unwrap().to_rfc3339(),
                    "cwd": "/outside/launcher"
                }
            }),
        );
        // Copied parent context is operational metadata only. It must not
        // create a second project or rehome the child before it becomes live.
        append_json_line(
            &child_path,
            serde_json::json!({
                "type": "turn_context",
                "timestamp": Utc.timestamp_millis_opt(fork_ms + 20).single().unwrap().to_rfc3339(),
                "payload": {
                    "cwd": project
                }
            }),
        );
        append_total_token_line(&child_path, fork_ms + 100, 1_500, 1_300, 130);
        append_agent_message_line(&child_path, fork_ms + 1_500);
        append_total_token_line(&child_path, fork_ms + 1_600, 1_700, 1_400, 150);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        let owner_usage = snapshot
            .project_usage_for_path("/outside/launcher")
            .expect("owner usage");
        assert_eq!(snapshot.totals.last30_days_tokens, 1_850);
        assert_eq!(owner_usage.total_tokens, 1_850);
        assert_eq!(owner_usage.indexed_files, 2);
        assert!(snapshot
            .project_usage_for_path(&project.display().to_string())
            .is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cached_scanner_advances_through_backlog_across_refreshes() {
        let root = make_temp_dir("scan-backlog");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        for index in 0..3 {
            write_token_file(
                &sessions_root.join(format!("session-{index}.jsonl")),
                now_ms + index,
                100 + index,
                20,
            );
        }
        let mut limits = default_test_limits(false);
        limits.max_session_files_scanned = 1;
        let cache_db_path = root.join("llmon.db");

        let first = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(&cache_db_path),
        )
        .expect("first snapshot");
        assert_eq!(first.scan_indexed_files, 1);
        assert_eq!(first.scan_pending_files, 2);
        assert!(first.scan_processed_bytes > 0);
        let second = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(&cache_db_path),
        )
        .expect("second snapshot");
        assert_eq!(second.scan_indexed_files, 2);
        assert!(second.scan_processed_bytes > first.scan_processed_bytes);
        let third = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(&cache_db_path),
        )
        .expect("third snapshot");
        assert_eq!(third.scan_indexed_files, 3);
        assert_eq!(third.scan_pending_files, 0);
        assert!(third.scan_processed_bytes > second.scan_processed_bytes);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unresolved_partial_fork_does_not_starve_new_session() {
        let root = make_temp_dir("scan-partial-priority");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        let fork_path = sessions_root.join("forked.jsonl");
        write_forked_replay_file(&fork_path, now_ms);

        let mut limits = default_test_limits(false);
        limits.max_session_files_scanned = 1;
        let cache_db_path = root.join("llmon.db");
        let first = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(&cache_db_path),
        )
        .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 0);
        assert_eq!(first.scan_pending_files, 1);

        let normal_path = sessions_root.join("normal.jsonl");
        write_token_file(&normal_path, now_ms + 1_000, 100, 20);

        let second = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(&cache_db_path),
        )
        .expect("second snapshot");
        assert_eq!(second.totals.last30_days_tokens, 120);
        assert_eq!(second.scan_indexed_files, 1);
        assert_eq!(second.scan_pending_files, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_populates_local_and_utc_projections() {
        let root = make_temp_dir("usage-zones");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        write_token_file(&sessions_root.join("session.jsonl"), now_ms, 100, 20);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        assert_eq!(snapshot.totals.last30_days_tokens, 120);
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 120);
        assert_eq!(snapshot.days.len(), snapshot.utc_days.len());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_charts_all_indexed_days_but_keeps_thirty_day_summary() {
        let root = make_temp_dir("usage-full-chart-history");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        let old_ms = now_ms - Duration::days(45).num_milliseconds();
        write_token_file(&sessions_root.join("old.jsonl"), old_ms, 1_000, 20);
        write_token_file(&sessions_root.join("recent.jsonl"), now_ms, 100, 20);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        let old_day = Utc
            .timestamp_millis_opt(old_ms)
            .single()
            .expect("old timestamp")
            .format("%Y-%m-%d")
            .to_string();
        assert!(snapshot.utc_days.len() >= 46);
        assert_eq!(
            snapshot
                .utc_days
                .iter()
                .find(|day| day.day == old_day)
                .map(|day| day.total_tokens),
            Some(1_020)
        );
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 120);
        assert_eq!(snapshot.utc_totals.peak_day_tokens, 120);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_delayed_missing_parent_partial() {
        let root = make_temp_dir("fork-replay-delayed-metadata");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let payload_timestamp_ms =
            Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();
        let session_path = sessions_root.join("forked.jsonl");
        let outer_timestamp_ms =
            write_delayed_fork_replay_prefix(&session_path, payload_timestamp_ms, 1_500);
        append_fork_replay_live_tail(&session_path, outer_timestamp_ms);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        assert_eq!(snapshot.totals.last30_days_tokens, 0);
        assert_eq!(snapshot.scan_pending_files, 1);
        assert_eq!(
            snapshot.days.iter().map(|day| day.agent_runs).sum::<i64>(),
            0,
            "an unresolved parent must not expose compressed replay runs"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_does_not_guess_missing_parent_after_append() {
        let root = make_temp_dir("fork-replay-delayed-resume");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let payload_timestamp_ms =
            Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();
        let session_path = sessions_root.join("forked.jsonl");
        let outer_timestamp_ms =
            write_delayed_fork_replay_prefix(&session_path, payload_timestamp_ms, 1_500);
        let cache_db_path = root.join("llmon.db");

        let replay_only = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("replay-only snapshot");
        assert_eq!(replay_only.totals.last30_days_tokens, 0);
        assert_eq!(
            replay_only
                .days
                .iter()
                .map(|day| day.agent_runs)
                .sum::<i64>(),
            0
        );

        append_fork_replay_live_tail(&session_path, outer_timestamp_ms);
        let resumed = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("resumed snapshot");
        assert_eq!(resumed.totals.last30_days_tokens, 0);
        assert_eq!(resumed.scan_pending_files, 1);
        assert_eq!(
            resumed.days.iter().map(|day| day.agent_runs).sum::<i64>(),
            0
        );

        let cached = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("cached resumed snapshot");
        assert_eq!(cached.totals.last30_days_tokens, 0);
        assert_eq!(cached.scan_pending_files, 1);
        assert_eq!(cached.days.iter().map(|day| day.agent_runs).sum::<i64>(), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_builds_project_activity_sorted_by_last_activity() {
        let root = make_temp_dir("project-activity");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let older_ms = now_ms - Duration::days(5).num_milliseconds();
        let newer_ms = now_ms - Duration::days(1).num_milliseconds();

        let starling = sessions_root.join("starling.jsonl");
        append_session_meta_line(&starling, older_ms, "/outside/Starling");
        append_total_token_line(&starling, older_ms + 100, 100, 0, 20);

        let sfm = sessions_root.join("sfm.jsonl");
        append_session_meta_line(&sfm, newer_ms, "/outside/SFM");
        append_total_token_line(&sfm, newer_ms + 100, 200, 150, 50);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");

        assert_eq!(snapshot.project_activity.len(), 2);
        assert_eq!(
            snapshot.project_activity[0].days.len(),
            ACTIVITY_TIMELINE_DAYS
        );
        assert_eq!(snapshot.project_activity[0].display_path, "/outside/SFM");
        assert_eq!(snapshot.project_activity[0].total_tokens, 250);
        assert_eq!(snapshot.project_activity[0].cache_read_tokens, 150);
        assert_eq!(
            snapshot.project_activity[1].display_path,
            "/outside/Starling"
        );
        assert_eq!(snapshot.project_activity[1].total_tokens, 120);
        assert_eq!(snapshot.project_activity[1].cache_read_tokens, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_permission_granted_external_access_with_session_project() {
        let root = make_temp_dir("project-permission-owner");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let project = root.join("main-project");
        let external = root.join("external-project");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        std::fs::create_dir_all(&project).expect("create main project dir");
        std::fs::create_dir_all(&external).expect("create external project dir");

        let now_ms = Utc::now().timestamp_millis();
        let session = sessions_root.join("session.jsonl");
        append_session_meta_line(&session, now_ms, &project.display().to_string());
        append_json_line(
            &session,
            serde_json::json!({
                "type": "turn_context",
                "payload": {
                    "cwd": project,
                    "permission_profile": {
                        "file_system": {
                            "entries": [{"path": external, "access": "write"}]
                        }
                    }
                }
            }),
        );
        append_json_line(
            &session,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": serde_json::json!({
                        "workdir": external,
                        "cmd": format!("sed -n '1,20p' {}/CMakeLists.txt", external.display()),
                        "sandbox_permissions": "require_escalated"
                    }).to_string()
                }
            }),
        );
        append_total_token_line(&session, now_ms + 100, 100, 25, 20);

        let cache_db_path = root.join("llmon.db");
        let first = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("first snapshot");
        let second = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("cached snapshot");
        let summary = second
            .project_usage_for_path(&project.display().to_string())
            .expect("project usage");
        assert_eq!(summary.display_path, project.display().to_string());
        assert_eq!(summary.total_tokens, 120);
        assert_eq!(summary.cache_read_tokens, 25);
        assert_eq!(summary.indexed_files, 1);
        assert_eq!(second.project_usage.len(), 1);
        assert!(second
            .project_usage_for_path(&external.display().to_string())
            .is_none());
        assert_eq!(
            first.totals.last30_days_tokens,
            second.totals.last30_days_tokens
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_all_usage_with_immutable_session_owner() {
        let root = make_temp_dir("project-context-transition");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let project_a = root.join("project-a");
        let project_b = root.join("project-b");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        std::fs::create_dir_all(&project_a).expect("create project a");
        std::fs::create_dir_all(&project_b).expect("create project b");

        let now_ms = Utc::now().timestamp_millis();
        let session = sessions_root.join("session.jsonl");
        append_session_meta_line(&session, now_ms, &project_a.display().to_string());
        for (project, timestamp_ms, input, cached, output) in [
            (&project_a, now_ms + 100, 100, 25, 20),
            (&project_b, now_ms + 200, 160, 35, 30),
        ] {
            append_json_line(
                &session,
                serde_json::json!({
                    "type": "turn_context",
                    "timestamp": Utc
                        .timestamp_millis_opt(timestamp_ms - 1)
                        .single()
                        .expect("timestamp")
                        .to_rfc3339(),
                    "payload": {
                        "cwd": project
                    }
                }),
            );
            append_total_token_line(&session, timestamp_ms, input, cached, output);
        }

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        let a = snapshot
            .project_usage_for_path(&project_a.display().to_string())
            .expect("project a usage");
        assert_eq!(a.total_tokens, 190);
        assert_eq!(a.cache_read_tokens, 35);
        assert_eq!(a.indexed_files, 1);
        assert!(snapshot
            .project_usage_for_path(&project_b.display().to_string())
            .is_none());
        assert_eq!(snapshot.totals.last30_days_tokens, 190);

        let filtered_a = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            Some(&project_a),
            default_test_limits(false),
            None,
        )
        .expect("filtered project a snapshot");
        assert_eq!(filtered_a.totals.last30_days_tokens, 190);
        assert_eq!(filtered_a.matched_session_files, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_late_settings_and_all_tokens_with_session_owner() {
        let root = make_temp_dir("late-settings-owner");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let project = root.join("Lantern");
        let external = root.join("nativefiledialog-extended");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        let session = sessions_root.join("session.jsonl");
        append_session_meta_line(&session, now_ms, &project.display().to_string());
        append_total_token_line(&session, now_ms + 100, 100, 25, 20);
        for _ in 0..130 {
            append_json_line(
                &session,
                serde_json::json!({"type": "event_msg", "payload": {"type": "noop"}}),
            );
        }
        append_json_line(
            &session,
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_settings": {"cwd": external}
                }
            }),
        );
        append_total_token_line(&session, now_ms + 200, 160, 35, 30);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        let owner = snapshot
            .project_usage_for_path(&project.display().to_string())
            .expect("owner usage");
        assert_eq!(owner.total_tokens, 190);
        assert_eq!(owner.cache_read_tokens, 35);
        assert!(snapshot
            .project_usage_for_path(&external.display().to_string())
            .is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_recovers_late_meta_before_attributing_early_tokens() {
        let root = make_temp_dir("late-meta-usage");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let project = root.join("Lantern");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        let session = sessions_root.join("session.jsonl");
        append_json_line(
            &session,
            serde_json::json!({"type": "turn_context", "payload": {"cwd": root.join("fallback")}}),
        );
        append_total_token_line(&session, now_ms + 100, 100, 25, 20);
        for _ in 0..128 {
            append_json_line(
                &session,
                serde_json::json!({"type": "event_msg", "payload": {"type": "noop"}}),
            );
        }
        append_session_meta_line(&session, now_ms + 200, &project.display().to_string());
        append_total_token_line(&session, now_ms + 300, 160, 35, 30);

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            None,
        )
        .expect("snapshot");
        let owner = snapshot
            .project_usage_for_path(&project.display().to_string())
            .expect("recovered owner usage");
        assert_eq!(owner.total_tokens, 190);
        assert!(snapshot
            .project_usage_for_path(&root.join("fallback").display().to_string())
            .is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cached_unresolved_session_reparses_when_an_owner_is_appended() {
        let root = make_temp_dir("unresolved-owner-cache");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        let project = root.join("Lantern");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let now_ms = Utc::now().timestamp_millis();
        let session = sessions_root.join("session.jsonl");
        write_token_file(&session, now_ms, 100, 20);
        let cache_db_path = root.join("llmon.db");

        let first = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 120);
        assert!(first.project_usage.is_empty());

        append_session_meta_line(&session, now_ms + 1, &project.display().to_string());
        let second = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(&cache_db_path),
        )
        .expect("second snapshot");
        let owner = second
            .project_usage_for_path(&project.display().to_string())
            .expect("replayed owner usage");
        assert_eq!(owner.total_tokens, 120);
        assert_eq!(owner.indexed_files, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_cached_totals_for_unplanned_unchanged_files() {
        let root = make_temp_dir("cache-unplanned");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let older_ms = now_ms - Duration::days(20).num_milliseconds();
        let newer_ms = now_ms - Duration::days(5).num_milliseconds();

        let older_path = sessions_root.join("older.jsonl");
        let newer_path = sessions_root.join("newer.jsonl");
        write_token_file(&older_path, older_ms, 100, 25);
        write_token_file(&newer_path, newer_ms, 80, 20);

        let cache_db_path = root.join("llmon.db");
        let warm_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: true,
            scan_cache_max_entries: 1000,
        };
        let warmed = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            warm_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("warm snapshot");
        assert_eq!(warmed.totals.last30_days_tokens, 225);

        let restrictive_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 1,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let restricted = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            restrictive_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("restricted snapshot");
        assert_eq!(
            restricted.totals.last30_days_tokens, warmed.totals.last30_days_tokens,
            "unchanged files outside current scan plan should still contribute via cache"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    fn write_owned_session(path: &Path, timestamp_ms: i64, cwd: &str, input: i64, output: i64) {
        let _ = std::fs::remove_file(path);
        append_session_meta_line(path, timestamp_ms, cwd);
        append_total_token_line(path, timestamp_ms + 1_000, input, 0, output);
    }

    fn archived_row_count(cache_db_path: &Path) -> i64 {
        let conn = Connection::open(cache_db_path.with_file_name(USAGE_ARCHIVE_DB_FILE_NAME))
            .expect("open usage archive");
        conn.query_row("SELECT COUNT(*) FROM archived_usage;", [], |row| row.get(0))
            .expect("count archived rows")
    }

    #[test]
    fn deleted_log_keeps_counting_from_the_usage_archive() {
        let root = make_temp_dir("archive-deleted-log");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        let session_ms = now_ms - Duration::hours(2).num_milliseconds();
        let kept = sessions_root.join("kept.jsonl");
        let deleted = sessions_root.join("deleted.jsonl");
        write_owned_session(&kept, session_ms, "/outside/Lantern", 100, 20);
        write_owned_session(&deleted, session_ms, "/outside/Starling", 300, 40);
        let snapshot = || {
            compute_snapshot(
                Harness::Codex,
                30,
                &codex_home,
                None,
                default_test_limits(false),
                Some(cache_db_path.as_path()),
            )
            .expect("snapshot")
        };
        let project_total = |snapshot: &LocalUsageSnapshot, path: &str| {
            snapshot
                .project_usage_for_path(path)
                .map(|project| project.total_tokens)
        };

        let first = snapshot();
        assert_eq!(first.totals.last30_days_tokens, 460);
        assert_eq!(archived_row_count(&cache_db_path), 0);

        std::fs::remove_file(&deleted).expect("delete log");
        let after_delete = snapshot();
        assert_eq!(after_delete.totals.last30_days_tokens, 460);
        assert_eq!(project_total(&after_delete, "/outside/Starling"), Some(340));
        assert_eq!(after_delete.scan_total_files, 1);
        assert_eq!(archived_row_count(&cache_db_path), 1);
        assert_eq!(cache_row_count(&cache_db_path, "codex"), 1);

        // A parser change rebuilds only the scan cache.
        {
            let conn = Connection::open(&cache_db_path).expect("open raw cache db");
            conn.execute(
                "UPDATE cache_meta SET value = 0 WHERE key = 'schema_version.codex';",
                [],
            )
            .expect("age codex schema version");
        }
        assert_eq!(snapshot().totals.last30_days_tokens, 460);

        // So does deleting the cache files (--rebuild-cache-on-start).
        for suffix in ["", "-wal", "-shm"] {
            let mut path = cache_db_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(path));
        }
        assert_eq!(snapshot().totals.last30_days_tokens, 460);

        // A restored log counts once, from the scan cache.
        write_owned_session(&deleted, session_ms, "/outside/Starling", 300, 40);
        let restored = snapshot();
        assert_eq!(restored.totals.last30_days_tokens, 460);
        assert_eq!(project_total(&restored, "/outside/Starling"), Some(340));
        assert_eq!(archived_row_count(&cache_db_path), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_rows_of_another_home_are_kept_but_not_counted() {
        let root = make_temp_dir("archive-other-home");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        let session_ms = now_ms - Duration::hours(2).num_milliseconds();
        let mut totals = Vec::new();
        for (name, input) in [("first", 100), ("second", 300)] {
            let home = root.join(name);
            let sessions_root = home.join("sessions");
            std::fs::create_dir_all(&sessions_root).expect("create sessions root");
            write_owned_session(
                &sessions_root.join("session.jsonl"),
                session_ms,
                "/outside/Lantern",
                input,
                0,
            );
            let snapshot = compute_snapshot(
                Harness::Codex,
                30,
                &home,
                None,
                default_test_limits(false),
                Some(cache_db_path.as_path()),
            )
            .expect("snapshot");
            totals.push(snapshot.totals.last30_days_tokens);
        }

        assert_eq!(totals, vec![100, 300]);
        assert_eq!(cache_row_count(&cache_db_path, "codex"), 2);
        assert_eq!(archived_row_count(&cache_db_path), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_partial_last_line_is_left_for_the_next_refresh() {
        let root = make_temp_dir("codex-partial-line");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        let session_ms = now_ms - Duration::hours(1).num_milliseconds();
        let path = sessions_root.join("session.jsonl");
        append_session_meta_line(&path, session_ms, "/outside/Lantern");
        append_total_token_line(&path, session_ms + 1_000, 100, 0, 20);
        let message = serde_json::json!({
            "type": "event_msg",
            "timestamp": Utc
                .timestamp_millis_opt(session_ms + 2_000)
                .single()
                .expect("valid timestamp")
                .to_rfc3339(),
            "payload": {"type": "agent_message", "message": "done"}
        })
        .to_string();
        let (head, tail) = message.split_at(message.len() / 2);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open for partial write");
            write!(file, "{head}").expect("write partial line");
        }
        let snapshot = || {
            compute_snapshot(
                Harness::Codex,
                30,
                &codex_home,
                None,
                default_test_limits(false),
                Some(cache_db_path.as_path()),
            )
            .expect("snapshot")
        };
        let runs = |snapshot: &LocalUsageSnapshot| -> i64 {
            snapshot.days.iter().map(|day| day.agent_runs).sum()
        };

        let first = snapshot();
        assert_eq!(first.totals.last30_days_tokens, 120);
        assert_eq!(runs(&first), 0);
        assert_eq!(first.scan_pending_files, 1);

        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open to finish line");
            writeln!(file, "{tail}").expect("finish partial line");
        }
        let second = snapshot();
        assert_eq!(second.totals.last30_days_tokens, 120);
        assert_eq!(runs(&second), 1);
        assert_eq!(second.scan_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_settled_cut_off_tail_does_not_stay_pending() {
        let root = make_temp_dir("codex-cut-off-tail");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let cache_db_path = root.join("llmon.db");
        let now_ms = Utc::now().timestamp_millis();
        let session_ms = now_ms - Duration::hours(2).num_milliseconds();
        let path = sessions_root.join("session.jsonl");
        append_session_meta_line(&path, session_ms, "/outside/Lantern");
        append_total_token_line(&path, session_ms + 1_000, 100, 0, 20);
        {
            // A crash can leave NUL padding with no newline at the end.
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open for padding");
            file.write_all(&[0_u8; 848]).expect("write padding");
            file.set_modified(SystemTime::now() - StdDuration::from_secs(60 * 60))
                .expect("age the log");
        }

        let snapshot = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            default_test_limits(false),
            Some(cache_db_path.as_path()),
        )
        .expect("snapshot");
        assert_eq!(snapshot.totals.last30_days_tokens, 120);
        assert_eq!(snapshot.scan_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_display_names_are_short() {
        assert_eq!(model_display_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_display_name("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_display_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_display_name("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(model_display_name("gpt-5.5"), "gpt-5.5");
    }

    #[test]
    fn compute_snapshot_resumes_from_cached_offset_after_append() {
        let root = make_temp_dir("cache-append-resume");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("session.jsonl");
        write_token_file(
            &session_path,
            now_ms - Duration::hours(2).num_milliseconds(),
            100,
            20,
        );

        let cache_db_path = root.join("llmon.db");
        let limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 4 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };

        let first = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(cache_db_path.as_path()),
        )
        .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 120);

        append_token_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
            40,
            10,
        );

        let second = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(cache_db_path.as_path()),
        )
        .expect("second snapshot");
        assert_eq!(second.totals.last30_days_tokens, 170);

        let third = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            limits,
            Some(cache_db_path.as_path()),
        )
        .expect("third snapshot");
        assert_eq!(
            third.totals.last30_days_tokens, 170,
            "unchanged file should not double-count appended usage after resume"
        );

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let (store, _) = load_scan_cache_store(&db, Harness::Codex).expect("load cache store");
        let key = session_path.to_string_lossy().to_string();
        let entry = store
            .entries
            .get(&key)
            .expect("cache row for appended session");
        assert!(entry.file_offset > 0);
        assert!(entry.fully_parsed);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_full_scan_ignores_file_and_byte_scan_caps() {
        let root = make_temp_dir("full-scan-caps");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let files = [
            (
                "a.jsonl",
                now_ms - Duration::days(3).num_milliseconds(),
                90,
                10,
            ),
            (
                "b.jsonl",
                now_ms - Duration::days(2).num_milliseconds(),
                70,
                5,
            ),
            (
                "c.jsonl",
                now_ms - Duration::days(1).num_milliseconds(),
                40,
                15,
            ),
        ];
        let mut expected_total = 0_i64;
        for (name, ts, input, output) in files {
            write_token_file(&sessions_root.join(name), ts, input, output);
            expected_total += input + output;
        }

        let capped_limits = ScanLimits {
            max_session_file_bytes: 1,
            max_session_total_bytes: 1,
            max_session_files_scanned: 1,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let capped = compute_snapshot(Harness::Codex, 30, &codex_home, None, capped_limits, None)
            .expect("capped snapshot");
        assert!(
            capped.totals.last30_days_tokens < expected_total,
            "planner caps should leave some files unscanned in non-full mode"
        );

        let uncapped_limits = ScanLimits {
            full_scan: true,
            ..capped_limits
        };
        let full = compute_snapshot(Harness::Codex, 30, &codex_home, None, uncapped_limits, None)
            .expect("full snapshot");
        assert_eq!(
            full.totals.last30_days_tokens, expected_total,
            "full scan should include all files even when scan caps are tiny"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_full_scan_reparses_when_cache_row_is_stale() {
        let root = make_temp_dir("full-scan-reparse");
        let codex_home = root.join("codex");
        let sessions_root = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("session.jsonl");
        write_token_file(
            &session_path,
            now_ms - Duration::hours(6).num_milliseconds(),
            120,
            30,
        );
        let expected_total = 150_i64;

        let cache_db_path = root.join("llmon.db");
        let baseline_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 4 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let baseline = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            baseline_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("baseline snapshot");
        assert_eq!(baseline.totals.last30_days_tokens, expected_total);

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let file_key = session_path.to_string_lossy().to_string();
        db.conn
            .execute(
                "UPDATE file_cache SET daily_json='{}', model_daily_json='{}' WHERE file_path = ?1;",
                rusqlite::params![file_key],
            )
            .expect("corrupt cache row");
        drop(db);

        let stale = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            baseline_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("stale snapshot");
        assert_eq!(
            stale.totals.last30_days_tokens, 0,
            "non-full scan should still trust unchanged cache rows"
        );

        let full_limits = ScanLimits {
            full_scan: true,
            ..baseline_limits
        };
        let repaired = compute_snapshot(
            Harness::Codex,
            30,
            &codex_home,
            None,
            full_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("repaired snapshot");
        assert_eq!(
            repaired.totals.last30_days_tokens, expected_total,
            "full scan with unlimited time should reparse files and repair stale cache rows"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prune_scan_cache_db_removes_stale_entries() {
        let root = make_temp_dir("cache-prune");
        let sessions_root = root.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let db_path = root.join("llmon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");

        let keep_path = sessions_root.join("keep.jsonl");
        std::fs::write(&keep_path, b"{}\n").expect("write keep jsonl");

        let wrong_ext_path = sessions_root.join("not_jsonl.txt");
        std::fs::write(&wrong_ext_path, b"ignored").expect("write txt");

        let outside_root = make_temp_dir("outside");
        let outside_path = outside_root.join("outside.jsonl");
        std::fs::write(&outside_path, b"{}\n").expect("write outside jsonl");

        let missing_path = sessions_root.join("missing.jsonl");

        let keep_key = keep_path.to_string_lossy().to_string();
        let wrong_ext_key = wrong_ext_path.to_string_lossy().to_string();
        let outside_key = outside_path.to_string_lossy().to_string();
        let missing_key = missing_path.to_string_lossy().to_string();

        let base = CachedFileScanEntry {
            size: 4,
            modified_epoch_secs: Some(1),
            file_offset: 4,
            fully_parsed: true,
            session_cwd: None,
            parser_state: HarnessParserState::Codex(ParserState::default()),
            daily: HashMap::new(),
            model_totals_by_day: HashMap::new(),
            updated_at: 1,
        };
        let mut initial_store = ScanCacheStore::default();
        initial_store.entries.insert(
            keep_key.clone(),
            CachedFileScanEntry {
                updated_at: 5,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            wrong_ext_key.clone(),
            CachedFileScanEntry {
                updated_at: 4,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            outside_key.clone(),
            CachedFileScanEntry {
                updated_at: 3,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            missing_key.clone(),
            CachedFileScanEntry {
                updated_at: 2,
                ..base
            },
        );
        let dirty_paths: HashSet<String> = initial_store.entries.keys().cloned().collect();
        persist_scan_cache_changes(
            &mut db,
            Harness::Codex,
            &initial_store,
            &HashSet::new(),
            &dirty_paths,
        )
        .expect("persist initial rows");

        let (mut loaded_store, mut removed_paths) =
            load_scan_cache_store(&db, Harness::Codex).expect("load store");
        let pruned = prune_scan_cache_store(
            &mut loaded_store,
            &sessions_root,
            1024,
            100,
            &mut removed_paths,
        );
        assert!(pruned, "expected stale rows to be removed");
        persist_scan_cache_changes(
            &mut db,
            Harness::Codex,
            &loaded_store,
            &removed_paths,
            &HashSet::new(),
        )
        .expect("persist pruned rows");

        let (reloaded_store, _) = load_scan_cache_store(&db, Harness::Codex).expect("reload store");
        assert_eq!(reloaded_store.entries.len(), 1);
        assert!(reloaded_store.entries.contains_key(&keep_key));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    fn cache_row_count(db_path: &Path, harness: &str) -> i64 {
        let conn = Connection::open(db_path).expect("open raw cache db");
        conn.query_row(
            "SELECT COUNT(*) FROM file_cache WHERE harness = ?1;",
            params![harness],
            |row| row.get(0),
        )
        .expect("count rows")
    }

    #[test]
    fn harness_schema_version_change_clears_only_that_harness() {
        let root = make_temp_dir("cache-harness-version");
        let db_path = root.join("llmon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");
        let mut store = ScanCacheStore::default();
        store.entries.insert(
            "/sessions/a.jsonl".to_string(),
            CachedFileScanEntry {
                size: 1,
                modified_epoch_secs: Some(1),
                file_offset: 1,
                fully_parsed: true,
                session_cwd: None,
                parser_state: HarnessParserState::Codex(ParserState::default()),
                daily: HashMap::new(),
                model_totals_by_day: HashMap::new(),
                updated_at: 1,
            },
        );
        let dirty_paths: HashSet<String> = store.entries.keys().cloned().collect();
        persist_scan_cache_changes(
            &mut db,
            Harness::Codex,
            &store,
            &HashSet::new(),
            &dirty_paths,
        )
        .expect("persist codex row");
        drop(db);
        {
            let conn = Connection::open(&db_path).expect("open raw cache db");
            conn.execute(
                "INSERT INTO file_cache(harness, file_path, file_size, daily_json, model_daily_json, updated_at)
                 VALUES('future', '/other/b.jsonl', 1, '{}', '{}', 1);",
                [],
            )
            .expect("insert row of another harness");
            conn.execute(
                "UPDATE cache_meta SET value = 0 WHERE key = 'schema_version.codex';",
                [],
            )
            .expect("age codex schema version");
        }

        let db = open_or_init_scan_cache_db(&db_path).expect("reopen cache db");
        let stored: i64 = db
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'schema_version.codex';",
                [],
                |row| row.get(0),
            )
            .expect("read codex schema version");
        assert_eq!(stored, CODEX_CACHE_SCHEMA_VERSION);
        drop(db);
        assert_eq!(cache_row_count(&db_path, "codex"), 0);
        assert_eq!(cache_row_count(&db_path, "future"), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_without_layout_version_is_reset_to_the_current_layout() {
        let root = make_temp_dir("cache-legacy-layout");
        let db_path = root.join("llmon.db");
        {
            let conn = Connection::open(&db_path).expect("create legacy cache db");
            conn.execute_batch(
                "
                CREATE TABLE cache_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
                INSERT INTO cache_meta(key, value) VALUES('schema_version', 14);
                CREATE TABLE file_cache (
                    file_path TEXT PRIMARY KEY,
                    file_size INTEGER NOT NULL,
                    daily_json TEXT NOT NULL,
                    model_daily_json TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO file_cache VALUES('/sessions/old.jsonl', 1, '{}', '{}', 1);
                ",
            )
            .expect("write legacy layout");
        }

        let db = open_or_init_scan_cache_db(&db_path).expect("open legacy cache db");
        let (store, _) = load_scan_cache_store(&db, Harness::Codex).expect("load store");
        assert!(store.entries.is_empty());
        let legacy_key: Option<i64> = db
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'schema_version';",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("read legacy key");
        assert_eq!(legacy_key, None);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_with_unknown_layout_version_is_rejected() {
        let root = make_temp_dir("cache-future-layout");
        let db_path = root.join("llmon.db");
        drop(open_or_init_scan_cache_db(&db_path).expect("create cache db"));
        {
            let conn = Connection::open(&db_path).expect("open raw cache db");
            conn.execute(
                "UPDATE cache_meta SET value = 99 WHERE key = 'layout_version';",
                [],
            )
            .expect("set future layout version");
        }

        let error = open_or_init_scan_cache_db(&db_path).expect_err("future layout must fail");
        assert!(format!("{error:#}").contains("layout version"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn trim_scan_cache_db_to_limit_drops_oldest_entries() {
        let root = make_temp_dir("cache-trim");
        let db_path = root.join("llmon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");

        let mut store = ScanCacheStore::default();
        for (path, updated_at) in [("a", 30_i64), ("b", 10_i64), ("c", 20_i64)] {
            store.entries.insert(
                path.to_string(),
                CachedFileScanEntry {
                    size: 1,
                    modified_epoch_secs: Some(1),
                    file_offset: 1,
                    fully_parsed: true,
                    session_cwd: None,
                    parser_state: HarnessParserState::Codex(ParserState::default()),
                    daily: HashMap::new(),
                    model_totals_by_day: HashMap::new(),
                    updated_at,
                },
            );
        }
        let dirty_paths: HashSet<String> = store.entries.keys().cloned().collect();
        persist_scan_cache_changes(
            &mut db,
            Harness::Codex,
            &store,
            &HashSet::new(),
            &dirty_paths,
        )
        .expect("persist initial rows");

        let (mut loaded_store, mut removed_paths) =
            load_scan_cache_store(&db, Harness::Codex).expect("load store");
        let pruned = trim_scan_cache_to_limit(&mut loaded_store, 2, &mut removed_paths);
        assert!(pruned, "expected trim to remove one row");
        persist_scan_cache_changes(
            &mut db,
            Harness::Codex,
            &loaded_store,
            &removed_paths,
            &HashSet::new(),
        )
        .expect("persist trimmed rows");

        let (reloaded_store, _) = load_scan_cache_store(&db, Harness::Codex).expect("reload store");
        let mut paths: Vec<String> = reloaded_store.entries.keys().cloned().collect();
        paths.sort();
        assert_eq!(paths, vec!["a".to_string(), "c".to_string()]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_keys_unify_wsl_unc_and_linux_paths() {
        assert_eq!(
            normalize_project_key(r"\\wsl.localhost\Ubuntu\home\user\demo-project\"),
            normalize_project_key("/home/user/demo-project")
        );
        assert_eq!(
            normalize_project_key(r"\\wsl$\Ubuntu\home\user\demo-project"),
            "/home/user/demo-project"
        );
    }

    #[test]
    fn owner_resolver_ignores_thread_settings_cwd() {
        let root = make_temp_dir("thread-settings-context");
        let project = root.join("project");
        let child = project.join("src");
        std::fs::create_dir_all(&child).expect("create child");
        let session = root.join("session.jsonl");
        append_session_meta_line(
            &session,
            Utc::now().timestamp_millis(),
            &project.display().to_string(),
        );
        append_json_line(
            &session,
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_settings": {"cwd": child}
                }
            }),
        );

        let owner = resolve_session_owner(&session)
            .expect("resolve owner")
            .expect("session owner");
        assert_eq!(
            normalize_project_key(&owner.cwd),
            normalize_project_key(&project.display().to_string())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owner_resolver_recovers_matching_late_meta_over_turn_context_fallback() {
        let root = make_temp_dir("late-meta-owner");
        let expected_id = "01234567-89ab-cdef-0123-456789abcdef";
        let session = root.join(format!("rollout-2026-07-29T00-00-00-{expected_id}.jsonl"));
        append_json_line(
            &session,
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": "ffffffff-ffff-ffff-ffff-ffffffffffff", "cwd": "/outside/parent"}
            }),
        );
        append_json_line(
            &session,
            serde_json::json!({"type": "turn_context", "payload": {"cwd": "/outside/fallback"}}),
        );
        for _ in 0..128 {
            append_json_line(
                &session,
                serde_json::json!({"type": "event_msg", "payload": {"type": "noop"}}),
            );
        }
        append_json_line(
            &session,
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": expected_id, "cwd": "/outside/Lantern"}
            }),
        );

        let owner = resolve_session_owner(&session)
            .expect("resolve owner")
            .expect("recovered owner");
        assert_eq!(owner.cwd, "/outside/Lantern");
        assert_eq!(owner.source, SessionOwnerSource::SessionMeta);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owner_resolver_uses_first_turn_context_only_after_eof() {
        let root = make_temp_dir("turn-context-owner");
        let project = root.join("project");
        let external = root.join("external");
        let session = root.join("session.jsonl");
        append_json_line(
            &session,
            serde_json::json!({"type": "turn_context", "payload": {"cwd": project}}),
        );
        append_json_line(
            &session,
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_settings": {"cwd": external}
                }
            }),
        );

        let owner = resolve_session_owner(&session)
            .expect("resolve owner")
            .expect("fallback owner");
        assert_eq!(owner.cwd, project.display().to_string());
        assert_eq!(owner.source, SessionOwnerSource::TurnContextFallback);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tool_calls_and_command_paths_do_not_establish_an_owner() {
        let root = make_temp_dir("tool-reference-context");
        let external = root.join("external-project");
        std::fs::create_dir_all(&external).expect("create external project");
        let session = root.join("session.jsonl");
        append_json_line(
            &session,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "shell_command",
                    "arguments": serde_json::json!({
                        "workdir": external,
                        "cmd": format!("sed -n '1,20p' {}/CMakeLists.txt", external.display()),
                        "sandbox_permissions": "require_escalated"
                    }).to_string()
                }
            }),
        );

        assert!(resolve_session_owner(&session)
            .expect("resolve owner")
            .is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_cwd_identity_does_not_walk_filesystem_for_git() {
        let root = make_temp_dir("no-git-walk");
        let project = root.join("repo");
        let nested = project.join("src").join("crate");
        std::fs::create_dir_all(project.join(".git")).expect("create git dir");
        std::fs::create_dir_all(&nested).expect("create nested");

        let identity =
            session_cwd_identity(&nested.display().to_string()).expect("identity from nested cwd");
        assert_eq!(
            normalize_project_key(&identity),
            normalize_project_key(&nested.display().to_string())
        );
        assert_ne!(
            normalize_project_key(&identity),
            normalize_project_key(&project.display().to_string())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_cwd_identity_rejects_relative_paths() {
        assert!(session_cwd_identity("relative/project").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn windows_drive_path_maps_to_wsl_mount() {
        assert_eq!(
            normalize_cross_platform_path(r"C:\Users\user\project"),
            Some(PathBuf::from("/mnt/c/Users/user/project"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_filter_matches_normalized_windows_session_cwd() {
        assert!(path_matches_workspace(
            r"C:\Users\user\project\src",
            Path::new("/mnt/c/Users/user/project")
        ));
        assert!(!path_matches_workspace(
            r"C:\Users\user\project-other",
            Path::new("/mnt/c/Users/user/project")
        ));
    }
}
