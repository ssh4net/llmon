//! Status-line bridge. Claude Code runs `statusLine.command` after each
//! assistant message and pipes a JSON object to it on stdin. For
//! subscribers that object carries `rate_limits` (`five_hour`, `seven_day`,
//! and a gateway `spend_limit`), each with `used_percentage` (0-100) and
//! `resets_at` (Unix seconds). `llmon statusline` stores the latest
//! windows in `<llmon-home>/limits.json` and prints a compact status
//! line; the TUI reads that snapshot.
//!
//! The command must never break Claude Code's status line: every failure
//! degrades to a minimal line and exit code 0.

use super::{
    as_f64, as_unix_seconds, unix_now, AccountRateLimits, LimitsSourceKind, RateLimitWindow,
    FIVE_HOUR_WINDOW_MINS, SEVEN_DAY_WINDOW_MINS,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

pub const SNAPSHOT_FILE_NAME: &str = "limits.json";
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const MAX_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredWindow {
    used_percentage: f64,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredSnapshot {
    schema_version: u32,
    captured_at: i64,
    #[serde(default)]
    five_hour: Option<StoredWindow>,
    #[serde(default)]
    seven_day: Option<StoredWindow>,
    #[serde(default)]
    spend_limit: Option<StoredWindow>,
}

/// Entry point of `llmon statusline`. Reads the status-line JSON from
/// stdin, records the rate limits, and prints a status line (or the output of
/// the wrapped command).
pub fn run(llmon_home: Option<&Path>, wrap: Option<&str>) {
    let mut input = Vec::new();
    let _ = std::io::stdin()
        .take(MAX_INPUT_BYTES)
        .read_to_end(&mut input);
    let parsed = serde_json::from_slice::<Value>(&input).ok();

    if let (Some(home), Some(value)) = (llmon_home, parsed.as_ref()) {
        if let Some(snapshot) = capture(value, unix_now()) {
            let _ = store(home, &snapshot);
        }
    }

    let wrapped = wrap.and_then(|command| run_wrapped(command, &input));
    let line = wrapped.unwrap_or_else(|| {
        parsed
            .as_ref()
            .map(status_text)
            .unwrap_or_else(|| "llmon".to_string())
    });
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    if !line.ends_with('\n') {
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
}

fn capture(input: &Value, now: i64) -> Option<StoredSnapshot> {
    let limits = input.get("rate_limits")?;
    let window = |key: &str| -> Option<StoredWindow> {
        let raw = limits.get(key)?;
        Some(StoredWindow {
            used_percentage: raw.get("used_percentage").and_then(as_f64)?,
            resets_at: raw.get("resets_at").and_then(as_unix_seconds),
        })
    };
    let snapshot = StoredSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        captured_at: now,
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
        spend_limit: window("spend_limit"),
    };
    // Only record snapshots that carry data, so an early status-line call
    // (before the first API response) never overwrites a useful snapshot.
    (snapshot.five_hour.is_some() || snapshot.seven_day.is_some() || snapshot.spend_limit.is_some())
        .then_some(snapshot)
}

fn store(llmon_home: &Path, snapshot: &StoredSnapshot) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(snapshot).context("Unable to encode limits snapshot")?;
    crate::storage::write_private_file_atomic(&llmon_home.join(SNAPSHOT_FILE_NAME), &bytes)
}

/// Loads the latest status-line snapshot. Windows whose reset time has
/// passed are reported without a percentage: the old value no longer applies
/// and the new one is unknown until Claude Code runs again.
pub fn load(llmon_home: &Path, now: i64) -> Result<Option<AccountRateLimits>> {
    let path = llmon_home.join(SNAPSHOT_FILE_NAME);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", path.display()))
        }
    };
    if !meta.file_type().is_file() || meta.len() > MAX_SNAPSHOT_BYTES {
        anyhow::bail!(
            "Refusing to read {}: not a small regular file",
            path.display()
        );
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("Unable to read {}", path.display()))?;
    let snapshot: StoredSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("Unable to parse {}", path.display()))?;

    let window = |stored: &Option<StoredWindow>, minutes: Option<f64>| {
        stored.as_ref().map(|stored| {
            let expired = stored.resets_at.is_some_and(|reset| reset <= now);
            RateLimitWindow {
                used_percent: (!expired).then_some(stored.used_percentage),
                window_duration_mins: minutes,
                resets_at: stored.resets_at.filter(|_| !expired),
            }
        })
    };
    let mut limits = AccountRateLimits::empty(LimitsSourceKind::StatusLine, snapshot.captured_at);
    limits.primary = window(&snapshot.five_hour, Some(FIVE_HOUR_WINDOW_MINS));
    limits.secondary = window(&snapshot.seven_day, Some(SEVEN_DAY_WINDOW_MINS));
    limits.spend_limit = window(&snapshot.spend_limit, None);
    Ok(Some(limits))
}

fn status_text(input: &Value) -> String {
    let model = input
        .get("model")
        .and_then(|model| {
            model
                .get("display_name")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Claude");
    let mut parts = vec![model.to_string()];
    let limits = input.get("rate_limits");
    for (key, label) in [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("spend_limit", "spend"),
    ] {
        if let Some(used) = limits
            .and_then(|limits| limits.get(key))
            .and_then(|window| window.get("used_percentage"))
            .and_then(as_f64)
        {
            parts.push(format!("{label} {}%", used.round() as i64));
        }
    }
    parts.join(" | ")
}

/// Runs the wrapped status-line command with the same stdin and returns its
/// stdout, or `None` when it cannot run or fails.
fn run_wrapped(command: &str, input: &[u8]) -> Option<String> {
    #[cfg(windows)]
    let mut child = Command::new("cmd");
    #[cfg(windows)]
    child.args(["/C", command]);
    #[cfg(not(windows))]
    let mut child = Command::new("sh");
    #[cfg(not(windows))]
    child.args(["-c", command]);

    let mut child = child
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "llmon-statusline-{label}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn sample_input() -> Value {
        serde_json::json!({
            "model": {"id": "claude-test-1", "display_name": "Test 1"},
            "rate_limits": {
                "five_hour": {"used_percentage": 12.4, "resets_at": 2_000_000_000},
                "seven_day": {"used_percentage": 40, "resets_at": 2_000_500_000}
            }
        })
    }

    #[test]
    fn capture_reads_documented_rate_limit_fields() {
        let snapshot = capture(&sample_input(), 1_900_000_000).expect("snapshot");
        assert_eq!(
            snapshot.five_hour,
            Some(StoredWindow {
                used_percentage: 12.4,
                resets_at: Some(2_000_000_000),
            })
        );
        assert_eq!(snapshot.seven_day.map(|w| w.used_percentage), Some(40.0));
        assert_eq!(snapshot.spend_limit, None);
        assert_eq!(status_text(&sample_input()), "Test 1 | 5h 12% | 7d 40%");
    }

    #[test]
    fn capture_ignores_input_without_rate_limits() {
        let input = serde_json::json!({"model": {"display_name": "Test 1"}});
        assert_eq!(capture(&input, 1), None);
        assert_eq!(status_text(&input), "Test 1");
    }

    #[test]
    fn stored_snapshot_loads_as_limits_and_expires_windows() {
        let home = temp_dir("load");
        let snapshot = capture(&sample_input(), 1_900_000_000).expect("snapshot");
        store(&home, &snapshot).expect("store snapshot");

        let limits = load(&home, 1_950_000_000)
            .expect("load")
            .expect("snapshot present");
        assert_eq!(limits.source, LimitsSourceKind::StatusLine);
        assert_eq!(limits.captured_at, 1_900_000_000);
        let primary = limits.primary.expect("five-hour window");
        assert_eq!(primary.used_percent, Some(12.4));
        assert_eq!(primary.window_duration_mins, Some(FIVE_HOUR_WINDOW_MINS));

        let later = load(&home, 2_000_100_000)
            .expect("load")
            .expect("snapshot present");
        assert_eq!(later.primary.and_then(|w| w.used_percent), None);
        assert_eq!(later.secondary.and_then(|w| w.used_percent), Some(40.0));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.join(SNAPSHOT_FILE_NAME))
                .expect("snapshot metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn missing_snapshot_is_not_an_error() {
        let home = temp_dir("missing");
        assert_eq!(load(&home, 1).expect("load"), None);
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn wrapped_command_receives_stdin_and_its_output_is_used() {
        let output = run_wrapped("tr a-z A-Z", b"hello").expect("wrapped output");
        assert_eq!(output, "HELLO");
        assert_eq!(run_wrapped("exit 3", b""), None);
    }
}
