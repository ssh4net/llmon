# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

llmon merges CoMon (Codex monitor) and ClaudeMon (Claude Code monitor) into
one TUI. The design, decisions, and phases are in `PLAN.md`; keep its
"last ported commit" markers current when porting.

The repo continues CoMon's git history: `master` is CoMon `c24855d` (comon
0.5.8) renamed to llmon 0.1.0 (binary `llmon`, `~/.llmon`, `LLMON_HOME`,
`llmon.db`). The usage scanner supports Codex and Claude Code; the UI,
session history, and live limits are still Codex-only (see `PLAN.md`).

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
  `UiClickAction`. UI preferences persist in `state.json`.
- `harness.rs` - `Harness` enum (Codex, Claude) with the stable `key()` used
  in the cache and config. Per-harness behavior is dispatched with `match`,
  not trait objects.
- `providers/<harness>/` - everything specific to one CLI: home resolution,
  log parser and its `ParserState`, session owner resolution, and (Codex)
  fork replay baselines. Shared code reaches it through
  `HarnessParserState` / `HarnessParsePlan` in `usage/`.
- `ui/` - `ui::render(frame, &mut AppState)` and all drawing. Most of it lives
  in `ui/mod.rs`; ClaudeMon splits out `apistat.rs` and `cost.rs`.
- `usage/` - the shared scanner: log discovery, scan planning, and the SQLite scan
  cache (`llmon.db`). The cache stores per-file byte offsets and parser state
  so a refresh resumes mid-file. `ScanLimits` bounds each refresh by file
  count, bytes, line size, and time. The Codex parser consumes a partial last
  line (its cumulative token totals make up for a lost event, but a run can be
  missed); the Claude parser must leave it for the next refresh. Rows are keyed by `(harness, file_path)`. Each harness has
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
- `read/` - the session history screen. `scan.rs` reads session metadata and
  titles and builds a catalog grouped by project. `catalog.rs` does
  Strict/Deep/Full discovery. Deep and Full crawl only the roots listed in
  `history_project_roots`, and only after the user confirms. `tui.rs` holds
  the browser state.
- Live limits feed `AccountRateLimits`:
  - `providers/codex/rpc.rs` spawns Codex App Server and calls
    `account/rateLimits/read` over stdio JSON-RPC, with line-size,
    pending-request, and timeout caps.
  - To port from ClaudeMon: `limits/statusline.rs` is the
    `claudemon statusline [--wrap]` subcommand. It writes `limits.json` atomically, must always exit 0, and
    must never break Claude Code's status line.
  - To port from ClaudeMon: `limits/oauth.rs` reads an undocumented endpoint. It is opt-in,
    and the token is read per request.
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
  lines that repeat the same usage, so dedupe by `(message.id, requestId)`.
  Without that, totals come out 2-3x too high. Skip
  `message.model == "<synthetic>"`.
- Neither format is documented. Parsers must tolerate changes and ignore
  unknown record types.
- Token columns differ between the two:
  - CoMon: INPUT / NON-CACHED / OUTPUT. Input includes cached input.
  - ClaudeMon: INPUT / CACHE-WRITE / CACHE-READ / OUTPUT. Input excludes
    cache.

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
