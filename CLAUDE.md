# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

llmon merges CoMon (Codex monitor) and ClaudeMon (Claude Code monitor) into
one TUI. The design, decisions, and phases are in `PLAN.md`; keep its
"last ported commit" markers current when porting.

The repo continues CoMon's git history: `master` is CoMon `c24855d` (comon
0.5.8) renamed to llmon 0.1.0 (binary `llmon`, `~/.llmon`, `LLMON_HOME`,
`llmon.db`). Usage, session history, and the limit sources support Codex and
Claude Code. The USAGE screen has Combined, Codex, and Claude views; the other
screens are still Codex-only (see `PLAN.md`).

`_handoff/sources/` is gitignored and holds read-only reference trees, each
with its own git repo:

- `CoMon/` - the original comon (upstream github.com/ssh4net/CoMon).
- `claudeMon/` - Claude Code monitor forked from CoMon 0.5.5 (`1f94a82`); the
  source for porting Claude support. Its `PLAN.md` and `docs/sources.md`
  describe Claude Code's transcript format and limit sources.

Never commit anything from `_handoff`. `.gitignore` also excludes `/internal`
(audit scripts), `/temp`, `/dist`, and `/dist_*`. `.codex/` is the
maintainer's Codex project config.

## Commands

```bash
cargo build --release
cargo test
cargo test <name_substring>                  # run matching tests only
cargo clippy --all-targets -- -D warnings    # must stay clean (CI)
cargo fmt --check                            # rustfmt-clean (CI); run cargo fmt after edits
bash scripts/check-ascii.sh                  # ASCII guard over tracked files (CI)
bash scripts/check-ascii.sh --staged         # staged files; scripts/install-pre-commit-hook.sh
```

CI (`.github/workflows/`) runs the ASCII check, plus rustfmt, clippy, and tests on
Linux, macOS, and Windows. Run the binary against a scratch state dir with
`LLMON_HOME=<dir> cargo run` to avoid touching `~/.llmon`.

Requirements: Rust 1.88+ and a C compiler (`rusqlite` builds bundled SQLite).
Portable Linux builds (`--musl`) also need `musl-tools`
(`x86_64-linux-musl-gcc`); `rustup target add` alone is not enough.
Packaging: `scripts/package-prebuilt.sh [--musl|--gnu]`,
`scripts/package-macos.sh` (signing values from env only),
`scripts/install-user.sh` / `install-user.ps1`.

## Architecture

Single binary crate: tokio + ratatui/crossterm + rusqlite. ClaudeMon has the
same layout; differences are noted per module.

- `main.rs` - clap args and `UserConfig` (`config.json` in the app home, with
  a `schema_version` field, auto-created on first run). Precedence: CLI flags,
  then `config.json`, then built-in defaults. Resolves the app home
  (`~/.llmon`, `LLMON_HOME`, `--llmon-home`), then calls `app::run`.
- `app/mod.rs` - event loop. `run_inner` creates bounded mpsc channels (input
  events, `AppEvent`, and capacity-1 refresh triggers per worker) plus a
  `watch` shutdown channel. It spawns workers for the usage scan, live limits,
  and history catalog. Workers do blocking I/O in `spawn_blocking` and send
  `AppEvent`s back. `AppState` holds all UI state. Key events map to per-screen
  command enums (`UsageCommand`, `ActivityCommand`, ...). Each frame the renderer
  registers `UiHitTarget` rects, and mouse clicks resolve against them to a
  `UiClickAction`. UI preferences persist in `state.json`, including the
  USAGE view (`HarnessView`, key `h`) and one color theme per harness
  (`HarnessThemes`). A harness's chart uses its own theme; the rest of the
  screen uses the theme of `AppState::focused_harness` (the single view's
  harness, the harness selected in the combined view (`usage_focus`, key `x`
  or a click on its chart or cards; their outlines take its color), or Codex
  on the other screens), and the swatches and `c` edit that theme. The usage worker scans every harness
  each refresh and sends `UsageUpdated(harness, snapshot)`; a separate worker
  polls the Claude limits (status-line snapshot every 10 s, or OAuth at most
  once a minute).
- `harness.rs` - `Harness` enum (Codex, Claude) with the stable `key()` used
  in the cache and config. Per-harness behavior is dispatched with `match`,
  not trait objects.
- `providers/<harness>/` - everything specific to one CLI: home resolution,
  log parser and its `ParserState`, session owner resolution, history
  summary/detail parsing (`history.rs`), and (Codex) fork replay baselines. Shared code reaches it through
  `HarnessParserState` / `HarnessParsePlan` in `usage/`.
- `ui/` - `ui::render(frame, &mut AppState)` and all drawing. Most of it lives
  in `ui/mod.rs`; ClaudeMon splits out `apistat.rs` and `cost.rs`. The USAGE
  cards, chart, and top models draw a `UsagePanel` (harness + snapshot, kept
  as `Arc` so the chart can borrow the state mutably). Per-harness parts
  dispatch on `panel.harness`: the LIMITS card (Codex text card, or Claude
  gauge rows reusing CoMon's segmented gauges and weekly pacing) and the token
  columns (3 for Codex, 4 for Claude). The combined view stacks two labeled
  panels' card groups (one row each), then draws both charts in equal-width
  halves from `aligned_usage_days`, so the days line up and one scroll offset
  drives both. Each card group puts three cards in each of the same halves
  (`six_card_columns`), so its middle gap lines up with the chart divider.
  To see a layout without a terminal, run
  `LLMON_RENDER_DUMP_DIR=<dir> cargo test render_dump -- --ignored`: it
  renders screens from synthetic data to text files (`AppState::for_tests()`).
- `usage/` - the shared scanner: log discovery, scan planning, and the SQLite scan
  cache (`llmon.db`). The cache stores per-file byte offsets and parser state
  so a refresh resumes mid-file. `ScanLimits` bounds each refresh by file
  count, bytes, line size, and time. Parsers leave a partial last line (a
  record still being written) for the next refresh, until the log has been
  unchanged for 10 minutes (`unterminated_tail_is_final`): such a tail was
  cut off, often by a crash that left NUL padding, so it is read once (and
  skipped if it is not valid JSON) instead of keeping the file pending. Rows are keyed by `(harness, file_path)`. Each harness has
  its own cache schema version (for example `CODEX_CACHE_SCHEMA_VERSION`);
  bump it whenever that parser or its cached aggregates change meaning, and
  only that harness's rows are rebuilt. `SCAN_CACHE_DB_LAYOUT_VERSION` covers
  the table layout. Entry point: `compute_snapshot` -> `LocalUsageSnapshot`.
  `--dump-usage` (hidden; `--harness claude` for Claude Code) prints every
  snapshot aggregate as JSON; compare it before and after refactors against a
  frozen copy of real logs, including a run with a tiny
  `--scan-time-budget-ms` repeated until nothing is pending, which exercises
  resuming files mid-way.
  `usage/archive.rs` keeps the aggregates of logs that were deleted or moved
  away (`llmon-archive.db`, next to `llmon.db`). That file is durable data:
  cache schema changes and `--rebuild-cache-on-start` never touch it, and its
  JSON columns (serde formats of `DailyTotals` / `TokenBreakdown`) may only
  gain `#[serde(default)]` fields without an archive layout migration.
- `read/` - the session history screen. `scan.rs` holds the shared catalog
  types and `build_catalog(harness, dir)`, which groups sessions by project and
  nests Claude subagent transcripts under their parent; record parsing is in
  `providers/<harness>/history.rs`. `--dump-history` (hidden) prints the
  catalog as JSON for regression checks. `catalog.rs` links sessions to
  repositories from structured tool-call arguments only (Codex function calls,
  Claude `tool_use` blocks), never from prose or tool output. `catalog.rs` does
  Strict/Deep/Full discovery. Deep and Full crawl only the roots listed in
  `history_project_roots`, and only after the user confirms. `tui.rs` holds
  the browser state.
- Live limits feed `AccountRateLimits`:
  - `providers/codex/rpc.rs` spawns Codex App Server and calls
    `account/rateLimits/read` over stdio JSON-RPC, with line-size,
    pending-request, and timeout caps.
  - `providers/claude/limits/statusline.rs` is the `llmon statusline [--wrap]`
    subcommand. It writes `limits.json` atomically, must always exit 0, and
    must never break Claude Code's status line.
  - `providers/claude/limits/oauth.rs` reads an undocumented endpoint
    (field notes in `docs/sources.md`). It is opt-in, and the token is read
    per request and never stored or refreshed.
  - The Claude types (`providers::claude::limits::AccountRateLimits`) are
    separate from the Codex App Server types until the UI unifies them.
    `--dump-limits --claude-limits <statusline|oauth>` (hidden) prints them.
- `locale.rs` - Classic, System Compact, and System Full number/date
  formatting (`DisplayFormatter`). It reads the OS locale through `libc` or
  `windows-sys`.
- `storage.rs` - private-file helpers: `0700` dirs, `0600` files, atomic
  writes, and refusal of symlinks and special files. Every file the app writes
  goes through these helpers.
- `pricing.rs` (ClaudeMon) - per-model price table in `config.json`, used by
  the COST screen.

**Project identity.** A session belongs to the cwd recorded in its own log
records (`session_cwd_identity`, `resolve_session_owner`). Never derive it
from filesystem `.git` walks, Claude's lossy project-directory slug, or paths
that tools touched. See `history_refactoring.md` for why.

## Data-source gotchas

- **Codex:** logs are `CODEX_HOME/sessions/**/*.jsonl`. `session_meta.cwd`
  owns the session. `thread_settings_applied.cwd` is mutable resume metadata,
  not ownership.
- **Claude Code:** logs are `<claude-dir>/projects/<slug>/<session>.jsonl`
  plus `<session>/subagents/*.jsonl`. One API response is written as several
  lines that repeat the usage, so dedupe by `(message.id, requestId)`.
  Without that, totals come out 2-3x too high. An early line can carry a
  lower `output_tokens` than a later one, so a response counts the largest
  value of each field. Skip `message.model == "<synthetic>"`.
  `usage.input_tokens` is only the uncached input after the last cache
  breakpoint (API prompt-caching docs: total input = `input_tokens +
  cache_creation_input_tokens + cache_read_input_tokens`). Claude Code caches
  almost every prompt, so it is a few tokens per request.
- Neither format is documented. Parsers must tolerate changes and ignore
  unknown record types.
- Token columns differ between the two; INPUT includes cached input in both:
  - Codex (as in CoMon): INPUT / NON-CACHED / OUTPUT.
  - Claude: INPUT / CACHE-WRITE / CACHE-READ / OUTPUT. ClaudeMon's INPUT
    excluded cache (the raw `input_tokens`); llmon shows total input, so the
    two harnesses' INPUT columns mean the same thing.

## Rules inherited from the reference projects

- All repository text (code, comments, docs, commits) is English and
  ASCII-only. `check-ascii.sh` enforces this.
- Tests use synthetic fixtures only. Never commit real transcripts, prompts,
  session ids, tokens, local paths, user names, or real project names, in
  fixtures, docs, or configs. Use placeholders such as `/home/user/...`.
  Screenshots must come from synthetic data.
- Persist only metadata and aggregates, never prompt or completion text.
- Read credentials only in an explicit opt-in mode. Never store, log, or
  refresh them. Admin API keys come from environment variables only.
- Unit tests live in a `#[cfg(test)] mod tests` at the bottom of each module.

## Commits

- Local commits only; never push or add remotes.
- Use the maintainer's git identity. No `Co-Authored-By` lines; the only
  trailer is `Assisted-by: Claude Code / Claude Opus 5.5` after the body.
- One logical change per commit, with a meaningful summary line and body.
