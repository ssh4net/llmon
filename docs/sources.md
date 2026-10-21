# Claude data sources

Field-level notes for the Claude Code live-limit sources (ported from
ClaudeMon; `PLAN.md` section 1 lists the sources of every harness). S1 and
S5 are ClaudeMon's source labels.
Observed 2026-09-26 with Claude Code 2.1.283 on a subscription account.
Only field names and value types are recorded here; no tokens or account
values.

## S1. OAuth usage endpoint (undocumented)

Request:

```
GET https://api.anthropic.com/api/oauth/usage
Authorization: Bearer <claudeAiOauth.accessToken>
anthropic-beta: oauth-2025-04-20
```

No rate-limit headers were returned.

Credentials file `<claude-dir>/.credentials.json` (Linux/Windows; macOS uses
the Keychain): `claudeAiOauth.{accessToken, refreshToken, expiresAt (ms),
refreshTokenExpiresAt, scopes[], subscriptionType ("pro", "max", ...),
rateLimitTier}`. llmon must never refresh or rewrite this file; when the
access token is expired it reports "stale token, run Claude Code".

Response (top level):

| Field | Type | Notes |
|-------|------|-------|
| `five_hour`, `seven_day` | window or null | `utilization` (0-100 float), `resets_at` (RFC 3339 with offset), `limit_dollars`, `used_dollars`, `remaining_dollars`, `locked_reason` |
| `seven_day_opus`, `seven_day_sonnet`, `seven_day_oauth_apps`, `seven_day_cowork`, ... | window or null | Legacy per-scope windows |
| many code-named keys (`nimbus_quill`, `amber_gauge`, ...) | window or null | Unstable internal names; ignore |
| `limits[]` | array | **Preferred.** Self-describing list, see below |
| `extra_usage` | object | `is_enabled`, `monthly_limit`, `used_credits`, `utilization`, `currency`, `decimal_places`, `disabled_reason`, `user_disabled`, `spend_limit_reached`, `credits_ever_enabled`, `daily`, `weekly` |
| `spend` | object | `used{amount_minor, currency, exponent}`, `limit`, `percent`, `severity`, `enabled`, `disabled_reason`, `cap`, `balance`, `auto_reload`, `disclaimer`, `can_purchase_credits`, `can_toggle` |
| `seven_day_breakdown` | object | `as_of`, `window_started_at`, `rows[]{key, display_name, percent}`; keys seen: `claude_code`, `chat`, `cowork`, `other` |
| `member_dashboard_available` | bool | |

`limits[]` entries:

| Field | Type | Values seen |
|-------|------|-------------|
| `kind` | string | `session`, `weekly_all`, `weekly_scoped` |
| `group` | string | `session`, `weekly` |
| `percent` | integer | 0-100 |
| `severity` | string | `normal` (others expected: warning/critical) |
| `resets_at` | RFC 3339 string or null | |
| `scope` | object or null | `model{id, display_name}`, `surface` |
| `is_active` | bool | Which limit currently binds |

Mapping to `limits::AccountRateLimits`: `session` -> `primary`
(300-minute window), `weekly_all` -> `secondary` (10080-minute window),
each `weekly_scoped` -> one entry in `buckets` named after
`scope.model.display_name`. `extra_usage`/`spend` replace the Codex
credits summary; `seven_day_breakdown` is a new "by surface" row.

## S6. `claude -p /usage` (undocumented; fallback of S5)

`claude -p "/usage" --output-format json --no-session-persistence --safe-mode`
runs the `/usage` screen as a local command: no model call (zero tokens, zero
cost, about 3 s), and Claude Code makes the usage request with its own login.
Observed 2026-10-21 with Claude Code 2.1.304 on a subscription account.

- `--no-session-persistence` leaves no transcript (without it every call
  writes a session file that the usage scan and HISTORY would count).
- `--safe-mode` skips hooks, MCP servers, and plugins; versions that do not
  know the flag fail with `unknown option`, and llmon repeats the call
  without it.
- Output is a JSON array of events (`system` init, an `assistant` event, a
  `result`). The `assistant` event has `local_command_source` (the text
  people read) and `usage_report`:
  `{session{total_cost_usd, ...}, rate_limits{limits[], extra_usage{is_enabled,
  monthly_limit, used_credits, utilization, currency}}}`.
- `rate_limits.limits[]` has the same entries as S1 (`kind`, `group`,
  `percent`, `resets_at` as RFC 3339, `scope`, `severity`, `is_active`), so it
  goes through the same parser. Per-model weekly limits are included.
- Some builds print only the `result` event, one JSON object whose `result`
  is the text and which has no `usage_report` (seen on macOS; the build was
  not recorded). llmon then reads the text lines
  `Current session: 6% used - resets Oct 21 at 8:20pm (Region/City)` and
  `Current week (all models)` / `Current week (<model>)` (the separator is a
  middle dot). The date is `Oct 21 at 8:20pm` in one build and
  `Oct 21, 8:20pm` in another; the year is missing, the minute is rounded,
  and the zone is the machine's own, so llmon reads it in its local zone.
  The text has no extra usage and no active limit. A reset that does not
  parse leaves the percentage without a countdown. The report wins when both
  are there.
- An account without a subscription login prints neither; llmon reports "no
  usage report or limits" and tries again at the next interval.
- `/status` does not run in print mode ("isn't available in this
  environment").

llmon uses it only while the S5 snapshot is stale (older than 15 minutes) and
at most every `claude_usage_poll_secs` (default 600, floor 300).

## S5. Status-line input (documented by Claude Code)

Claude Code pipes a JSON object to the `statusLine.command` on stdin. The
relevant part:

```
"rate_limits": {           // optional: subscribers, after first API response
  "five_hour":  { "used_percentage": number, "resets_at": number },
  "seven_day":  { "used_percentage": number, "resets_at": number },
  "spend_limit":{ "used_percentage": number, "resets_at": number }  // gateway only
}
```

`resets_at` is Unix epoch seconds; each window is present only while its
reset time has not passed. The same payload also carries model, session
cost, context-window usage, and `prompt_cache` health.

S5 needs no credentials, so it is the default source; S1 is opt-in and adds
per-model weekly limits, extra usage, and the by-surface breakdown. S6 fills
in for S5 when the snapshot is stale.
