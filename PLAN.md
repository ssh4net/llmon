# llmon work plan

llmon merges CoMon (Codex monitor) and ClaudeMon (Claude Code monitor, forked
from CoMon 0.5.5) into one single-binary TUI for several coding-agent
harnesses. It has a combined view and a full view for each harness.

Reference sources: `_handoff/sources/CoMon` (HEAD `c24855d`, comon 0.5.8) and
`_handoff/sources/claudeMon` (HEAD `c1fac48`). The directory is gitignored.

## Decisions

| # | Topic | Decision | Status |
|---|-------|----------|--------|
| D1 | Code base | Start from CoMon HEAD; port the Claude parts from ClaudeMon | decided |
| D2 | Views | Combined view (ALL) plus full per-harness views, switched by a header button and a key | decided |
| D3 | Claude limits | ClaudeMon's APISTAT LIMITS view becomes the Claude LIMITS card | decided |
| D4 | Cost | COST for every harness, combined and per harness | decided |
| D5 | Activity, History | Combined and per harness | decided |
| D6 | More harnesses | Gemini CLI, Grok CLI, GitHub Copilot CLI, after Codex + Claude ship | decided |
| D7 | Git history | Build on CoMon's history (86 commits) | decided |
| D8 | Deleted logs | Keep stats of deleted or moved logs for every harness (`llmon-archive.db`) | decided |
| D9 | APISTAT tab | Keep CoMon's APISTAT as the Codex server-history screen, or fold it into cards | open |
| D10 | Copilot | Needs a machine with Copilot CLI to capture fixtures | open |
| D11 | Retention | How long to keep usage history; until decided, forever (each archived row records when its log disappeared) | open |

Why D1: ClaudeMon is CoMon 0.5.5 renamed. CoMon added only 2 commits since
the fork, both UI work. The history browser (`read/catalog.rs`,
`read/tui.rs`) is identical in both projects. The Claude code is mostly in
separate files (`limits/`, `pricing.rs`, `ui/apistat.rs`, `ui/cost.rs`).
Building on ClaudeMon would mean restoring `codex_rpc`, the APISTAT server
charts, reset credits, and multi-project attribution, then redoing the
newer CoMon UI work.

---

## 1. Harnesses and data sources

| Harness | Local data | Tokens | Project | Title | Limits | Cost | Checked |
|---------|-----------|--------|---------|-------|--------|------|---------|
| Codex | `~/.codex/sessions/**/*.jsonl` | `token_count` events, cumulative | `session_meta.cwd` | CoMon logic | App Server; also `rate_limits` inside `token_count` events | Codex rate card (credits) | in CoMon |
| Claude | `~/.claude/projects/<slug>/*.jsonl` + `subagents/` | per response, deduplicated | record `cwd` | `ai-title`, `custom-title` | status line (default), OAuth (opt-in) | list prices; `cost-state` cross-check | in ClaudeMon |
| Gemini CLI | `~/.gemini/tmp/<hash>/chats/session-*.json` | per message `tokens{input,output,cached,thoughts,tool,total}` + `model` | dir name is sha256(project path) | first user message | none found | price table | schema checked locally |
| Grok CLI | `~/.grok/sessions/<url-encoded cwd>/<id>/` | `updates.jsonl` `turn_completed` usage, per model | `summary.json` `info.cwd` | `summary.json` `generated_title` | none found | recorded `costUsdTicks` | schema checked locally |
| Copilot CLI | `~/.copilot/session-state/<id>/events.jsonl`, `~/.copilot/session-store.db` | `session.shutdown` `modelMetrics`; per-request rows in `assistant_usage_events` | `workspace.yaml` | session-store summaries | premium requests | premium requests | docs only |

Gotchas by harness:

- **Codex.** `token_count` carries cumulative `total_token_usage`:
  - fields: `input_tokens` (includes cached), `cached_input_tokens`,
    `output_tokens`, `reasoning_output_tokens`
  - CoMon's fork-replay logic stays as is.
  - The same events also carry `rate_limits` (primary/secondary windows,
    credits, `plan_type`). That gives an offline limits fallback when the App
    Server cannot start.
- **Claude.** One API response is written as several lines that repeat the
  same usage, so dedupe by `(message.id, requestId)`. Skip `<synthetic>`.
  Transcripts are deleted after `cleanupPeriodDays` (30 by default). See
  ClaudeMon `PLAN.md` and `docs/sources.md`.
- **Gemini.**
  - Session files are JSON that gets rewritten in place, so byte-offset
    resume does not apply. Reparse a file whenever its size or mtime changes.
  - One session can span several files with repeated messages (20 of 781
    token-bearing message ids in the local sample). Dedupe by
    `(sessionId, message id)`.
  - The project directory name is sha256 of the project path. Resolve it by
    hashing known paths (cwds from other harnesses, history roots, the
    current dir); all 7 local hashes matched this way. Unresolved projects
    show as `gemini:<hash8>`.
  - To verify: whether `input` includes `cached`, and whether `thoughts` is
    separate from `output`. Both are true in the Gemini API.
- **Grok.**
  - Each usage record has: input, output, cache read, cache creation,
    reasoning, `modelCalls`, `apiDurationMs`, `costUsdTicks`, and
    `modelUsage` per model.
  - To verify: whether usage is per turn or cumulative. Values drop within a
    session, which suggests per turn.
  - To verify: the tick unit. 1e-10 USD per tick gives a plausible cost on
    the local sample.
- **Copilot.** Known only from third-party docs (ccusage, codeburn).
  - `session.shutdown` gives totals only once a session ends.
  - `session-store.db` `assistant_usage_events` has per-request rows while a
    session runs.
  - Billing is in premium requests, not tokens.
  - Capture real fixtures before implementing.

## 2. Token model

One canonical struct for every harness:

| Field | Codex | Claude | Gemini | Grok | Copilot |
|-------|-------|--------|--------|------|---------|
| `input_uncached` | input - cached | `input_tokens` | input - cached (verify) | inputTokens (verify) | inputTokens - cacheRead - cacheWrite |
| `cache_write` | 0 | `cache_creation_input_tokens` | 0 | cacheCreationTokens | cacheWriteTokens |
| `cache_write_1h` | 0 | `ephemeral_1h` share | 0 | 0 | 0 |
| `cache_read` | `cached_input_tokens` | `cache_read_input_tokens` | cached | cachedReadTokens | cacheReadTokens |
| `output` | `output_tokens` | `output_tokens` | output + thoughts (verify) | outputTokens | output |
| `reasoning` (part of output, display only) | `reasoning_output_tokens` | thinking tokens | thoughts | reasoningTokens | reasoning |

Total = input_uncached + cache_write + cache_read + output. This equals
CoMon's input + output and ClaudeMon's four-column sum, so existing numbers
do not change. Columns are chosen per view:

- Codex view: CoMon's 3 columns (INPUT / NON-CACHED / OUTPUT), derived from
  the canonical struct.
- Every other view: 4 columns (INPUT / CACHE-WRITE / CACHE-READ / OUTPUT).

## 3. Cost

| Harness | Source | Shown as |
|---------|--------|----------|
| Codex | OpenAI Codex rate card: credits per 1M tokens for input, cached input, and output; 1 credit = USD 0.04 | credits and USD |
| Claude | List price table; Claude Code `cost-state` records as a cross-check | USD, API-equivalent |
| Gemini | Price table | USD, API-equivalent |
| Grok | Recorded `costUsdTicks` | USD, recorded |
| Copilot | Premium requests from model metrics; optional price table | premium requests (USD when priced) |

Notes:

- **Price tables** live in `config.json`, one per harness, each with an
  `updated` date. Model matching follows ClaudeMon: exact id, then id
  without a date suffix, then the longest family prefix. Unknown models are
  listed as unpriced.
- **Labels.** Every view says whether a cost is recorded, a rate card, or
  API-equivalent, and that subscription usage is not billed per token.
- **Combined COST:**
  - a total per harness, in USD
  - daily bars stacked by harness color
  - per-model rows, labeled with the harness
  - per-project rows across harnesses
- **Per-harness COST** is ClaudeMon's COST screen, with that harness's cost
  source.

## 4. UI

### 4.1 Harness switch

- A clickable `HARNESS [ALL] CODEX CLAUDE ...` control sits in the header.
  It lists only harnesses that are enabled and have data.
- `h` / `H` cycle forward and back. `h` is unbound in both CoMon and
  ClaudeMon.
- One global selection applies to every tab and is saved in `state.json`.

### 4.2 Tabs

| Tab | ALL | One harness |
|-----|-----|-------------|
| USAGE | One card row per harness, then per-harness charts side by side (4.3) | CoMon's USAGE screen as it is today |
| MODELS | Tokens per day per model across harnesses (series colored and labeled by harness) | ClaudeMon's MODELS view for that harness |
| COST | Section 3, combined | Section 3, one harness |
| ACTIVITY | Heatmaps of summed activity; project rows across harnesses | CoMon's ACTIVITY for that harness |
| HISTORY | Projects grouped by path across harnesses; each session shows a harness badge and its resume command | Sessions of that harness only |
| APISTAT | Depends on D9 | Codex: CoMon's server charts, credits, and reset credits |

ClaudeMon's MODELS view works for any harness with local per-model data, so
it moves out of APISTAT into its own tab.

Resume commands are `codex resume <id>`, `claude --resume <id>`, and
`grok -r <id>`. Check the Gemini and Copilot commands during the spike.

### 4.3 Combined USAGE layout

```
llmon :: 0.1.0     HARNESS [ALL] CODEX CLAUDE     USAGE MODELS COST ACTIVITY HISTORY
VIEW TOKENS TIME RUNS   GRAPH WEEK MONTH   STYLE CLASS SCOMP SFULL
+ CODEX -------------------------------------------------------------------------+
| LIMITS        | TODAY         | LAST 7 DAYS   | LAST 30 DAYS  | COST 30D       |
| 5h 12% 7d 86% | 1.2B tokens   | 13 165 runs   | 45 418 runs   | 9 120 cr       |
| credits 1 000 | runs 912      | 17.9B tokens  | 23.6B tokens  | USD 364.80     |
+ CLAUDE ------------------------------------------------------------------------+
| LIMITS        | TODAY         | LAST 7 DAYS   | LAST 30 DAYS  | COST 30D       |
| 5h 40% 7d 55% | 420M tokens   | 2 104 runs    | 8 950 runs    | USD 812.40     |
| Opus wk 61%   | runs 140      | 3.1B tokens   | 11.2B tokens  | API-equivalent |
+ CODEX  last 30 days        TOKENS -+ CLAUDE  last 30 days       TOKENS -+
| 09/05 ##############          1.2B | 09/05 ######                420M |
| 09/06 ########                0.7B | 09/06 #########             610M |
| ...                                | ...                               |
| TOP gpt-5.5 81% gpt-5.4 13%        | TOP Opus 5.5 70% Sonnet 5 25%     |
+------------------------------------+-----------------------------------+
```

- **Shared grid.** All card rows use the same column grid, so each card
  lines up with the same card for the other harnesses.
- **Claude LIMITS card** (D3) shows:
  - the 5h and weekly windows, with the weekly pace gauge
  - the most-used per-model weekly limit
  - extra usage when it is enabled

  Clicking the card opens that harness's full view, which also shows every
  per-model limit, usage by surface, the binding limit, the source, and the
  capture age.
- **Weekly pace.** Every harness uses CoMon's newer pace logic (daily
  allowance with carryover). ClaudeMon has the older elapsed-share version.

### 4.4 More than two harnesses

- **Card rows.** Show one card row per harness with data in the range. When
  the terminal is too short, rows collapse to a one-line summary
  (`CODEX 5h 12% 7d 86% | today 1.2B | 30d 23.6B | 9 120 cr`).
- **Charts.** Charts sit side by side while each one is at least about 40
  columns wide. Below that, one combined chart shows bars stacked by
  harness color.
- **Harness colors.** Each harness has a fixed color, used on card borders,
  chart series, and history badges. The colors must stay readable under all
  accent themes, so they are kept separate from the theme accent.

### 4.5 Polish backlog

Known items:

- port CoMon's 2 post-fork UI commits (they come with D1)
- ClaudeMon's clearer empty and not-configured states, generalized for
  every harness
- `ctrl+s` table copy (OSC 52) on MODELS and COST
- footer hints and help overlay updated for the harness switch

The maintainer adds the rest.

## 5. Architecture

- **Providers.** Each harness lives in `src/providers/<harness>/`:
  - discovery
  - the parser and its saved state
  - session owner resolution
  - history metadata
  - limit sources
  - cost source

  Dispatch uses a plain `enum Harness { Codex, Claude, Gemini, Grok,
  Copilot }` with `match`, not trait objects.
- **Shared code:**
  - the event loop and workers
  - the UI
  - scan scheduling and budgets
  - the SQLite cache
  - the history browser (`read/catalog.rs`, `read/tui.rs`)
  - `locale.rs`, `storage.rs`
  - cwd identity (`session_cwd_identity`, `normalize_project_key`)
- **Parse modes:**
  - append-only JSONL with byte-offset resume: Codex, Claude, Grok
  - whole-file JSON reparsed on change: Gemini
  - SQLite query: Copilot `session-store.db`
- **Cache (`llmon.db`):** (layout done in phase 1)
  - `file_cache` rows are keyed by `(harness, file_path)`.
  - `cache_meta` key `layout_version` covers the table layout; each harness
    has its own schema version (`schema_version.<harness>`), so a parser
    change rebuilds only that harness.
  - The aggregates of logs that no longer exist move to a separate file,
    `llmon-archive.db` (table `archived_usage`, done in phase 2). Cache
    schema changes and `--rebuild-cache-on-start` never touch it.
  - ClaudeMon's current `DELETE FROM file_cache` on a version change would
    lose that history; llmon must not repeat this.
- **Scan budget.** The per-refresh budget is split across harnesses, so a
  large backlog in one harness cannot starve the others. First-run indexing
  status is shown per harness.
- **Config:**
  - each harness has `harnesses.<name>.enabled` (`auto` | `on` | `off`;
    `auto` turns it on when its home dir exists)
  - an optional home override per harness
  - live-limit settings per harness: Codex `auto|on|off`, Claude
    `statusline|oauth|off`
  - one price table per harness
- **Status line.** The `llmon statusline [--wrap]` subcommand replaces
  `claudemon statusline`. It always exits 0 and must never break Claude
  Code's status line.

## 6. Migration from comon and claudemon

| Source | Unique data | Action |
|--------|-------------|--------|
| `~/.comon/comon.db` | none (rows of missing logs are dropped) | do not import; rescan `~/.codex/sessions` |
| `~/.claudemon/claudemon.db` | rows of deleted transcripts | copy rows whose file is gone into `llmon-archive.db` (harness = claude); rescan the rest |
| `config.json` (both) | settings | merge into the per-harness layout; `history_project_roots` is the union |
| `~/.comon/state.json` | UI preferences | copy |
| `~/.claudemon/limits.json` | latest limits snapshot | read as a fallback until llmon writes its own |
| `statusLine` in `~/.claude/settings.json` | points to `claudemon statusline` | print the change, or edit only after the user confirms |

Rules:

- **Command.** Migration runs through `llmon migrate [--dry-run]`, and llmon
  offers it on first run when an old home dir exists.
- **Read-only.** Old databases are opened read-only and parsed directly.
  Never open them with ClaudeMon's `open_or_init` path, because it deletes
  rows on a version mismatch.
- **Safe to repeat.** Each import is recorded in `cache_meta`, so running it
  again does nothing.
- **Optional extra history.** Claude Code's `~/.claude/stats-cache.json`
  (tokens per model per day) can fill days older than the oldest transcript.
  Those days are marked as totals only.

Until the migration ships, ClaudeMon should not change its cache schema
version, because that clears its saved history of deleted transcripts.

## 7. Phases

| Phase | Work | Size |
|-------|------|------|
| 0 | Done: repo setup per D7; rename comon to llmon (crate, binary, `~/.llmon`, `LLMON_HOME`, `llmon.db`); CI (ASCII check, `cargo test`, clippy on Linux, macOS, Windows) | S |
| 1 | Done: provider seam with Codex only, no behavior change. `Harness` enum; cache keyed by `(harness, file_path)` with per-harness schema versions; canonical `TokenBreakdown`; Codex parser, owner resolver, and App Server client in `providers/codex`. Tests green (183: the comon migration tests were replaced). `--dump-usage` output on frozen real logs is identical at every step | L |
| 2 | In progress. Done: usage of deleted logs kept in `llmon-archive.db`; Claude usage parser in `providers/claude` (output identical to ClaudeMon on frozen real logs, including incremental resume); NOTICE credits ClaudeMon. Remaining: split session history (`read/scan.rs` record parsing, `catalog.rs` tool-call evidence) into shared code and `providers/codex/history.rs`, designed together with the Claude version (subagent nesting). Claude provider: usage parser, owner, history scan, status-line and OAuth limits, pricing, model names; port ClaudeMon's 156 tests | M |
| 3 | Harness switch, combined USAGE (4.3), Claude LIMITS card, MODELS tab | M |
| 4 | COST per harness and combined (Codex rate card, Claude prices); ACTIVITY and HISTORY combined | M |
| 5 | `llmon migrate`, `archived_usage`, optional `stats-cache.json` import | S |
| 6 | Gemini and Grok providers (fixtures from local logs), then Copilot (D10) | M each |
| 7 | Polish backlog, packaging scripts, README with synthetic screenshots, release | S |

While phases 1-2 are in progress, CoMon and ClaudeMon take bug fixes only.
This file records the last ported commit of each:

- CoMon: `c24855d`
- ClaudeMon: `c1fac48`

## 8. Testing

- **Fixtures.** Synthetic fixtures only, per harness, generated from real
  logs with all text replaced and only structure and numbers kept. Real
  logs, prompts, session ids, and paths are never committed.
- **Golden test.** The hidden `--dump-usage` flag prints every snapshot
  aggregate as JSON. Run it on a frozen local copy of real logs (cold and
  warm cache, unfiltered and with `--project` filters) before and after each
  refactor; the output must be identical. Run locally only; the logs and the
  output are not committed.
- **Per-harness tests:**
  - deduplication (Claude, Gemini)
  - incremental resume
  - truncated last line
  - unknown record types
  - project resolution (Gemini hash matching)
  - cost against recorded values (Claude `cost-state`, Grok `costUsdTicks`)
- **Migration tests.** Synthetic comon and claudemon databases at their
  current schema versions; repeated runs; read-only access to the sources.

## 9. Risks

| Risk | Mitigation |
|------|------------|
| Every log format is undocumented | Tolerant parsers, unknown records ignored, a fixture per observed version, per-harness schema version |
| The phase 1 refactor of `usage/mod.rs` (5.6k lines) changes Codex numbers | Golden comparison against comon before any Claude work |
| Combined layout too dense on small terminals | One-line card rows and a stacked chart fallback (4.4) |
| Prices drift | Tables in config with an `updated` date; recorded costs preferred where they exist |
| Copilot data only known from third-party docs | Phase 6 waits for real fixtures (D10) |
| Both upstreams keep changing during the merge | Bug-fix-only freeze; ported-commit markers in this file |
| Codex parser consumes a partial last line of a live log (inherited from comon) | Cumulative token totals compensate; an agent run can be missed. Fix with the shared line reader in phase 2 |
