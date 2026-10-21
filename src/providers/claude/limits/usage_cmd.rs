//! Fallback source for the Claude Code limits: `claude -p /usage`.
//!
//! The status-line snapshot only changes while Claude Code runs, so after a
//! long pause it shows old numbers. Then llmon asks Claude Code itself for the
//! limits, at most once per poll interval, and stops asking as soon as a fresh
//! status-line snapshot arrives. `/usage` is a local command: it costs no
//! tokens, and Claude Code makes the one request to Anthropic with its own
//! login, so llmon reads no credentials.
//!
//! In print mode with `--output-format json --verbose`, Claude Code emits the
//! events of the run; the `/usage` one carries a `usage_report` with a
//! `rate_limits` object in the same `limits[]` shape as the OAuth endpoint
//! (field notes in `docs/sources.md`). The report is undocumented, so a
//! missing field is an error, not a panic. Without verbose mode, which is the
//! `verbose` setting of the user's Claude Code and off by default, the output
//! is only the result event: the text of the command and no report. llmon asks
//! for `--verbose` so the setting does not matter; a build that prints no
//! report even so gets the `Current session` and `Current week` lines of that
//! text read instead (`parse_text`). The report always wins when both are
//! there.
//!
//! Flags keep the call light and free of side effects: `--no-session-persistence`
//! leaves no transcript for the usage scan to count, and `--safe-mode` skips
//! the user's hooks, MCP servers, and plugins.

use super::{
    oauth, unix_now, AccountRateLimits, LimitsSourceKind, RateLimitSnapshot, RateLimitWindow,
    FIVE_HOUR_WINDOW_MINS, SEVEN_DAY_WINDOW_MINS,
};
use anyhow::{bail, Context, Result};
use chrono::{Datelike, Local, TimeZone};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_POLL_SECS: u64 = 600;
/// Every poll is a request to Anthropic made with the user's login, so the
/// interval has a floor.
pub const MIN_POLL_SECS: u64 = 300;
/// A status-line snapshot older than this is stale: the card flags its age and
/// the fallback starts polling.
pub const STALE_SNAPSHOT_SECS: i64 = 15 * 60;
/// Claude Code starts in a few seconds; this only bounds a hung process.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 8 * 1024;
const MAX_ERROR_CHARS: usize = 100;

/// The poll interval for a configured number of seconds; `None` (0) turns the
/// fallback off.
pub fn poll_interval(configured_secs: u64) -> Option<Duration> {
    (configured_secs > 0).then(|| Duration::from_secs(configured_secs.max(MIN_POLL_SECS)))
}

/// Whether a snapshot is recent enough to show without asking Claude Code.
pub fn is_fresh(limits: &AccountRateLimits, now: i64) -> bool {
    now.saturating_sub(limits.captured_at) <= STALE_SNAPSHOT_SECS
}

/// What to show: a fresh status-line snapshot, else the newer of the stale
/// snapshot and the last poll.
pub fn pick(
    status: Option<AccountRateLimits>,
    polled: Option<&AccountRateLimits>,
    now: i64,
) -> Option<AccountRateLimits> {
    match (status, polled) {
        (Some(status), _) if is_fresh(&status, now) => Some(status),
        (status, Some(polled))
            if status
                .as_ref()
                .is_none_or(|status| polled.captured_at >= status.captured_at) =>
        {
            Some(polled.clone().without_expired(now))
        }
        (status, _) => status,
    }
}

/// Polling state of the limits worker.
pub struct Fallback {
    bin: String,
    cwd: PathBuf,
    interval: Duration,
    last_attempt: Option<Instant>,
    polled: Option<AccountRateLimits>,
    error: Option<String>,
    reported_error: Option<String>,
}

/// The result of one worker tick.
pub struct Tick {
    /// What to show; the status-line read's own error when there is nothing
    /// else to show.
    pub limits: Result<Option<AccountRateLimits>>,
    /// The poll's error when it changed since the last tick (`Some(None)` when
    /// it was cleared).
    pub error: Option<Option<String>>,
}

impl Fallback {
    /// `cwd` is where Claude Code runs: a directory of llmon's own, so no
    /// project settings apply.
    pub fn new(bin: String, cwd: PathBuf, interval: Duration) -> Self {
        Self {
            bin,
            cwd,
            interval,
            last_attempt: None,
            polled: None,
            error: None,
            reported_error: None,
        }
    }

    /// Takes the status-line read of this tick and, if that snapshot is stale
    /// and a poll is due, asks Claude Code (`fetch`). A fresh snapshot pauses
    /// the polling.
    pub fn tick(
        &mut self,
        status: Result<Option<AccountRateLimits>>,
        now_unix: i64,
        now: Instant,
        fetch: impl FnOnce(&str, &Path) -> Result<AccountRateLimits>,
    ) -> Tick {
        let fresh = status
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|status| is_fresh(status, now_unix));
        if fresh {
            self.error = None;
        } else if self
            .last_attempt
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval)
        {
            self.last_attempt = Some(now);
            match fetch(&self.bin, &self.cwd) {
                Ok(limits) => {
                    self.polled = Some(limits);
                    self.error = None;
                }
                Err(error) => self.error = Some(short_message(&error)),
            }
        }
        let limits = match status {
            Ok(status) => Ok(pick(status, self.polled.as_ref(), now_unix)),
            Err(error) => match self.polled.as_ref() {
                Some(polled) => Ok(Some(polled.clone().without_expired(now_unix))),
                None => Err(error),
            },
        };
        let error = (self.error != self.reported_error).then(|| {
            self.reported_error = self.error.clone();
            self.error.clone()
        });
        Tick { limits, error }
    }
}

fn short_message(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_string(),
    }
}

/// Runs `claude -p /usage` and parses the limits. Blocking; run it off the UI
/// thread. Setting `cancel` stops it within a moment, so quitting llmon never
/// waits for Claude Code.
pub fn fetch(bin: &str, cwd: &Path, cancel: &AtomicBool) -> Result<AccountRateLimits> {
    fetch_with(bin, cwd, RUN_TIMEOUT, cancel)
}

fn fetch_with(
    bin: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<AccountRateLimits> {
    let mut captured = run(bin, cwd, true, timeout, cancel)?;
    // Claude Code versions before --safe-mode reject the flag.
    if !captured.success && captured.stderr_text().contains("unknown option") {
        captured = run(bin, cwd, false, timeout, cancel)?;
    }
    if !captured.success {
        let detail = captured.stderr_text();
        let detail = detail.lines().find(|line| !line.trim().is_empty());
        match detail {
            Some(detail) => bail!("claude failed ({}): {}", captured.status, detail.trim()),
            None => bail!("claude failed ({})", captured.status),
        }
    }
    parse_output(&String::from_utf8_lossy(&captured.stdout), unix_now())
}

struct Captured {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Captured {
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

fn run(
    bin: &str,
    cwd: &Path,
    safe_mode: bool,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Captured> {
    let mut command = Command::new(bin);
    command.args([
        "-p",
        "/usage",
        "--output-format",
        "json",
        "--verbose",
        "--no-session-persistence",
    ]);
    if safe_mode {
        command.arg("--safe-mode");
    }
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("`{bin}` was not found (install Claude Code or set claude_bin)")
        } else {
            anyhow::Error::new(error).context(format!("Unable to run `{bin}`"))
        }
    })?;
    let stdout = child.stdout.take().context("No pipe for the output")?;
    let stderr = child.stderr.take().context("No pipe for the errors")?;
    let stdout = std::thread::spawn(move || drain(stdout, MAX_STDOUT_BYTES));
    let stderr = std::thread::spawn(move || drain(stderr, MAX_STDERR_BYTES));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().context("Unable to wait for claude")? {
            Some(status) => break status,
            None if cancel.load(Ordering::Relaxed) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("cancelled");
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "claude did not answer within {} s",
                    timeout.as_secs().max(1)
                );
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    Ok(Captured {
        success: status.success(),
        status: status.to_string(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Reads a pipe to its end, keeping at most `cap` bytes; the rest is read and
/// dropped so the process never blocks on a full pipe.
fn drain(mut reader: impl Read, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    kept
}

/// The limits in the output of `claude -p /usage --output-format json`: a
/// JSON array of events (a single event object and one event per line are
/// accepted too). The first `usage_report` is used; without one, the text of
/// the command.
pub(crate) fn parse_output(text: &str, now: i64) -> Result<AccountRateLimits> {
    let events = match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(events)) => events,
        Ok(event) => vec![event],
        Err(_) => text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect(),
    };
    if events.is_empty() {
        bail!("claude printed no JSON events");
    }
    let mut limits = match events.iter().find_map(|event| event.get("usage_report")) {
        Some(report) => parse_report(report, now)?,
        None => parse_text(&events, now).context(
            "claude's output has no usage report or limits (a subscription login is needed)",
        )?,
    };
    limits.source = LimitsSourceKind::UsageCommand;
    Ok(limits)
}

fn parse_report(report: &Value, now: i64) -> Result<AccountRateLimits> {
    let rate_limits = report
        .get("rate_limits")
        .filter(|rate_limits| rate_limits.is_object())
        .context("claude's usage report has no rate limits")?;
    let limits = oauth::parse_usage(rate_limits, now)?;
    if limits.primary.is_none() && limits.secondary.is_none() && limits.buckets.is_empty() {
        bail!("claude's usage report lists no limits");
    }
    Ok(limits)
}

/// The limits from the text of the command, for builds whose JSON has no
/// `usage_report`. Its limit lines read
/// `Current session: 6% used - resets Oct 21 at 8:20pm (Region/City)` and
/// `Current week (all models): ...` or `Current week (<model>): ...`; the date
/// is written `Oct 21 at 8:20pm` or `Oct 21, 8:20pm` depending on the version.
/// Lines that do not fit are skipped, and the text carries no extra usage or
/// active limit.
fn parse_text(events: &[Value], now: i64) -> Option<AccountRateLimits> {
    let text = events
        .iter()
        .flat_map(|event| {
            ["result", "local_command_source"]
                .into_iter()
                .filter_map(move |key| event.get(key)?.as_str())
        })
        .find(|text| text.lines().any(|line| line.trim().starts_with("Current ")))?;
    let mut limits = AccountRateLimits::empty(LimitsSourceKind::UsageCommand, now);
    for line in text.lines() {
        add_text_limit(&mut limits, line.trim(), now);
    }
    (limits.primary.is_some() || limits.secondary.is_some() || !limits.buckets.is_empty())
        .then_some(limits)
}

fn add_text_limit(limits: &mut AccountRateLimits, line: &str, now: i64) -> Option<()> {
    let (label, rest) = line.strip_prefix("Current ")?.split_once(": ")?;
    let (percent, rest) = rest.split_once("% used")?;
    let percent = percent.trim().parse::<f64>().ok()?;
    // A reset that cannot be read leaves the percentage without a countdown.
    let resets_at = rest
        .split_once("resets ")
        .and_then(|(_, when)| parse_reset(when, now));
    let window = |minutes| RateLimitWindow {
        used_percent: Some(percent),
        window_duration_mins: Some(minutes),
        resets_at,
    };
    if label == "session" {
        limits.primary = Some(window(FIVE_HOUR_WINDOW_MINS));
        return Some(());
    }
    let scope = label.strip_prefix("week")?.trim();
    let scope = scope
        .strip_prefix('(')
        .and_then(|scope| scope.strip_suffix(')'))
        .map(str::trim);
    match scope {
        None => limits.secondary = Some(window(SEVEN_DAY_WINDOW_MINS)),
        Some(scope) if scope.eq_ignore_ascii_case("all models") => {
            limits.secondary = Some(window(SEVEN_DAY_WINDOW_MINS));
        }
        Some(scope) => limits.buckets.push(RateLimitSnapshot {
            limit_name: Some(scope.strip_suffix(" only").unwrap_or(scope).to_string()),
            primary: Some(window(SEVEN_DAY_WINDOW_MINS)),
        }),
    }
    Some(())
}

/// Unix seconds of a reset written in local time, as Claude Code prints it in
/// the time zone of this machine: `Oct 21 at 8:20pm (Region/City)`,
/// `Oct 21, 8:20pm`, or just `8:20pm` for the next time the clock shows it.
/// The zone name is not needed: llmon starts Claude Code with its own
/// environment, so both use the same zone.
fn parse_reset(when: &str, now: i64) -> Option<i64> {
    let when = when.split(" (").next()?.replace(',', " ");
    let mut words = when
        .split_whitespace()
        .filter(|word| !word.eq_ignore_ascii_case("at"));
    let first = words.next()?;
    let (date, clock) = match words.next() {
        Some(day) => (Some((first, day)), words.next()?),
        None => (None, first),
    };
    if words.next().is_some() {
        return None;
    }
    let (hour, minute) = parse_clock(clock)?;
    let today = Local.timestamp_opt(now, 0).single()?;
    let at = |year: i32, month: u32, day: u32| {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .map(|time| time.timestamp())
    };
    match date {
        Some((month, day)) => {
            let (month, day) = (month_number(month)?, day.parse::<u32>().ok()?);
            // A reset lies ahead, so a date that is long past is next year's.
            match at(today.year(), month, day)? {
                found if found + 86_400 >= now => Some(found),
                _ => at(today.year() + 1, month, day),
            }
        }
        None => match at(today.year(), today.month(), today.day())? {
            found if found >= now => Some(found),
            _ => {
                let tomorrow = today.date_naive().succ_opt()?;
                at(tomorrow.year(), tomorrow.month(), tomorrow.day())
            }
        },
    }
}

/// `8:20pm`, `6pm`, `12am` as a 24-hour (hour, minute).
fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let text = text.to_ascii_lowercase();
    let (digits, pm) = match text.strip_suffix("pm") {
        Some(digits) => (digits, true),
        None => (text.strip_suffix("am")?, false),
    };
    let (hour, minute) = match digits.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (digits.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    Some((hour % 12 + if pm { 12 } else { 0 }, minute))
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let prefix = name.get(..3)?.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|month| *month == prefix)
        .map(|index| index as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::claude::limits::RateLimitWindow;
    use std::cell::Cell;

    const NOW: i64 = 1_800_000_000;

    /// Synthetic output with the layout recorded in docs/sources.md.
    fn sample_output() -> String {
        serde_json::json!([
            {"type": "system", "subtype": "init", "session_id": "synthetic"},
            {
                "type": "assistant",
                "local_command_source": "<local-command-stdout>text for people</local-command-stdout>",
                "usage_report": {
                    "session": {"total_cost_usd": 0},
                    "rate_limits": {
                        "limits": [
                            {"kind": "session", "group": "session", "percent": 12,
                             "resets_at": "2027-01-15T10:00:00+00:00", "scope": null,
                             "severity": "normal", "is_active": false},
                            {"kind": "weekly_all", "group": "weekly", "percent": 34,
                             "resets_at": "2027-01-20T09:00:00.5+00:00", "scope": null,
                             "severity": "normal", "is_active": true},
                            {"kind": "weekly_scoped", "group": "weekly", "percent": 5,
                             "resets_at": "2027-01-20T09:00:00+00:00",
                             "scope": {"model": {"display_name": "Sonnet"}, "surface": null},
                             "severity": "normal", "is_active": false}
                        ],
                        "extra_usage": {"is_enabled": false, "monthly_limit": null,
                                        "used_credits": null, "utilization": null,
                                        "currency": null}
                    }
                }
            },
            {"type": "result", "subtype": "success", "local_command": "usage"}
        ])
        .to_string()
    }

    fn limits_at(captured_at: i64, percent: f64) -> AccountRateLimits {
        let mut limits = AccountRateLimits::empty(LimitsSourceKind::StatusLine, captured_at);
        limits.primary = Some(RateLimitWindow {
            used_percent: Some(percent),
            window_duration_mins: Some(300.0),
            resets_at: Some(captured_at + 3 * 3600),
        });
        limits
    }

    fn percent(limits: &Option<AccountRateLimits>) -> Option<f64> {
        limits.as_ref()?.primary.as_ref()?.used_percent
    }

    #[test]
    fn the_usage_report_becomes_the_limits() {
        let limits = parse_output(&sample_output(), NOW).expect("parse");
        assert_eq!(limits.source, LimitsSourceKind::UsageCommand);
        assert_eq!(limits.captured_at, NOW);
        let session = limits.primary.expect("session");
        assert_eq!(session.used_percent, Some(12.0));
        assert_eq!(session.window_duration_mins, Some(300.0));
        assert_eq!(session.resets_at, Some(1_800_007_200));
        let weekly = limits.secondary.expect("weekly");
        assert_eq!(weekly.used_percent, Some(34.0));
        assert_eq!(weekly.resets_at, Some(1_800_435_600));
        assert_eq!(limits.buckets.len(), 1);
        assert_eq!(limits.buckets[0].limit_name.as_deref(), Some("Sonnet"));
        assert_eq!(
            limits.buckets[0]
                .primary
                .as_ref()
                .expect("bucket")
                .used_percent,
            Some(5.0)
        );
        assert_eq!(limits.active_limit.as_deref(), Some("Weekly"));
        assert!(!limits.extra_usage.expect("extra usage").enabled);
    }

    #[test]
    fn single_events_and_json_lines_are_accepted() {
        let events: Vec<Value> = serde_json::from_str(&sample_output()).expect("events");
        let report = events[1].to_string();
        assert!(parse_output(&report, NOW).is_ok(), "one event object");

        let lines = events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_output(&lines, NOW).is_ok(), "one event per line");
    }

    #[test]
    fn output_without_limits_is_an_error() {
        let extra_only =
            serde_json::json!([{"usage_report": {"rate_limits": {"extra_usage": {}}}}]).to_string();
        let cases = [
            ("", "no JSON"),
            ("not json at all", "no JSON"),
            (r#"[{"type":"result","result":"API usage"}]"#, "no usage"),
            (r#"[{"usage_report":{}}]"#, "no rate limits"),
            (r#"[{"usage_report":{"rate_limits":[]}}]"#, "no rate limits"),
            (
                r#"[{"usage_report":{"rate_limits":{"limits":[]}}}]"#,
                "lists no limits",
            ),
            (extra_only.as_str(), "lists no limits"),
        ];
        for (input, expected) in cases {
            let error = parse_output(input, NOW).expect_err(input);
            assert!(error.to_string().contains(expected), "{input}: {error}");
        }
    }

    fn local_unix(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("local time")
            .timestamp()
    }

    /// The text of the command in the layout of a build that prints no
    /// `usage_report`. Values are synthetic; `\u{b7}` is a middle dot.
    fn sample_text(separator: &str) -> String {
        format!(
            "You are currently using your subscription to power your Claude Code usage\n\n\
             Current session: 12% used \u{b7} resets Jan 15{separator}10am (Region/City)\n\
             Current week (all models): 34% used \u{b7} resets Jan 20{separator}9:30pm (Region/City)\n\
             Current week (Sonnet only): 5% used \u{b7} resets Jan 20{separator}9:30pm (Region/City)\n\n\
             What's contributing to your limits usage?\n\
             Last 24h \u{b7} 10 requests \u{b7} 1 session\n  50% of your usage was at >150k context"
        )
    }

    #[test]
    fn the_text_is_read_when_there_is_no_usage_report() {
        let now = local_unix(2027, 1, 14, 9, 0);
        for separator in [" at ", ", "] {
            let output = serde_json::json!({
                "type": "result",
                "subtype": "success",
                "local_command": "usage",
                "result": sample_text(separator),
            })
            .to_string();
            let limits = parse_output(&output, now).expect(separator);
            assert_eq!(limits.source, LimitsSourceKind::UsageCommand);
            assert_eq!(limits.captured_at, now);
            let session = limits.primary.expect("session");
            assert_eq!(session.used_percent, Some(12.0));
            assert_eq!(session.window_duration_mins, Some(FIVE_HOUR_WINDOW_MINS));
            assert_eq!(session.resets_at, Some(local_unix(2027, 1, 15, 10, 0)));
            let weekly = limits.secondary.expect("weekly");
            assert_eq!(weekly.used_percent, Some(34.0));
            assert_eq!(weekly.window_duration_mins, Some(SEVEN_DAY_WINDOW_MINS));
            assert_eq!(weekly.resets_at, Some(local_unix(2027, 1, 20, 21, 30)));
            assert_eq!(limits.buckets.len(), 1);
            assert_eq!(limits.buckets[0].limit_name.as_deref(), Some("Sonnet"));
            let scoped = limits.buckets[0].primary.as_ref().expect("bucket");
            assert_eq!(scoped.used_percent, Some(5.0));
            assert_eq!(limits.active_limit, None);
            assert!(limits.extra_usage.is_none());
        }
    }

    #[test]
    fn the_text_is_found_in_the_local_command_source_too() {
        let events = serde_json::json!([
            {"type": "assistant",
             "local_command_source": format!("<local-command-stdout>{}</local-command-stdout>",
                                             sample_text(" at "))},
            {"type": "result", "result": sample_text(" at ")}
        ])
        .to_string();
        let limits = parse_output(&events, local_unix(2027, 1, 14, 9, 0)).expect("parse");
        assert_eq!(limits.buckets.len(), 1, "one copy of the text is read");
        assert_eq!(limits.primary.and_then(|w| w.used_percent), Some(12.0));
    }

    #[test]
    fn the_usage_report_wins_over_the_text() {
        let mut events: Vec<Value> = serde_json::from_str(&sample_output()).expect("events");
        events.push(serde_json::json!({
            "type": "result",
            "result": "Current session: 99% used \u{b7} resets Jan 15 at 10am (Region/City)"
        }));
        let limits = parse_output(&Value::Array(events).to_string(), NOW).expect("parse");
        assert_eq!(limits.primary.and_then(|w| w.used_percent), Some(12.0));
    }

    #[test]
    fn a_reset_that_cannot_be_read_keeps_the_percentage() {
        let output = serde_json::json!({
            "result": "Current session: 40% used \u{b7} resets soon\nCurrent week (all models): 7% used"
        })
        .to_string();
        let limits = parse_output(&output, NOW).expect("parse");
        let session = limits.primary.expect("session");
        assert_eq!(session.used_percent, Some(40.0));
        assert_eq!(session.resets_at, None);
        assert_eq!(limits.secondary.and_then(|w| w.used_percent), Some(7.0));
    }

    #[test]
    fn reset_times_are_read_in_every_written_form() {
        let now = local_unix(2027, 1, 14, 9, 0);
        let cases = [
            (
                "Jan 15 at 10:05am (Region/City)",
                Some((2027, 1, 15, 10, 5)),
            ),
            ("Jan 15, 10:05am (Region/City)", Some((2027, 1, 15, 10, 5))),
            ("Jan 15 at 12am", Some((2027, 1, 15, 0, 0))),
            ("Jan 15 at 12pm", Some((2027, 1, 15, 12, 0))),
            ("Jan 15 at 11:59PM", Some((2027, 1, 15, 23, 59))),
            ("Sept 3 at 1pm", Some((2027, 9, 3, 13, 0))),
            ("Mar 3 at 1pm", Some((2027, 3, 3, 13, 0))),
            // Only a time: the next time the clock shows it.
            ("6pm (Region/City)", Some((2027, 1, 14, 18, 0))),
            ("8am", Some((2027, 1, 15, 8, 0))),
            // A date that is long past is next year's.
            ("Jan 2 at 1pm", Some((2028, 1, 2, 13, 0))),
            ("Jan 13 at 1pm", Some((2027, 1, 13, 13, 0))),
            ("soon", None),
            ("Foo 3 at 1pm", None),
            ("Jan 15 at 25pm", None),
            ("Jan 15 at 10:61am", None),
            ("Jan 15 at 0am", None),
            ("Jan 15 at 10", None),
            ("Feb 30 at 1pm", None),
            ("Jan 15 at 10am extra", None),
            ("", None),
        ];
        for (text, expected) in cases {
            let expected = expected.map(|(y, mo, d, h, mi)| local_unix(y, mo, d, h, mi));
            assert_eq!(parse_reset(text, now), expected, "{text}");
        }
    }

    #[test]
    fn the_poll_interval_has_a_floor_and_zero_means_off() {
        assert_eq!(poll_interval(0), None);
        assert_eq!(poll_interval(1), Some(Duration::from_secs(MIN_POLL_SECS)));
        assert_eq!(poll_interval(600), Some(Duration::from_secs(600)));
        assert_eq!(poll_interval(3600), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn a_fresh_status_line_wins_and_a_stale_one_gives_way_to_a_newer_poll() {
        let fresh = limits_at(NOW - 60, 10.0);
        let stale = limits_at(NOW - 2 * 3600, 20.0);
        let mut polled = limits_at(NOW - 300, 30.0);
        polled.source = LimitsSourceKind::UsageCommand;

        assert_eq!(
            percent(&pick(Some(fresh.clone()), Some(&polled), NOW)),
            Some(10.0)
        );
        assert_eq!(
            percent(&pick(Some(stale.clone()), Some(&polled), NOW)),
            Some(30.0)
        );
        assert_eq!(percent(&pick(None, Some(&polled), NOW)), Some(30.0));
        assert_eq!(percent(&pick(Some(stale.clone()), None, NOW)), Some(20.0));
        assert_eq!(percent(&pick(None, None, NOW)), None);

        // A status line that ran after the poll is newer, even while stale.
        let mut old_poll = polled.clone();
        old_poll.captured_at = NOW - 3 * 3600;
        assert_eq!(
            percent(&pick(Some(stale.clone()), Some(&old_poll), NOW)),
            Some(20.0)
        );
    }

    #[test]
    fn a_polled_window_loses_its_percentage_after_its_reset() {
        let mut polled = limits_at(NOW - 4 * 3600, 30.0);
        polled.source = LimitsSourceKind::UsageCommand;
        // The window reset an hour ago.
        let shown = pick(None, Some(&polled), NOW).expect("polled limits");
        let window = shown.primary.expect("window");
        assert_eq!(window.used_percent, None);
        assert_eq!(window.resets_at, None);
    }

    fn fallback() -> Fallback {
        Fallback::new(
            "claude".to_string(),
            PathBuf::from("."),
            Duration::from_secs(600),
        )
    }

    #[test]
    fn polling_starts_when_the_status_line_is_stale_and_follows_the_interval() {
        let mut fallback = fallback();
        let start = Instant::now();
        let calls = Cell::new(0);
        let fetch = |_: &str, _: &Path| {
            calls.set(calls.get() + 1);
            Ok(limits_at(NOW, 40.0))
        };
        let stale = || Ok(Some(limits_at(NOW - 3600, 20.0)));

        let tick = fallback.tick(stale(), NOW, start, fetch);
        assert_eq!(percent(&tick.limits.expect("limits")), Some(40.0));
        assert_eq!(tick.error, None);

        // Not due again before the interval, whatever the tick rate.
        let _ = fallback.tick(stale(), NOW + 10, start + Duration::from_secs(10), fetch);
        let _ = fallback.tick(stale(), NOW + 599, start + Duration::from_secs(599), fetch);
        assert_eq!(calls.get(), 1);
        let _ = fallback.tick(stale(), NOW + 600, start + Duration::from_secs(600), fetch);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_fresh_status_line_pauses_polling_and_a_stale_one_resumes_it() {
        let mut fallback = fallback();
        let start = Instant::now();
        let calls = Cell::new(0);
        let answered_at = Cell::new(NOW);
        let fetch = |_: &str, _: &Path| {
            calls.set(calls.get() + 1);
            Ok(limits_at(answered_at.get(), 40.0))
        };
        let _ = fallback.tick(Ok(Some(limits_at(NOW - 3600, 20.0))), NOW, start, fetch);

        // Claude Code runs again: the status line is fresh, no polls, and the
        // status line is what is shown.
        let later = NOW + 4000;
        for step in 0..5u64 {
            let tick = fallback.tick(
                Ok(Some(limits_at(later - 5, 50.0))),
                later + step as i64,
                start + Duration::from_secs(1000 + step),
                fetch,
            );
            assert_eq!(percent(&tick.limits.expect("limits")), Some(50.0));
        }
        assert_eq!(calls.get(), 1, "no poll while the status line is live");

        // It goes quiet again; the next poll is due at once (the interval has
        // passed) and its answer is newer than the stale snapshot.
        let much_later = later + 2 * 3600;
        answered_at.set(much_later);
        let tick = fallback.tick(
            Ok(Some(limits_at(later - 5, 50.0))),
            much_later,
            start + Duration::from_secs(8000),
            fetch,
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(percent(&tick.limits.expect("limits")), Some(40.0));
    }

    #[test]
    fn a_missing_snapshot_is_polled_too_and_failures_are_reported_once() {
        let mut fallback = fallback();
        let start = Instant::now();
        let failing =
            |_: &str, _: &Path| -> Result<AccountRateLimits> { bail!("claude is broken") };

        let tick = fallback.tick(Ok(None), NOW, start, failing);
        assert!(tick.limits.expect("limits").is_none(), "nothing to show");
        assert_eq!(tick.error, Some(Some("claude is broken".to_string())));

        // Unchanged error: not reported again.
        let tick = fallback.tick(Ok(None), NOW + 10, start + Duration::from_secs(10), failing);
        assert_eq!(tick.error, None);

        // A later success clears it.
        let tick = fallback.tick(
            Ok(None),
            NOW + 700,
            start + Duration::from_secs(700),
            |_, _| Ok(limits_at(NOW + 700, 8.0)),
        );
        assert_eq!(tick.error, Some(None));
        assert_eq!(percent(&tick.limits.expect("limits")), Some(8.0));
    }

    #[test]
    fn a_failed_poll_keeps_the_stale_snapshot_and_the_last_good_poll() {
        let mut fallback = fallback();
        let start = Instant::now();
        let stale = || Ok(Some(limits_at(NOW - 3 * 3600, 20.0)));

        let tick = fallback.tick(stale(), NOW, start, |_, _| bail!("claude timed out"));
        assert_eq!(percent(&tick.limits.expect("limits")), Some(20.0));
        assert!(tick.error.is_some());

        let tick = fallback.tick(
            stale(),
            NOW + 700,
            start + Duration::from_secs(700),
            |_, _| Ok(limits_at(NOW + 700, 25.0)),
        );
        assert_eq!(percent(&tick.limits.expect("limits")), Some(25.0));

        let tick = fallback.tick(
            stale(),
            NOW + 1400,
            start + Duration::from_secs(1400),
            |_, _| bail!("claude timed out"),
        );
        assert_eq!(
            percent(&tick.limits.expect("limits")),
            Some(25.0),
            "the last good poll is kept next to the error"
        );
        assert_eq!(tick.error, Some(Some("claude timed out".to_string())));
    }

    #[test]
    fn an_unreadable_status_line_falls_back_to_the_poll() {
        let mut fallback = fallback();
        let start = Instant::now();
        let broken = || Err(anyhow::anyhow!("limits.json is not valid"));

        let tick = fallback.tick(broken(), NOW, start, |_, _| bail!("no claude"));
        assert!(tick.limits.is_err(), "nothing else to show");

        let tick = fallback.tick(
            broken(),
            NOW + 700,
            start + Duration::from_secs(700),
            |_, _| Ok(limits_at(NOW + 700, 9.0)),
        );
        assert_eq!(percent(&tick.limits.expect("limits")), Some(9.0));
    }

    #[test]
    fn long_errors_are_cut_to_one_short_line() {
        let long = anyhow::anyhow!("{}\nsecond line", "x".repeat(300));
        let message = short_message(&long);
        assert_eq!(message.chars().count(), MAX_ERROR_CHARS + 3);
        assert!(message.ends_with("..."));
        assert!(!message.contains("second"));
        assert_eq!(short_message(&anyhow::anyhow!("short")), "short");
    }

    #[cfg(unix)]
    mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

        fn make_temp_dir(prefix: &str) -> PathBuf {
            let unique = format!(
                "{}-{}",
                std::process::id(),
                TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let dir = std::env::temp_dir().join(format!("llmon-usage-cmd-{prefix}-{unique}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            dir
        }

        /// A stand-in for `claude`: a shell script that records its arguments
        /// and then runs `body`.
        fn fake_claude(dir: &Path, body: &str) -> String {
            let path = dir.join("fake-claude");
            let script = format!(
                "#!/bin/sh\necho \"$@\" >> \"{}/args.txt\"\n{body}\n",
                dir.display()
            );
            std::fs::write(&path, script).expect("write script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
            path.display().to_string()
        }

        fn fetch_for_test(bin: &str, dir: &Path, timeout: Duration) -> Result<AccountRateLimits> {
            fetch_with(bin, dir, timeout, &AtomicBool::new(false))
        }

        fn calls(dir: &Path) -> Vec<String> {
            std::fs::read_to_string(dir.join("args.txt"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        #[test]
        fn the_command_is_run_with_the_quiet_flags_and_parsed() {
            let dir = make_temp_dir("run");
            std::fs::write(dir.join("out.json"), sample_output()).expect("write output");
            let bin = fake_claude(&dir, &format!("cat \"{}/out.json\"", dir.display()));

            let limits = fetch_for_test(&bin, &dir, Duration::from_secs(10)).expect("fetch");
            assert_eq!(limits.source, LimitsSourceKind::UsageCommand);
            assert_eq!(limits.primary.expect("session").used_percent, Some(12.0));
            assert_eq!(
                calls(&dir),
                vec![
                    "-p /usage --output-format json --verbose --no-session-persistence --safe-mode"
                ]
            );
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_version_without_safe_mode_is_asked_again_without_it() {
            let dir = make_temp_dir("old");
            std::fs::write(dir.join("out.json"), sample_output()).expect("write output");
            let body = format!(
                "case \"$*\" in *--safe-mode*) echo \"error: unknown option '--safe-mode'\" >&2; exit 1;; esac\ncat \"{}/out.json\"",
                dir.display()
            );
            let bin = fake_claude(&dir, &body);

            assert!(fetch_for_test(&bin, &dir, Duration::from_secs(10)).is_ok());
            let calls = calls(&dir);
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(calls[0].ends_with("--safe-mode"));
            assert!(!calls[1].contains("--safe-mode"));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_failing_command_reports_its_first_error_line() {
            let dir = make_temp_dir("fail");
            let bin = fake_claude(&dir, "echo >&2\necho 'Not logged in' >&2\nexit 3");
            let error = fetch_for_test(&bin, &dir, Duration::from_secs(10)).expect_err("fails");
            let text = error.to_string();
            assert!(text.contains("Not logged in"), "{text}");
            assert!(text.contains('3'), "{text}");
            assert_eq!(calls(&dir).len(), 1, "no retry for other errors");
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_hung_command_is_killed_at_the_timeout() {
            let dir = make_temp_dir("hang");
            let bin = fake_claude(&dir, "exec sleep 30");
            let started = Instant::now();
            let error =
                fetch_for_test(&bin, &dir, Duration::from_millis(300)).expect_err("times out");
            assert!(error.to_string().contains("did not answer"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(10));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn cancelling_stops_a_running_command_at_once() {
            let dir = make_temp_dir("cancel");
            let bin = fake_claude(&dir, "exec sleep 30");
            let cancel = AtomicBool::new(false);
            let started = Instant::now();
            let error = std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(150));
                    cancel.store(true, Ordering::Relaxed);
                });
                fetch_with(&bin, &dir, Duration::from_secs(25), &cancel).expect_err("cancelled")
            });
            assert!(error.to_string().contains("cancelled"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(10));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_missing_binary_says_so() {
            let dir = make_temp_dir("missing");
            let error = fetch_for_test(
                "/nonexistent/llmon-test-claude",
                &dir,
                Duration::from_secs(5),
            )
            .expect_err("missing");
            assert!(error.to_string().contains("was not found"), "{error}");
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn huge_output_is_capped_and_does_not_block_the_command() {
            let dir = make_temp_dir("huge");
            // About 6 MiB of output, more than the cap and the pipe buffer.
            let bin = fake_claude(&dir, "yes 0123456789abcdef | head -c 6000000");
            let error = fetch_for_test(&bin, &dir, Duration::from_secs(20)).expect_err("not JSON");
            assert!(error.to_string().contains("no JSON"), "{error}");
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
