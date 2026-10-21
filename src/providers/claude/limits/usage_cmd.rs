//! Fallback source for the Claude Code limits: `claude -p /usage`.
//!
//! The status-line snapshot only changes while Claude Code runs, so after a
//! long pause it shows old numbers. Then llmon asks Claude Code itself for the
//! limits, at most once per poll interval, and stops asking as soon as a fresh
//! status-line snapshot arrives. `/usage` is a local command: it costs no
//! tokens, and Claude Code makes the one request to Anthropic with its own
//! login, so llmon reads no credentials.
//!
//! In print mode with `--output-format json`, Claude Code emits the events of
//! the run; the `/usage` one carries a `usage_report` with a `rate_limits`
//! object in the same `limits[]` shape as the OAuth endpoint (field notes in
//! `docs/sources.md`). The text of the command is for people and is not
//! parsed. The report is undocumented, so a missing field is an error, not a
//! panic.
//!
//! Flags keep the call light and free of side effects: `--no-session-persistence`
//! leaves no transcript for the usage scan to count, and `--safe-mode` skips
//! the user's hooks, MCP servers, and plugins.

use super::{oauth, unix_now, AccountRateLimits, LimitsSourceKind};
use anyhow::{bail, Context, Result};
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

/// The limits of the first `usage_report` in the output of
/// `claude -p /usage --output-format json`: a JSON array of events (a single
/// event object and one event per line are accepted too).
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
    let report = events
        .iter()
        .find_map(|event| event.get("usage_report"))
        .context("claude reported no usage (a subscription login is needed)")?;
    let rate_limits = report
        .get("rate_limits")
        .filter(|rate_limits| rate_limits.is_object())
        .context("claude's usage report has no rate limits")?;
    let mut limits = oauth::parse_usage(rate_limits, now)?;
    if limits.primary.is_none() && limits.secondary.is_none() && limits.buckets.is_empty() {
        bail!("claude's usage report lists no limits");
    }
    limits.source = LimitsSourceKind::UsageCommand;
    Ok(limits)
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
                vec!["-p /usage --output-format json --no-session-persistence --safe-mode"]
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
