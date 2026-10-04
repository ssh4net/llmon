//! Claude Code subscription limits and the sources that produce them (field
//! notes in `docs/sources.md`):
//!
//! - status-line bridge (default): `llmon statusline` records the rate
//!   limits Claude Code passes to its status line; the TUI reads that
//!   snapshot. No credentials are involved.
//! - OAuth usage endpoint (opt-in): the endpoint behind Claude Code's
//!   `/usage` screen, called with the user's Claude Code OAuth token. It adds
//!   per-model weekly limits, extra usage, and the by-surface breakdown.

pub mod oauth;
pub mod statusline;

use chrono::DateTime;
use serde_json::Value;

pub const FIVE_HOUR_WINDOW_MINS: f64 = 5.0 * 60.0;
pub const SEVEN_DAY_WINDOW_MINS: f64 = 7.0 * 24.0 * 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsSourceKind {
    StatusLine,
    OAuth,
}

impl LimitsSourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::StatusLine => "STATUSLINE",
            Self::OAuth => "OAUTH",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    /// Share of the limit used, 0-100.
    pub used_percent: Option<f64>,
    pub window_duration_mins: Option<f64>,
    /// Unix seconds when the window resets.
    pub resets_at: Option<i64>,
}

/// A limit scoped to one model or surface (for example the weekly limit of
/// one model family).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitSnapshot {
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
}

/// Paid usage beyond the subscription limits.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraUsage {
    pub enabled: bool,
    pub used_amount: Option<f64>,
    pub limit_amount: Option<f64>,
    pub currency: Option<String>,
    pub used_percent: Option<f64>,
}

/// Share of the weekly usage by surface (Claude Code, chat, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceShare {
    pub name: String,
    pub percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountRateLimits {
    pub source: LimitsSourceKind,
    /// Unix seconds when the data was captured.
    pub captured_at: i64,
    /// Subscription type reported with the credentials ("pro", "max", ...).
    pub plan: Option<String>,
    /// Five-hour session window.
    pub primary: Option<RateLimitWindow>,
    /// Seven-day window across all models.
    pub secondary: Option<RateLimitWindow>,
    /// Scoped weekly limits (per model).
    pub buckets: Vec<RateLimitSnapshot>,
    /// Gateway spend limit (status line only).
    pub spend_limit: Option<RateLimitWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub surfaces: Vec<SurfaceShare>,
    /// Name of the limit that currently binds, when the server reports it.
    pub active_limit: Option<String>,
}

impl AccountRateLimits {
    pub fn empty(source: LimitsSourceKind, captured_at: i64) -> Self {
        Self {
            source,
            captured_at,
            plan: None,
            primary: None,
            secondary: None,
            buckets: Vec::new(),
            spend_limit: None,
            extra_usage: None,
            surfaces: Vec::new(),
            active_limit: None,
        }
    }
}

/// The limits as JSON, for `--dump-limits`.
pub fn limits_dump_json(limits: &AccountRateLimits) -> Value {
    fn window_json(window: &RateLimitWindow) -> Value {
        serde_json::json!({
            "used_percent": window.used_percent,
            "window_duration_mins": window.window_duration_mins,
            "resets_at": window.resets_at,
        })
    }
    serde_json::json!({
        "source": limits.source.label(),
        "captured_at": limits.captured_at,
        "plan": limits.plan,
        "primary": limits.primary.as_ref().map(window_json),
        "secondary": limits.secondary.as_ref().map(window_json),
        "buckets": limits.buckets.iter().map(|bucket| serde_json::json!({
            "limit_name": bucket.limit_name,
            "primary": bucket.primary.as_ref().map(window_json),
        })).collect::<Vec<_>>(),
        "spend_limit": limits.spend_limit.as_ref().map(window_json),
        "extra_usage": limits.extra_usage.as_ref().map(|extra| serde_json::json!({
            "enabled": extra.enabled,
            "used_amount": extra.used_amount,
            "limit_amount": extra.limit_amount,
            "currency": extra.currency,
            "used_percent": extra.used_percent,
        })),
        "surfaces": limits.surfaces.iter().map(|surface| serde_json::json!({
            "name": surface.name,
            "percent": surface.percent,
        })).collect::<Vec<_>>(),
        "active_limit": limits.active_limit,
    })
}

pub fn normalize_epoch_millis(value: i64) -> i64 {
    if value > 0 && value < 1_000_000_000_000 {
        value * 1000
    } else {
        value
    }
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Reads a number that may be encoded as an integer, float, or numeric string.
pub(crate) fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|number: &f64| number.is_finite())
}

/// Reads a timestamp given as Unix seconds/milliseconds or an RFC 3339 string
/// and returns Unix seconds.
pub(crate) fn as_unix_seconds(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|parsed| parsed.timestamp());
    }
    let raw = value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))?;
    Some(normalize_epoch_millis(raw) / 1000)
}
