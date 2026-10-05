mod app;
mod harness;
mod locale;
mod pricing;
mod providers;
mod read;
mod storage;
mod ui;
mod usage;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;

const USER_CONFIG_SCHEMA_VERSION: u32 = 5;
const USER_CONFIG_FILE_NAME: &str = "config.json";
const DEFAULT_USAGE_DAYS: u32 = 30;
const DEFAULT_REFRESH_USAGE_SECS: u64 = 300;
const DEFAULT_REFRESH_LIMITS_SECS: u64 = 60;
const DEFAULT_MAX_SESSION_FILE_MIB: u64 = 256;
const DEFAULT_MAX_SESSION_TOTAL_MIB: u64 = 256;
const DEFAULT_MAX_SESSION_FILES: usize = 10_000;
const DEFAULT_MAX_JSONL_LINE_KIB: u64 = 512;
const DEFAULT_SCAN_TIME_BUDGET_MS: u64 = 1500;
const DEFAULT_HISTORY_CATALOG_MAX_CANDIDATES: usize = 10_000;
const DEFAULT_HISTORY_CATALOG_SCAN_BUDGET_MS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct UserConfig {
    schema_version: u32,
    usage_days: u32,
    refresh_usage_secs: u64,
    refresh_limits_secs: u64,
    max_session_file_mib: u64,
    max_session_total_mib: u64,
    max_session_files: usize,
    max_jsonl_line_kib: u64,
    scan_time_budget_ms: u64,
    full_scan: bool,
    scan_cache_max_entries: usize,
    history_project_roots: Vec<PathBuf>,
    history_deep_depth: u8,
    history_deep_max_depth: u8,
    history_catalog_max_candidates: usize,
    history_catalog_scan_budget_ms: u64,
    /// Claude Code limits source: "statusline" (default), "oauth", or "off".
    claude_limits: ClaudeLimitsArg,
    /// Prices that add to or override the built-in tables, per harness:
    /// model id -> USD per million tokens.
    pricing: pricing::PricingOverrides,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            schema_version: USER_CONFIG_SCHEMA_VERSION,
            usage_days: DEFAULT_USAGE_DAYS,
            refresh_usage_secs: DEFAULT_REFRESH_USAGE_SECS,
            refresh_limits_secs: DEFAULT_REFRESH_LIMITS_SECS,
            max_session_file_mib: DEFAULT_MAX_SESSION_FILE_MIB,
            max_session_total_mib: DEFAULT_MAX_SESSION_TOTAL_MIB,
            max_session_files: DEFAULT_MAX_SESSION_FILES,
            max_jsonl_line_kib: DEFAULT_MAX_JSONL_LINE_KIB,
            scan_time_budget_ms: DEFAULT_SCAN_TIME_BUDGET_MS,
            full_scan: false,
            scan_cache_max_entries: usage::DEFAULT_SCAN_CACHE_MAX_ENTRIES,
            // Repository discovery is intentionally opt-in. Session history and
            // usage are reconstructed from Codex logs without crawling $HOME.
            history_project_roots: Vec::new(),
            history_deep_depth: read::catalog::DEFAULT_DEEP_DEPTH,
            history_deep_max_depth: read::catalog::MAX_DEEP_DEPTH,
            history_catalog_max_candidates: DEFAULT_HISTORY_CATALOG_MAX_CANDIDATES,
            history_catalog_scan_budget_ms: DEFAULT_HISTORY_CATALOG_SCAN_BUDGET_MS,
            claude_limits: ClaudeLimitsArg::Statusline,
            pricing: pricing::PricingOverrides::default(),
        }
    }
}

fn validate_dir(path: &std::path::Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    // Best-effort canonicalization to normalize `..` and symlinks.
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn resolve_workspace_filter(project_override: Option<&Path>) -> Option<PathBuf> {
    // Codex-native path filter: sessions whose session cwd equals or is under this path.
    // No filesystem .git discovery.
    let project_candidate = project_override?;
    Some(
        std::fs::canonicalize(project_candidate).unwrap_or_else(|_| {
            if project_candidate.is_absolute() {
                project_candidate.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(project_candidate))
                    .unwrap_or_else(|_| project_candidate.to_path_buf())
            }
        }),
    )
}

fn resolve_llmon_home(override_home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_home {
        return Some(path);
    }
    if let Ok(value) = std::env::var("LLMON_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(value) = std::env::var("HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".llmon"));
        }
    }
    if let Ok(value) = std::env::var("USERPROFILE") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".llmon"));
        }
    }
    None
}

fn load_or_bootstrap_user_config(llmon_home: &Path) -> Result<UserConfig> {
    let path = llmon_home.join(USER_CONFIG_FILE_NAME);
    if !path.exists() {
        let defaults = UserConfig::default();
        let encoded = serde_json::to_vec_pretty(&defaults).with_context(|| {
            format!("Unable to encode default user config at {}", path.display())
        })?;
        crate::storage::write_private_file(&path, &encoded)?;
        return Ok(defaults);
    }

    crate::storage::enforce_private_file_if_exists(&path)?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("Unable to read user config {}", path.display()))?;
    let mut config = serde_json::from_slice::<UserConfig>(&bytes)
        .with_context(|| format!("Unable to parse user config {}", path.display()))?;
    if config.schema_version > USER_CONFIG_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported llmon config schema version: {} (maximum {})",
            config.schema_version,
            USER_CONFIG_SCHEMA_VERSION
        );
    }
    if config.schema_version < USER_CONFIG_SCHEMA_VERSION {
        config.schema_version = USER_CONFIG_SCHEMA_VERSION;
        let encoded = serde_json::to_vec_pretty(&config)
            .with_context(|| format!("Unable to migrate user config {}", path.display()))?;
        crate::storage::write_private_file(&path, &encoded)?;
    }
    Ok(config)
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HarnessArg {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LiveLimitsArg {
    Auto,
    On,
    Off,
}

impl From<LiveLimitsArg> for app::LiveLimitsMode {
    fn from(value: LiveLimitsArg) -> Self {
        match value {
            LiveLimitsArg::Auto => Self::Auto,
            LiveLimitsArg::On => Self::On,
            LiveLimitsArg::Off => Self::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ClaudeLimitsArg {
    /// Read the snapshot `llmon statusline` records (no credentials).
    Statusline,
    /// Call the OAuth usage endpoint with Claude Code's token (opt-in).
    Oauth,
    /// No Claude Code limits.
    Off,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Status-line bridge: set Claude Code's `statusLine.command` to
    /// `llmon statusline`. Records the rate limits Claude Code reports and
    /// prints a compact status line.
    Statusline {
        /// Existing status-line command to run with the same input; its output
        /// is printed instead of the built-in line.
        #[arg(long)]
        wrap: Option<String>,
    },
}

#[derive(Debug, Parser)]
#[command(
    name = "llmon",
    version,
    about = "Usage, limits, and session history TUI for coding-agent CLIs"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Launch with the session history screen active.
    #[arg(short = 'r', long = "read")]
    read_mode: bool,

    /// Override path to the Codex CLI binary (default: `codex` in PATH).
    #[arg(long)]
    codex_bin: Option<String>,

    /// Override a standalone Codex App Server executable (spawned directly).
    #[arg(long, conflicts_with = "codex_bin")]
    app_server_bin: Option<PathBuf>,

    /// Live limits behavior: auto tries App Server if found, on requires it, off disables it.
    #[arg(long, value_enum, default_value = "auto")]
    live_limits: LiveLimitsArg,

    /// Override CODEX_HOME (default: $CODEX_HOME or ~/.codex).
    #[arg(long)]
    codex_home: Option<PathBuf>,

    /// Override the Claude Code config directory (default: $CLAUDE_CONFIG_DIR or ~/.claude).
    #[arg(long)]
    claude_dir: Option<PathBuf>,

    /// Override LLMON_HOME for llmon-owned state/cache files (default: $LLMON_HOME or ~/.llmon).
    #[arg(long)]
    llmon_home: Option<PathBuf>,

    /// Print effective llmon config path and exit.
    #[arg(long)]
    print_config_path: bool,

    /// Override the sessions directory directly for read mode.
    #[arg(long)]
    sessions_dir: Option<PathBuf>,

    /// Print the effective sessions directory for read mode and exit.
    #[arg(long)]
    print_sessions_dir: bool,

    /// Working directory to launch Codex App Server in (default: current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Filter usage stats to sessions whose working directory equals or is under this path.
    ///
    /// If `--cwd` is not provided, this also becomes the default working directory for
    /// launching Codex App Server.
    #[arg(long, alias = "workspace")]
    project: Option<PathBuf>,

    /// Number of days to scan for local usage (clamped to 1..=90; default from config).
    #[arg(long)]
    usage_days: Option<u32>,

    /// Periodic refresh interval for usage stats in seconds (default from config).
    #[arg(long)]
    refresh_usage_secs: Option<u64>,

    /// Periodic refresh interval for limits/credits in seconds (default from config).
    #[arg(long)]
    refresh_limits_secs: Option<u64>,

    /// Per-file scan budget weight in MiB used by planner (default from config).
    ///
    /// Large files are still supported via incremental parsing and cache offsets.
    #[arg(long)]
    max_session_file_mib: Option<u64>,

    /// Max total size in MiB to scan across session files (default from config).
    #[arg(long)]
    max_session_total_mib: Option<u64>,

    /// Max number of session files to scan per refresh (default from config).
    #[arg(long)]
    max_session_files: Option<usize>,

    /// Max size in KiB of one JSONL line parsed from session files (default from config).
    #[arg(long)]
    max_jsonl_line_kib: Option<u64>,

    /// Max parse budget in milliseconds per refresh (0 = unlimited; default from config).
    #[arg(long)]
    scan_time_budget_ms: Option<u64>,

    /// Scan all session files under CODEX_HOME/sessions (ignore mtime cutoff; overrides config).
    #[arg(long, conflicts_with = "no_full_scan")]
    full_scan: bool,

    /// Disable full session scan even if enabled in config.
    #[arg(long)]
    no_full_scan: bool,

    /// Max number of entries to keep in scan cache (default from config).
    #[arg(long)]
    scan_cache_max_entries: Option<usize>,

    /// Rebuild local scan cache on startup (delete `llmon.db` before first usage scan).
    #[arg(long)]
    rebuild_cache_on_start: bool,

    /// Compute the usage snapshot once through the scan cache, print its
    /// aggregates as JSON, and exit. Used to check that refactors keep every
    /// total unchanged.
    #[arg(long, hide = true)]
    dump_usage: bool,

    /// Build the session history catalog once, print it with each session's
    /// detail counts as JSON, and exit.
    #[arg(long, hide = true)]
    dump_history: bool,

    /// Fetch the Claude Code limits once from the --claude-limits source,
    /// print them as JSON, and exit.
    #[arg(long, hide = true)]
    dump_limits: bool,

    /// Claude Code limits source (default from config: statusline).
    #[arg(long, value_enum)]
    claude_limits: Option<ClaudeLimitsArg>,

    /// Harness for --dump-usage, --dump-history, and --print-sessions-dir.
    #[arg(long, value_enum, default_value = "codex", hide = true)]
    harness: HarnessArg,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(Command::Statusline { wrap }) = &args.command {
        // Must stay fast and must never fail Claude Code's status line.
        let home = resolve_llmon_home(args.llmon_home.clone());
        providers::claude::limits::statusline::run(home.as_deref(), wrap.as_deref());
        return Ok(());
    }

    if args.dump_limits {
        let limits = match args.claude_limits.unwrap_or(ClaudeLimitsArg::Statusline) {
            ClaudeLimitsArg::Statusline => {
                let home = resolve_llmon_home(args.llmon_home.clone())
                    .context("Unable to resolve LLMON_HOME (default: ~/.llmon)")?;
                providers::claude::limits::statusline::load(
                    &home,
                    providers::claude::limits::unix_now(),
                )?
            }
            ClaudeLimitsArg::Oauth => {
                let claude_dir = providers::claude::resolve_claude_dir(args.claude_dir.clone())
                    .context("Unable to resolve the Claude Code config directory")?;
                Some(providers::claude::limits::oauth::fetch(&claude_dir)?)
            }
            ClaudeLimitsArg::Off => None,
        };
        let json = limits
            .as_ref()
            .map(providers::claude::limits::limits_dump_json)
            .unwrap_or(serde_json::Value::Null);
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }

    if args.print_config_path {
        let llmon_home = resolve_llmon_home(args.llmon_home.clone())
            .context("Unable to resolve LLMON_HOME (default: ~/.llmon)")?;
        println!("{}", llmon_home.join(USER_CONFIG_FILE_NAME).display());
        return Ok(());
    }

    let selected_harness = match args.harness {
        HarnessArg::Codex => harness::Harness::Codex,
        HarnessArg::Claude => harness::Harness::Claude,
    };
    let selected_home = match selected_harness {
        harness::Harness::Codex => args.codex_home.clone(),
        harness::Harness::Claude => args.claude_dir.clone(),
    };
    if args.print_sessions_dir {
        read::print_sessions_dir(selected_harness, selected_home, args.sessions_dir.clone())?;
        return Ok(());
    }
    if args.dump_history {
        let config =
            read::build_config(selected_harness, selected_home, args.sessions_dir.clone())?;
        let catalog = read::scan::build_catalog(config.harness, &config.sessions_dir)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&read::scan::catalog_dump_json(&catalog))?
        );
        return Ok(());
    }
    let read_config = read::build_config(
        harness::Harness::Codex,
        args.codex_home.clone(),
        args.sessions_dir.clone(),
    )?;

    let launch_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // `--project` controls usage scope.
    let cwd_override = args
        .cwd
        .clone()
        .map(|p| validate_dir(&p, "--cwd"))
        .transpose()?;
    let project_override = args
        .project
        .clone()
        .map(|p| validate_dir(&p, "--project"))
        .transpose()?;

    // Default to all workspaces unless user explicitly provides `--project`.
    let project = resolve_workspace_filter(project_override.as_deref());

    // `cwd` controls where `codex app-server` is launched.
    let cwd = cwd_override
        .or_else(|| project.clone())
        .unwrap_or_else(|| launch_dir.clone());
    let llmon_home = resolve_llmon_home(args.llmon_home.clone())
        .context("Unable to resolve LLMON_HOME (default: ~/.llmon)")?;
    crate::storage::ensure_private_dir(&llmon_home)?;
    let user_config = load_or_bootstrap_user_config(&llmon_home)?;
    let codex_home = providers::codex::resolve_codex_home(args.codex_home.clone())
        .context("Unable to resolve CODEX_HOME")?;

    let usage_days = args
        .usage_days
        .unwrap_or(user_config.usage_days)
        .clamp(1, 90);
    let refresh_usage_secs = args
        .refresh_usage_secs
        .unwrap_or(user_config.refresh_usage_secs)
        .max(30);
    let refresh_limits_secs = args
        .refresh_limits_secs
        .unwrap_or(user_config.refresh_limits_secs)
        .max(10);
    let full_scan = if args.full_scan {
        true
    } else if args.no_full_scan {
        false
    } else {
        user_config.full_scan
    };
    let max_session_total_mib = args
        .max_session_total_mib
        .unwrap_or(user_config.max_session_total_mib)
        .max(1);
    let max_session_file_mib = args
        .max_session_file_mib
        .unwrap_or(user_config.max_session_file_mib)
        .max(1)
        .min(max_session_total_mib);
    let max_session_files = args
        .max_session_files
        .unwrap_or(user_config.max_session_files)
        .max(1);
    let max_jsonl_line_kib = args
        .max_jsonl_line_kib
        .unwrap_or(user_config.max_jsonl_line_kib)
        .max(1);
    let scan_time_budget_ms = args
        .scan_time_budget_ms
        .unwrap_or(user_config.scan_time_budget_ms);
    let scan_cache_max_entries = args
        .scan_cache_max_entries
        .unwrap_or(user_config.scan_cache_max_entries)
        .max(1);

    let max_session_total_bytes = max_session_total_mib.saturating_mul(1024 * 1024);
    let max_session_file_bytes = max_session_file_mib
        .saturating_mul(1024 * 1024)
        .min(max_session_total_bytes);
    let max_jsonl_line_bytes =
        usize::try_from(max_jsonl_line_kib.saturating_mul(1024)).unwrap_or(usize::MAX);
    let usage_scan_limits = usage::ScanLimits {
        max_session_file_bytes,
        max_session_total_bytes,
        max_session_files_scanned: max_session_files,
        max_jsonl_line_bytes,
        scan_time_budget_ms,
        full_scan,
        scan_cache_max_entries,
    };
    let system_locale = locale::SystemLocale::detect();

    if args.dump_usage {
        let (harness, harness_home) = match args.harness {
            HarnessArg::Codex => (harness::Harness::Codex, codex_home.clone()),
            HarnessArg::Claude => (
                harness::Harness::Claude,
                providers::claude::resolve_claude_dir(args.claude_dir.clone())
                    .context("Unable to resolve the Claude Code config directory")?,
            ),
        };
        let snapshot = usage::compute_snapshot(
            harness,
            usage_days,
            &harness_home,
            project.as_deref(),
            usage_scan_limits,
            Some(&llmon_home.join(usage::SCAN_CACHE_DB_FILE_NAME)),
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&usage::snapshot_dump_json(&snapshot))?
        );
        return Ok(());
    }

    let config = app::Config {
        claude_dir: providers::claude::resolve_claude_dir(args.claude_dir.clone()),
        claude_limits_mode: match args.claude_limits.unwrap_or(user_config.claude_limits) {
            ClaudeLimitsArg::Statusline => app::ClaudeLimitsMode::StatusLine,
            ClaudeLimitsArg::Oauth => app::ClaudeLimitsMode::OAuth,
            ClaudeLimitsArg::Off => app::ClaudeLimitsMode::Off,
        },
        codex_bin: args.codex_bin.clone(),
        app_server_bin: args.app_server_bin.clone(),
        live_limits_mode: args.live_limits.into(),
        llmon_home,
        codex_home,
        read_sessions_dir: read_config.sessions_dir,
        start_in_read_screen: args.read_mode,
        cwd,
        workspace_path: project,
        usage_days,
        refresh_usage_secs,
        refresh_limits_secs,
        usage_scan_limits,
        rebuild_cache_on_start: args.rebuild_cache_on_start,
        system_locale,
        history_project_roots: user_config.history_project_roots,
        history_deep_depth: user_config.history_deep_depth.clamp(
            1,
            user_config
                .history_deep_max_depth
                .clamp(1, read::catalog::MAX_DEEP_DEPTH),
        ),
        history_deep_max_depth: user_config
            .history_deep_max_depth
            .clamp(1, read::catalog::MAX_DEEP_DEPTH),
        history_catalog_max_candidates: user_config.history_catalog_max_candidates.max(1),
        history_catalog_scan_budget_ms: user_config.history_catalog_scan_budget_ms.max(25),
        pricing: pricing::Pricing::new(&user_config.pricing),
    };

    app::run(config).await
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let dir = test_temp_base_without_git_parent().join(format!("llmon-main-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn test_temp_base_without_git_parent() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn resolve_workspace_path_uses_all_workspaces_outside_repo() {
        let root = make_temp_dir("non-repo");
        let workspace = resolve_workspace_filter(None);
        assert!(workspace.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_filter_uses_all_workspaces_without_project_override() {
        let root = make_temp_dir("launch-path");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace dir");

        let filtered = resolve_workspace_filter(None);
        assert!(filtered.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_filter_uses_project_path_as_filter() {
        let root = make_temp_dir("project-override-path");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        let expected = std::fs::canonicalize(&nested).unwrap_or_else(|_| nested.clone());

        let filtered = resolve_workspace_filter(Some(nested.as_path()));
        assert_eq!(filtered, Some(expected));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_filter_accepts_non_git_directory() {
        let root = make_temp_dir("project-override-non-git");
        let plain = root.join("plain-dir");
        std::fs::create_dir_all(&plain).expect("create plain dir");
        let expected = std::fs::canonicalize(&plain).unwrap_or_else(|_| plain.clone());

        let filtered = resolve_workspace_filter(Some(plain.as_path()));
        assert_eq!(filtered, Some(expected));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn user_config_schema_one_migrates_with_discovery_disabled() {
        let llmon_home = make_temp_dir("config-migration");
        let path = llmon_home.join(USER_CONFIG_FILE_NAME);
        let legacy = serde_json::json!({
            "schema_version": 1,
            "usage_days": 45,
            "refresh_usage_secs": 600
        });
        crate::storage::write_private_file(
            &path,
            &serde_json::to_vec_pretty(&legacy).expect("encode legacy config"),
        )
        .expect("write legacy config");

        let migrated = load_or_bootstrap_user_config(&llmon_home).expect("migrate config");
        assert_eq!(migrated.schema_version, USER_CONFIG_SCHEMA_VERSION);
        assert_eq!(migrated.usage_days, 45);
        assert_eq!(migrated.refresh_usage_secs, 600);
        assert_eq!(
            migrated.history_deep_depth,
            read::catalog::DEFAULT_DEEP_DEPTH
        );
        assert!(migrated.history_project_roots.is_empty());

        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read migrated config"))
                .expect("parse migrated config");
        assert_eq!(persisted["schema_version"], USER_CONFIG_SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(llmon_home);
    }

    #[test]
    fn user_config_schema_two_preserves_explicit_discovery_roots() {
        let llmon_home = make_temp_dir("config-discovery-root-migration");
        let path = llmon_home.join(USER_CONFIG_FILE_NAME);
        let legacy = serde_json::json!({
            "schema_version": 2,
            "history_project_roots": ["/Volumes/Ext/src"]
        });
        crate::storage::write_private_file(
            &path,
            &serde_json::to_vec_pretty(&legacy).expect("encode legacy config"),
        )
        .expect("write legacy config");

        let migrated = load_or_bootstrap_user_config(&llmon_home).expect("migrate config");
        assert_eq!(migrated.schema_version, USER_CONFIG_SCHEMA_VERSION);
        assert_eq!(
            migrated.history_project_roots,
            vec![PathBuf::from("/Volumes/Ext/src")]
        );

        let _ = std::fs::remove_dir_all(llmon_home);
    }
}
