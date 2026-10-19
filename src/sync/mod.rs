//! Distributed mode: llmon instances on different machines exchange their
//! usage, projects, and sessions through a shared folder (an SMB share,
//! Syncthing, a NAS, ...). It is opt-in: nothing here runs unless a sync
//! folder is configured (`sync.dir` in `config.json`, or `--sync-dir`).
//!
//! Every instance writes one file, `llmon-sync-<machine-id>.json`, and reads
//! the files of the others. There is no server, no listening port, and no
//! credential: the folder is the only channel, and an instance that is off
//! leaves its last file behind.
//!
//! What a file holds:
//! - the machine (id, name, OS, time zone offset, llmon version);
//! - usage per harness and project: the per-day and per-model-per-day
//!   tokens, runs, and agent time the scan cache keeps, summed per project,
//!   with no log file paths;
//! - sessions per harness: id, project path, start time, model, git branch,
//!   repository URL (without credentials), subagent count, and token total.
//!   Titles are the first line of the prompt, so they are left out unless
//!   `sync.share_titles` is on.

use crate::harness::Harness;
use crate::read::scan::Catalog;
use crate::usage::{ExportGroup, UsageExport};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const SYNC_FORMAT: &str = "llmon-sync";
pub(crate) const SYNC_VERSION: u32 = 1;
pub(crate) const SYNC_FILE_PREFIX: &str = "llmon-sync-";
pub(crate) const SYNC_FILE_SUFFIX: &str = ".json";
/// File of this machine's id, in the llmon home.
const MACHINE_ID_FILE_NAME: &str = "machine-id";
/// Sessions exported per harness, newest first.
const MAX_EXPORT_SESSIONS: usize = 20_000;

/// The `sync` section of `config.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SyncConfig {
    /// The shared folder. Empty or missing means distributed mode is off.
    pub(crate) dir: Option<PathBuf>,
    /// How other machines see this one (default: the host name).
    pub(crate) machine_name: Option<String>,
    /// Also share session titles (the first line of the prompt). Off by
    /// default, because prompts stay on the machine otherwise.
    pub(crate) share_titles: bool,
    /// Machines (by name or id) whose files are ignored, for example a second
    /// llmon on this PC that reads the same logs.
    pub(crate) ignore: Vec<String>,
}

/// The distributed mode settings of a run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SyncSettings {
    pub(crate) dir: PathBuf,
    pub(crate) machine: MachineInfo,
    pub(crate) share_titles: bool,
    pub(crate) ignore: Vec<String>,
}

impl SyncSettings {
    /// `None` unless a sync folder is set (config or `--sync-dir`).
    pub(crate) fn resolve(
        config: &SyncConfig,
        dir_override: Option<PathBuf>,
        llmon_home: &Path,
    ) -> Result<Option<Self>> {
        let dir = dir_override
            .or_else(|| config.dir.clone())
            .filter(|dir| !dir.as_os_str().is_empty());
        let Some(dir) = dir else {
            return Ok(None);
        };
        let machine = MachineInfo::local(llmon_home, config.machine_name.as_deref())?;
        Ok(Some(Self {
            dir,
            machine,
            share_titles: config.share_titles,
            ignore: config.ignore.clone(),
        }))
    }

    /// This machine's file in the sync folder.
    pub(crate) fn own_file(&self) -> PathBuf {
        self.dir.join(sync_file_name(&self.machine.id))
    }
}

pub(crate) fn sync_file_name(machine_id: &str) -> String {
    format!("{SYNC_FILE_PREFIX}{machine_id}{SYNC_FILE_SUFFIX}")
}

/// One machine running llmon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MachineInfo {
    /// Random, kept in `<llmon home>/machine-id`.
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) os: String,
    pub(crate) llmon: String,
    /// Offset of the local calendar from UTC, in minutes: days of the local
    /// calendar only line up between machines that share it.
    pub(crate) utc_offset_minutes: i32,
}

impl MachineInfo {
    pub(crate) fn local(llmon_home: &Path, name_override: Option<&str>) -> Result<Self> {
        let name = name_override
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(default_machine_name);
        Ok(Self {
            id: load_or_create_machine_id(llmon_home)?,
            name,
            os: os_label(),
            llmon: env!("CARGO_PKG_VERSION").to_string(),
            utc_offset_minutes: chrono::Local::now().offset().local_minus_utc() / 60,
        })
    }
}

fn load_or_create_machine_id(llmon_home: &Path) -> Result<String> {
    let path = llmon_home.join(MACHINE_ID_FILE_NAME);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim();
        if is_valid_machine_id(id) {
            return Ok(id.to_string());
        }
    }
    let id = random_machine_id();
    crate::storage::write_private_file(&path, format!("{id}\n").as_bytes())
        .with_context(|| format!("Unable to save the machine id in {}", path.display()))?;
    Ok(id)
}

pub(crate) fn is_valid_machine_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 128 random bits as hex. `RandomState` is seeded from the OS; the clock and
/// process id are mixed in so two machines never share an id.
fn random_machine_id() -> String {
    use std::hash::BuildHasher;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut id = String::with_capacity(32);
    for round in 0..2_u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_u64(round);
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

fn default_machine_name() -> String {
    let host = hostname().unwrap_or_else(|| "unknown".to_string());
    if is_wsl() {
        format!("{host} (WSL)")
    } else {
        host
    }
}

#[cfg(unix)]
fn hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: the buffer is valid for `buffer.len()` bytes, and gethostname
    // writes at most that many.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec())
        .ok()
        .filter(|name| !name.is_empty())
}

#[cfg(not(unix))]
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
}

fn is_wsl() -> bool {
    cfg!(target_os = "linux")
        && std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

fn os_label() -> String {
    match std::env::consts::OS {
        "linux" if is_wsl() => "Linux (WSL)".to_string(),
        "linux" => "Linux".to_string(),
        "windows" => "Windows".to_string(),
        "macos" => "macOS".to_string(),
        other => other.to_string(),
    }
}

/// The file one instance publishes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SyncFile {
    pub(crate) format: String,
    pub(crate) version: u32,
    pub(crate) machine: MachineInfo,
    /// Unix seconds when the file was written.
    pub(crate) generated_at: i64,
    /// Harness key -> usage per project.
    #[serde(default)]
    pub(crate) usage: BTreeMap<String, Vec<ExportGroup>>,
    /// Harness key -> sessions, newest first.
    #[serde(default)]
    pub(crate) sessions: BTreeMap<String, Vec<ExportSession>>,
}

/// One session, without its log path and (unless shared) its title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExportSession {
    pub(crate) id: String,
    /// The project path on the machine that has the session.
    pub(crate) cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at: Option<String>,
    #[serde(default)]
    pub(crate) started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
    /// Repository URL with any user name or password removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repo: Option<String>,
    /// Only with `sync.share_titles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) subagents: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tokens: Option<i64>,
}

/// Builds this machine's file from its usage exports and its session catalog.
pub(crate) fn build_sync_file(
    machine: &MachineInfo,
    usage: &BTreeMap<Harness, Arc<UsageExport>>,
    catalog: &Catalog,
    share_titles: bool,
    now: i64,
) -> SyncFile {
    let mut file = SyncFile {
        format: SYNC_FORMAT.to_string(),
        version: SYNC_VERSION,
        machine: machine.clone(),
        generated_at: now,
        usage: BTreeMap::new(),
        sessions: BTreeMap::new(),
    };
    for (harness, export) in usage {
        file.usage
            .insert(harness.key().to_string(), export.groups.clone());
        let sessions = export_sessions(catalog, *harness, &export.file_tokens, share_titles);
        file.sessions.insert(harness.key().to_string(), sessions);
    }
    file
}

fn export_sessions(
    catalog: &Catalog,
    harness: Harness,
    file_tokens: &BTreeMap<String, i64>,
    share_titles: bool,
) -> Vec<ExportSession> {
    let tokens_of = |path: &Path| file_tokens.get(path.to_string_lossy().as_ref()).copied();
    let mut sessions: Vec<ExportSession> = catalog
        .projects
        .iter()
        .flat_map(|project| project.sessions.iter())
        .filter(|session| session.harness == harness)
        .map(|session| {
            // The session's tokens include its subagents' transcripts.
            let parts: Vec<i64> = std::iter::once(session.file_path.as_path())
                .chain(session.subagent_files.iter().map(PathBuf::as_path))
                .filter_map(tokens_of)
                .collect();
            ExportSession {
                id: session.session_id.clone(),
                cwd: session.cwd.clone(),
                started_at: session.started_at_raw.clone(),
                started_ms: session.started_at_sort_key_ms,
                model: session.model.clone(),
                provider: session.model_provider.clone(),
                client_version: session.client_version.clone(),
                branch: session.git_branch.clone(),
                commit: session.git_commit.clone(),
                repo: session.repo_url.as_deref().map(strip_url_credentials),
                title: share_titles.then(|| session.title.clone()),
                subagents: session.subagent_files.len(),
                tokens: (!parts.is_empty()).then(|| parts.iter().sum()),
            }
        })
        .collect();
    sessions.sort_by(|left, right| {
        right
            .started_ms
            .cmp(&left.started_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions.truncate(MAX_EXPORT_SESSIONS);
    sessions
}

/// A repository URL without a user name, password, query, or fragment:
/// `https://user:token@host/org/repo.git?x` -> `https://host/org/repo.git`.
/// Other forms (`git@host:org/repo.git`) carry no secret and stay as they are.
pub(crate) fn strip_url_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path = path.split(['?', '#']).next().unwrap_or("");
    let host = host.split(['?', '#']).next().unwrap_or("");
    format!("{scheme}://{host}{path}")
}

/// A fingerprint of what the file says about usage and sessions, not of when
/// it was written: unchanged content is not written again.
pub(crate) fn content_fingerprint(file: &SyncFile) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(bytes) = serde_json::to_vec(&(&file.usage, &file.sessions)) {
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

/// Writes this machine's file into the sync folder, which must already
/// exist (a missing folder usually means an unmounted share, and creating it
/// would put the files on the wrong disk). Returns the size written.
pub(crate) fn publish(settings: &SyncSettings, file: &SyncFile) -> Result<usize> {
    let metadata = std::fs::metadata(&settings.dir)
        .with_context(|| format!("Sync folder {} is not available", settings.dir.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("Sync folder {} is not a directory", settings.dir.display());
    }
    let bytes = serde_json::to_vec(file).context("Unable to encode the sync file")?;
    crate::storage::write_shared_file_atomic(&settings.own_file(), &bytes)?;
    Ok(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::scan::{ProjectRecord, SessionSummary};
    use crate::usage::TokenBreakdown;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("llmon-sync-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn session(harness: Harness, id: &str, cwd: &str, started_ms: i64) -> SessionSummary {
        SessionSummary {
            harness,
            file_path: PathBuf::from(format!("/logs/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            title: format!("secret prompt of {id}"),
            started_at_raw: Some("2026-09-01T10:00:00Z".to_string()),
            started_at_label: "2026-09-01 10:00".to_string(),
            started_at_sort_key_ms: started_ms,
            git_branch: Some("main".to_string()),
            git_commit: None,
            repo_url: Some("https://user:token@example.com/org/repo.git".to_string()),
            model_provider: Some("openai".to_string()),
            client_version: None,
            model: Some("gpt-test".to_string()),
            subagent_files: Vec::new(),
        }
    }

    fn catalog_of(sessions: Vec<SessionSummary>) -> Catalog {
        Catalog {
            sessions_dir: PathBuf::from("/logs"),
            projects: vec![ProjectRecord {
                stable_id: "path:/work/app".to_string(),
                logical_name: "app".to_string(),
                display_path: "/work/app".to_string(),
                checkouts: vec!["/work/app".to_string()],
                owner_session_count: sessions.len(),
                sessions,
                confidence: 100,
                source_flags: 0,
                missing: false,
            }],
            files_scanned: 0,
            files_skipped: 0,
        }
    }

    fn machine() -> MachineInfo {
        MachineInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            name: "pc-a".to_string(),
            os: "Linux".to_string(),
            llmon: "0.5.3".to_string(),
            utc_offset_minutes: 540,
        }
    }

    fn usage_export() -> BTreeMap<Harness, Arc<UsageExport>> {
        let mut file_tokens = BTreeMap::new();
        file_tokens.insert("/logs/a.jsonl".to_string(), 100);
        file_tokens.insert("/logs/sub.jsonl".to_string(), 25);
        let mut daily = BTreeMap::new();
        daily.insert("U:2026-09-01".to_string(), Default::default());
        let mut models = BTreeMap::new();
        models.insert(
            "U:2026-09-01".to_string(),
            BTreeMap::from([(
                "gpt-test".to_string(),
                TokenBreakdown {
                    input: 1,
                    ..Default::default()
                },
            )]),
        );
        BTreeMap::from([(
            Harness::Codex,
            Arc::new(UsageExport {
                groups: vec![ExportGroup {
                    cwd: Some("/work/app".to_string()),
                    daily,
                    models,
                }],
                file_tokens,
            }),
        )])
    }

    #[test]
    fn urls_lose_credentials_queries_and_fragments_only() {
        assert_eq!(
            strip_url_credentials("https://user:token@example.com/org/repo.git"),
            "https://example.com/org/repo.git"
        );
        assert_eq!(
            strip_url_credentials("https://example.com/org/repo.git?a=b#c"),
            "https://example.com/org/repo.git"
        );
        assert_eq!(
            strip_url_credentials("ssh://git@example.com:22/org/repo.git"),
            "ssh://example.com:22/org/repo.git"
        );
        assert_eq!(
            strip_url_credentials("git@example.com:org/repo.git"),
            "git@example.com:org/repo.git"
        );
        assert_eq!(strip_url_credentials("/local/path"), "/local/path");
    }

    #[test]
    fn sessions_are_exported_without_titles_unless_shared() {
        let mut with_agents = session(Harness::Codex, "a", "/work/app", 2_000);
        with_agents.subagent_files = vec![PathBuf::from("/logs/sub.jsonl")];
        let catalog = catalog_of(vec![
            session(Harness::Codex, "old", "/work/app", 1_000),
            with_agents,
            session(Harness::Claude, "c", "/work/app", 3_000),
        ]);
        let usage = usage_export();

        let private = build_sync_file(&machine(), &usage, &catalog, false, 42);
        assert_eq!(private.format, SYNC_FORMAT);
        assert_eq!(private.generated_at, 42);
        let sessions = &private.sessions["codex"];
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "old"],
            "newest first, and only this harness"
        );
        assert!(sessions.iter().all(|session| session.title.is_none()));
        assert_eq!(sessions[0].tokens, Some(125), "own log plus the subagent");
        assert_eq!(sessions[0].subagents, 1);
        assert_eq!(sessions[1].tokens, None, "no cached tokens for this log");
        assert_eq!(
            sessions[0].repo.as_deref(),
            Some("https://example.com/org/repo.git")
        );
        assert!(!private.sessions.contains_key("claude"), "no Claude usage");
        let json = serde_json::to_string(&private).expect("encode");
        assert!(!json.contains("secret prompt"), "{json}");
        assert!(!json.contains("token@"), "{json}");
        assert!(!json.contains("/logs/"), "no log paths: {json}");

        let shared = build_sync_file(&machine(), &usage, &catalog, true, 42);
        assert_eq!(
            shared.sessions["codex"][0].title.as_deref(),
            Some("secret prompt of a")
        );
    }

    #[test]
    fn fingerprint_ignores_the_write_time_only() {
        let catalog = catalog_of(vec![session(Harness::Codex, "a", "/work/app", 1)]);
        let usage = usage_export();
        let first = build_sync_file(&machine(), &usage, &catalog, false, 1);
        let later = build_sync_file(&machine(), &usage, &catalog, false, 999);
        assert_eq!(content_fingerprint(&first), content_fingerprint(&later));
        let other = build_sync_file(
            &machine(),
            &usage,
            &catalog_of(vec![session(Harness::Codex, "b", "/work/app", 1)]),
            false,
            1,
        );
        assert_ne!(content_fingerprint(&first), content_fingerprint(&other));
    }

    #[test]
    fn machine_id_is_created_once_and_reused() {
        let home = make_temp_dir("machine-id");
        let first = MachineInfo::local(&home, Some("  my-pc  ")).expect("machine");
        assert!(is_valid_machine_id(&first.id), "{}", first.id);
        assert_eq!(first.name, "my-pc");
        let second = MachineInfo::local(&home, None).expect("machine");
        assert_eq!(second.id, first.id);
        assert!(!second.name.is_empty());
        assert_eq!(second.llmon, env!("CARGO_PKG_VERSION"));

        // A damaged id file is replaced.
        std::fs::write(home.join(MACHINE_ID_FILE_NAME), "not an id\n").expect("damage");
        let repaired = MachineInfo::local(&home, None).expect("machine");
        assert!(is_valid_machine_id(&repaired.id));
        assert_ne!(repaired.id, first.id);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn settings_are_off_without_a_sync_folder() {
        let home = make_temp_dir("settings");
        let config = SyncConfig::default();
        assert_eq!(
            SyncSettings::resolve(&config, None, &home).expect("resolve"),
            None
        );
        let empty = SyncConfig {
            dir: Some(PathBuf::new()),
            ..SyncConfig::default()
        };
        assert_eq!(
            SyncSettings::resolve(&empty, None, &home).expect("resolve"),
            None
        );
        assert!(
            !home.join(MACHINE_ID_FILE_NAME).exists(),
            "an opt-out run writes nothing"
        );

        let flag = SyncSettings::resolve(&config, Some(home.join("share")), &home)
            .expect("resolve")
            .expect("enabled by the flag");
        assert_eq!(flag.dir, home.join("share"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn publish_writes_one_file_and_needs_an_existing_folder() {
        let home = make_temp_dir("publish");
        let share = home.join("share");
        let settings = SyncSettings {
            dir: share.clone(),
            machine: machine(),
            share_titles: false,
            ignore: Vec::new(),
        };
        let file = build_sync_file(
            &settings.machine,
            &usage_export(),
            &catalog_of(vec![session(Harness::Codex, "a", "/work/app", 1)]),
            false,
            7,
        );

        let missing = publish(&settings, &file).expect_err("folder is missing");
        assert!(
            format!("{missing:#}").contains("not available"),
            "{missing:#}"
        );
        assert!(!share.exists(), "a missing folder is not created");

        std::fs::create_dir_all(&share).expect("create share");
        let written = publish(&settings, &file).expect("publish");
        let path = settings.own_file();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("llmon-sync-0123456789abcdef0123456789abcdef.json")
        );
        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(bytes.len(), written);
        let parsed: SyncFile = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed, file);
        let leftovers: Vec<_> = std::fs::read_dir(&share)
            .expect("list")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // Publishing again replaces the file.
        publish(&settings, &file).expect("republish");
        let _ = std::fs::remove_dir_all(home);
    }
}
