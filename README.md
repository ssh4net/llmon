# llmon

Single-binary, cross-platform TUI for coding-agent CLIs: local usage stats,
live account limits, and a session-history browser.

llmon merges [CoMon](https://github.com/ssh4net/CoMon) (Codex monitor) and
[ClaudeMon](https://github.com/woffko/claudeMon) (Claude Code monitor,
derived from CoMon) into one tool with a
combined view and full per-harness views. See [PLAN.md](PLAN.md) for the
design and phases, and `CHANGELOG.md` for release history.

Status: early development. The TUI currently shows Codex only:

- Local Codex usage stats (last 7/30 days, chart, top models) by scanning `CODEX_HOME/sessions`.
- Local session-history browser grouped by project path, with session titles and prompt previews.
- Live account limits/credits by spawning Codex App Server and calling `account/rateLimits/read` when an App Server executable is available.

The horizontal Usage token chart shows three columns: **INPUT / NON-CACHED / OUTPUT**.
Input includes cached input; non-cached is input minus cached input; output is
generated tokens. Bar lengths and summary-card totals use input plus output.

## Requirements

- Rust 1.88 or newer installed (the current stable toolchain is recommended).
- C/C++ compiler toolchain available (needed to build bundled SQLite through `rusqlite`).
- Codex CLI installed as `codex` on your `PATH`, or a discoverable/explicit Codex App Server executable (required only for live limits/credits).
  - Usage stats still work without Codex CLI (they only need the session logs on disk).
- For portable Linux builds (`--musl`), install both the Rust musl target and a musl C compiler.
  - Debian/Ubuntu: `sudo apt install musl-tools`
  - Required tool for x86_64 musl builds: `x86_64-linux-musl-gcc`

Claude Code support is being added. The USAGE screen has three views (`h`
switches them):

- **Combined** (default): the Codex cards, the Claude Code cards below
  them, then the Codex chart on the left and the Claude Code chart on the
  right, with the same days in the same rows. Both charts scroll together.
  Each harness has its own color: click its chart or cards (or press `x`)
  to select it, which outlines them in its color, then pick a color swatch
  (or press `c`) to recolor it.
- **Codex** and **Claude Code**: the full single-harness screen. The Claude
  Code view shows input / cache-write / cache-read / output token columns
  (input includes the cached input, as in the Codex columns)
  and a LIMITS card laid out like the Codex one: the 5-hour limit (remaining),
  the weekly limit (used / remaining, colored by pace), the per-model limit,
  extra usage in place of credits, and the weekly gauge. An old status-line
  snapshot is noted on the card.

The same switch applies to the MODELS, COST, ACTIVITY, and HISTORY screens:

- **MODELS**: tokens per day per model as a line chart, and a card per
  model with its share, input, output, and cache split.
- **COST**: the cost of that usage by day, model, and project, with totals
  per harness in the combined view. Codex is priced at OpenAI API prices,
  which equal the Codex credit rate card at $0.04 per credit (the Codex
  view also shows credits); Claude Code at Anthropic API list prices. Both
  are API-equivalent: subscription usage is not billed per token. Models
  without their own price use their family's (for example `gpt-5.1` for
  `gpt-5.1-codex-max`), and the screen says so; models without any price
  are listed and left out. `d` cycles the dates (all, 7 days, 30 days).

The built-in prices are dated in the COST screen. To add a model or change
a price, set it under `pricing` in `config.json`, in USD per million
tokens. Cache prices that are left out follow Anthropic's multipliers of
`input` (5-minute writes 1.25x, 1-hour writes 2x, reads 0.1x):

```json
"pricing": {
  "codex": { "codex-auto-review": { "input": 1.25, "cache_read": 0.125, "output": 10.0 } },
  "claude": { "claude-opus-5-5": { "input": 4.0, "output": 20.0, "cache_read": 0.2 } }
}
```

- **ACTIVITY**: the project heatmaps of one harness with its cards, or in
  the combined view each project summed over both harnesses, without the
  cards (they are on USAGE) so more projects fit.

- **HISTORY**: the sessions of one harness, or of both grouped by project
  (a project used from Codex and Claude Code is one project), each session
  marked with its harness. The session detail shows the resume command of
  its harness (`codex resume <id>` or `claude --resume <id>`).

APISTAT and LIMITS show Codex App Server data, so they stay Codex only. The
combined USAGE view needs about 120 columns; on narrower terminals use the
single views.

## Claude Code live limits

Claude Code passes the current 5-hour and weekly limits to its status-line
command. Point it at llmon in `~/.claude/settings.json`:

```json
{
  "statusLine": { "type": "command", "command": "llmon statusline" }
}
```

To keep an existing status line, wrap it; its output is shown unchanged:

```bash
llmon statusline --wrap 'your-status-line-command'
```

The snapshot is stored in `~/.llmon/limits.json` and updates while Claude
Code runs. `--claude-limits oauth` (or `"claude_limits": "oauth"` in
`config.json`) reads the OAuth usage endpoint behind Claude Code's `/usage`
instead, which adds per-model weekly limits and extra usage; the endpoint is
undocumented and uses Claude Code's token, read per request and never stored.
`--claude-limits off` disables Claude limits. The command never fails Claude Code's status line: on any error it
prints a minimal line and exits 0. No credentials are read.

## Run

By default, `llmon` shows usage for **All workspaces** (regardless of current directory).

Press `s` / `F2` at runtime to switch between the Usage and Session history screens.

Use `--project <path>` (or `--workspace <path>`) to filter usage stats to sessions whose
working directory equals or is under that path (Codex session `cwd`).

`--cwd` controls where Codex App Server is launched and does not change usage scope.

```bash
# If installed (recommended):
llmon

# Start directly on the Session history screen:
llmon --read

# Or run from the repo without installing:
cargo run --release
```

Common flags:

- `--codex-home <path>`: override CODEX_HOME (default: `$CODEX_HOME` or `~/.codex`)
- `--llmon-home <path>`: override LLMON_HOME for llmon state/cache files (default: `$LLMON_HOME` or `~/.llmon`)
- `--print-config-path`: print effective llmon config path and exit
- `-r` / `--read`: start on the Session history screen
- `--sessions-dir <path>`: override the Codex sessions directory used by the Session history screen
- `--print-sessions-dir`: print effective sessions directory and exit
- `--codex-bin <path>`: override Codex CLI binary (spawned as `<path> app-server`)
- `--app-server-bin <path>`: override a standalone Codex App Server executable (spawned directly)
- `--live-limits <auto|on|off>`: `auto` tries App Server if found, `on` requires it, `off` disables live limits (default: `auto`)
- `--cwd <path>`: directory to launch Codex App Server in (default: current directory; does not change usage scope)
- `--project <path>` / `--workspace <path>`: filter usage stats by session working directory path (also becomes default `--cwd` if `--cwd` not set)
- `--usage-days <n>`: summary/model-share window (clamped 1..=90; default from config); charts index the complete local history
- `--refresh-usage-secs <n>`: usage refresh interval in seconds (default from config)
- `--refresh-limits-secs <n>`: limits refresh interval in seconds (default from config)
- `--max-session-file-mib <n>`: per-file planning weight (MiB) for scan budget (default from config)
- `--max-session-total-mib <n>`: max total size (MiB) to scan across session files (default from config)
- `--max-session-files <n>`: max number of session files to scan per refresh (default from config)
- `--max-jsonl-line-kib <n>`: max parsed JSONL line size in KiB (default from config)
- `--scan-time-budget-ms <n>`: max parse time budget per refresh in ms (`0` disables budget)
- `--full-scan`: process the complete pending session backlog in one refresh (ignores file/byte planning caps)
- `--no-full-scan`: disable full scan for this run (overrides config)
- `--scan-cache-max-entries <n>`: max entries kept in cache database (`llmon.db`) (default from config)
- `--rebuild-cache-on-start`: delete local scan cache DB files (`llmon.db`, `llmon.db-wal`, `llmon.db-shm`) before first usage scan

Config precedence:

- CLI flags
- `~/.llmon/config.json` (or `$LLMON_HOME/config.json`, or `--llmon-home <path>/config.json`)
- built-in defaults

`config.json` is auto-created on first run. Example:

```json
{
  "schema_version": 3,
  "usage_days": 30,
  "refresh_usage_secs": 300,
  "refresh_limits_secs": 60,
  "max_session_file_mib": 256,
  "max_session_total_mib": 256,
  "max_session_files": 10000,
  "max_jsonl_line_kib": 512,
  "scan_time_budget_ms": 1500,
  "full_scan": false,
  "scan_cache_max_entries": 50000,
  "history_project_roots": [],
  "history_deep_depth": 2,
  "history_deep_max_depth": 8
}
```

### Optional Deep repository discovery

Usage and Strict History reconstruct from Codex session logs and do not crawl
project folders. Deep/Full History discovery is opt-in: add only the developer
roots you intend to scan, for example:

```json
{
  "history_project_roots": ["/path/to/projects"]
}
```

From the History screen, press `r` or `F5` and confirm the listed roots before
llmon enumerates them. Cached Deep/Full results remain available at startup but
are marked as cached until explicitly refreshed. Do not use your whole home
directory as a discovery root unless you deliberately want llmon to inspect
all of its accessible subfolders; macOS can request access to protected folders
inside such a root.

Example:

```bash
llmon --codex-home "C:\\Users\\You\\.codex" --cwd "C:\\Repos\\some-git-repo"
```

Codex App-only Windows installs:

```bash
# Usage/session history only; avoids App Server probing.
llmon --live-limits off

# If auto-detection misses the bundled CLI-style binary:
llmon --codex-bin "C:\\Path\\To\\Codex\\codex.exe"

# If the app ships a standalone App Server binary:
llmon --app-server-bin "C:\\Path\\To\\Codex\\app-server.exe"
```

Large-log recovery/tuning example:

```bash
# One-time backfill for copied/old sessions (full reparse + cache refresh):
llmon --full-scan --scan-time-budget-ms 0

# Normal usage with bounded incremental refresh:
llmon --scan-time-budget-ms 1500 --max-jsonl-line-kib 512
```

## Key bindings

- `h` Switch the USAGE, MODELS, COST, ACTIVITY, and HISTORY view: Combined, Codex, or Claude Code (or click the pills in the header)
- `d` Cycle the dates on MODELS and COST (all time, 7 days, 30 days)
- `Tab` Toggle data (Tokens/Time/Runs)
- `g` / `w` Toggle grouping (Day/Week/Month)
- `f` Toggle layout (Horz/Vert)
- `z` / `F6` Toggle Usage zone (Local/UTC); APISTAT always uses server UTC
- `n` Cycle display formatting (Classic/System Compact/System Full)
- `x` Select Codex or Claude Code in the combined USAGE view (or click its chart or cards)
- `c` Cycle the color theme of the selected harness. Each harness keeps its own (Claude Code starts orange); the rest of the screen uses the selected harness's color
- Mouse wheel or arrow keys Scroll chart history (`PgUp`/`PgDn`, `Home`/`End` also work)
- Mouse: click the top tabs, Usage/Activity controls (including the Usage style selector), `#` bar-fill mode, color swatches, or the bottom-right Quit action
- Mouse: hover a filled vertical chart bar to see its exact date and full locale-aware value
- `s` / `F2` Switch to the next screen (USAGE, MODELS, COST, APISTAT, ACTIVITY, LIMITS, HISTORY)
- `r` / `F5` Refresh current screen
- `?` Help overlay
- `q` Quit (with confirmation)
- `Enter` / `y` Continue past "no sessions found" warning (when shown)
- Session history: `Up` / `Down` / mouse wheel navigate, `Enter` / `Right` open project sessions, `Backspace` / `Left` / `Esc` go back

## Build from source

### 1) Setup Cargo (Rust)

If you don't have `cargo` yet, install Rust via the official `rustup` installer:

Linux/macOS:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Windows:

- Download and run the installer from https://rustup.rs

Verify:

```bash
cargo --version
```

### 2) Build the app

From the repository root:

```bash
cargo build --release
```

The binary will be at:

- Windows: `target\\release\\llmon.exe`
- Linux/macOS: `target/release/llmon`

### 3) Install the app (user scope)

To run `llmon` from anywhere:

```bash
cargo install --path . --locked --force
```

This installs the binary into:

- Linux/macOS: `~/.cargo/bin`
- Windows: `%USERPROFILE%\\.cargo\\bin`

Make sure that directory is on your `PATH` (the Rust installer typically does this for you).

Optional: install into `~/.local` instead:

```bash
cargo install --path . --locked --force --root ~/.local
```

### 4) Quick install scripts (user scope)

Linux/macOS:

```bash
# Native install (default):
bash scripts/install-user.sh

# Portable Linux build/install (musl target, auto-detected arch):
# Requires musl-tools on Debian/Ubuntu.
bash scripts/install-user.sh --musl

# Explicit target example:
bash scripts/install-user.sh --musl --target x86_64-unknown-linux-musl
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\install-user.ps1
```

Optional custom install root:

- Bash: `bash scripts/install-user.sh ~/.local`
- PowerShell: `.\scripts\install-user.ps1 -Root "$HOME\\.local"`

Install script behavior:

- Installs `llmon` into the chosen user root.
- Supports optional `--target <triple>` and `--musl` build/install mode on Linux.
- Adds missing Rust target via `rustup target add` when a target is requested.
- Prepares `LLMON_HOME` (default `~/.llmon`, or `$LLMON_HOME` if set).
- Refuses to use symlink/reparse-point `LLMON_HOME` paths.

### 5) Build a prebuilt zip package (for GitHub releases)

Portable Linux release order (Debian/Ubuntu example):

```bash
# 1) Build prerequisites
sudo apt update
sudo apt install -y build-essential musl-tools zip

# 2) Rust target for portable Linux builds
rustup toolchain install stable
rustup target add x86_64-unknown-linux-musl

# 3) Build + package from repo root
bash scripts/package-prebuilt.sh --musl

# 4) Upload generated zip to GitHub Release
ls dist/llmon-v*-unknown-linux-musl.zip
```

If a musl build fails with `failed to find tool "x86_64-linux-musl-gcc"`,
install `musl-tools` and retry. Adding the Rust target with `rustup target add`
is necessary but not sufficient because bundled SQLite is compiled through a C
compiler.

Additional maintainer options:

```bash
# Linux: defaults to portable musl package
# Other OSes: defaults to host target package
bash scripts/package-prebuilt.sh

# Build portable Linux package (musl)
bash scripts/package-prebuilt.sh --musl

# Force host-target package (glibc on Linux)
bash scripts/package-prebuilt.sh --gnu
```

macOS signed release package:

```bash
# Local verification package with ad-hoc signing and no notarization:
SKIP_NOTARY=1 bash scripts/package-macos.sh

# Developer ID signed package, submitted with a stored notarytool profile:
SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
NOTARY_PROFILE="llmon-notary" \
bash scripts/package-macos.sh

# Optional explicit target:
bash scripts/package-macos.sh --target aarch64-apple-darwin
```

The macOS script builds `llmon`, signs the executable, bundles Homebrew-linked
dylibs into the package when needed, and creates:

- `dist/llmon-v<version>-<apple-target>.zip`

Signing identity and notarization profile values are read from environment
variables only; do not commit credentials or Apple account details into the repo.

Package output:

- `dist/llmon-v<version>-<target>.zip`

On Linux, prefer `*-unknown-linux-musl.zip` for maximum compatibility across distros.

Each zip includes:

- `llmon` binary
- `install.sh` (user-scope install, no Cargo needed)
- `LICENSE`, `NOTICE`, `README.txt`

### 6) Install from prebuilt zip (no compile)

User flow:

```bash
unzip llmon-v<version>-<target>.zip
cd llmon-v<version>-<target>
bash install.sh
```

Optional custom install root:

```bash
bash install.sh ~/.local
```

## Development checks

ASCII-only guardrails for docs/code/scripts:

```bash
# Run full repository check (tracked files)
bash scripts/check-ascii.sh

# Install local pre-commit hook (checks staged files on commit)
bash scripts/install-pre-commit-hook.sh
```

CI also runs this check on each push and pull request via `.github/workflows/ascii-check.yml`.

## Notes

- Usage stats are derived from Codex session JSONL logs. If you have no session data yet, values will be empty.
- Usage charts index the complete local session history and cache completed work incrementally. Until the initial backlog is complete, llmon shows an indexing status instead of partial totals.
- APISTAT displays the server-owned UTC buckets returned by Codex App Server. USAGE reconstructs local estimates from session logs, so small differences can remain even when the date range and UTC grouping match.
- Limits/credits require Codex App Server to start successfully (auth, environment, and a usable working directory). llmon auto-detects `codex` on `PATH` and common Windows Codex App bundle locations; use `--codex-bin` or `--app-server-bin` when needed.
- Weekly limit percentages are shown as used / remaining. The weekly Limits gauge shows total weekly usage and shares the weekly text's daily-allowance warning color: white below 50% consumed, yellow from 50%, orange from 70%, and red from 90%. Unused allowance carries forward across reset-anchored 24-hour periods. Its marker shows the cumulative allowance through today, not elapsed clock time; monthly gauges remain white.
- llmon stores local app state in `~/.llmon/state.json` by default (or `$LLMON_HOME`, or `--llmon-home`).
- Display formatting starts in Classic mode; press `n` or use `STYLE CLASS/SCOMP/SFULL` in the Usage controls to choose Classic, System Compact, or System Full. Both System modes use the operating system locale for dates, times, decimals, and calendar labels; Compact uses the detected thousands separator and abbreviates dashboard token values, while Full groups expanded integers with regular spaces. The choice is saved in `state.json` without changing stored data.
- Vertical chart labels preserve the selected style when they fit and compact only individual values that exceed their bar width. Hovering a filled bar shows the exact value.
- The quit dialog's `Don't show again` checkbox disables future `q`/`QUIT` confirmations after a confirmed exit. The checkbox beside `QUIT` shows that saved state; clicking it asks before enabling or disabling confirmation.
- llmon stores scan cache in `~/.llmon/llmon.db` to avoid rereading unchanged session files.
- Large session logs are parsed incrementally with persisted parser offsets in `llmon.db`; unchanged files are reused from cache.
- If historical days look incomplete after adding old session files, run once with `--full-scan --scan-time-budget-ms 0` to force a full reparse and refresh cached summaries.
- llmon uses embedded SQLite (`rusqlite` with bundled SQLite); no system `sqlite3` CLI is required at runtime.
- llmon stores user-editable runtime settings in `~/.llmon/config.json` by default.
- Privacy: llmon stores metadata (workspace paths, timestamps, token/run/time aggregates) and does not persist prompt/completion text.
- File permissions: on Unix-like systems, llmon enforces `0700` on `LLMON_HOME` and `0600` on files it writes (`config.json`, `state.json`, `llmon.db`).
- Symlink hardening: llmon refuses symlink targets for `LLMON_HOME` files (`config.json`, `state.json`, `llmon.db`, `llmon.db-wal`, `llmon.db-shm`) and rejects symlink/reparse-point `LLMON_HOME` during install scripts.
