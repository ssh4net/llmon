//! Distributed mode: llmon instances on different machines exchange their
//! usage, projects, and sessions through a shared folder (an SMB share,
//! Syncthing, a NAS, ...). It is opt-in: nothing here runs unless a sync
//! folder is configured (`sync.dir` in `config.json`, or `--sync-dir`).
//!
//! Every instance writes one file, `llmon-sync-<machine-id>.json`, and reads
//! the files of the others. There is no server and no listening port: the
//! folder is the only channel, and an instance that is off leaves its last
//! file behind.
//!
//! With a passphrase (`LLMON_SYNC_PASSPHRASE`, or the key file `llmon sync
//! passphrase` writes) the files are encrypted and named by the machine id
//! alone (see `crypto`); without one they stay plain JSON. A PC reads only
//! the kind it writes, so a plain file cannot stand in for an encrypted one.
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

mod crypto;

use crate::harness::Harness;
use crate::read::scan::{session_start_fields, Catalog, RemoteSession, SessionSummary};
use crate::usage::{ExportGroup, RemoteUsage, UsageExport};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Read as _;
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
/// Limits on what is accepted from another machine's file.
const MAX_SYNC_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PEER_GROUPS: usize = 50_000;
const MAX_PEER_SESSIONS: usize = 200_000;
const MAX_MODELS_PER_DAY: usize = 64;

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
    /// Set when a passphrase is: the files are then encrypted.
    pub(crate) key: Option<crypto::SyncKey>,
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
        let key = crypto::resolve_key(llmon_home)?;
        Ok(Some(Self {
            dir,
            machine,
            share_titles: config.share_titles,
            ignore: config.ignore.clone(),
            key,
        }))
    }

    /// This machine's file in the sync folder.
    pub(crate) fn own_file(&self) -> PathBuf {
        self.dir.join(self.own_file_name())
    }

    fn own_file_name(&self) -> String {
        match self.key {
            Some(_) => encrypted_file_name(&self.machine.id),
            None => sync_file_name(&self.machine.id),
        }
    }

    /// The file this machine wrote in the other form (plain before a
    /// passphrase was set, encrypted after it was removed), which must not
    /// stay behind: a plain copy would keep the data readable.
    fn other_own_file(&self) -> PathBuf {
        self.dir.join(self.other_own_file_name())
    }

    fn other_own_file_name(&self) -> String {
        match self.key {
            Some(_) => sync_file_name(&self.machine.id),
            None => encrypted_file_name(&self.machine.id),
        }
    }
}

pub(crate) fn sync_file_name(machine_id: &str) -> String {
    format!("{SYNC_FILE_PREFIX}{machine_id}{SYNC_FILE_SUFFIX}")
}

/// An encrypted file is named by the machine id alone: random, so it says
/// nothing, and it is the same name after the key changes, so the file is
/// replaced rather than left behind.
pub(crate) fn encrypted_file_name(machine_id: &str) -> String {
    machine_id.to_ascii_lowercase()
}

/// Whether a file name could be an encrypted sync file.
fn is_encrypted_file_name(name: &str) -> bool {
    is_valid_machine_id(name)
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

/// Offset of this machine's local calendar from UTC, in minutes.
pub(crate) fn local_utc_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
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
            utc_offset_minutes: local_utc_offset_minutes(),
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
    let json = serde_json::to_vec(file).context("Unable to encode the sync file")?;
    let bytes = match &settings.key {
        Some(key) => key.seal(&json)?,
        None => json,
    };
    crate::storage::write_shared_file_atomic(&settings.own_file(), &bytes)?;
    remove_other_own_file(settings)?;
    Ok(bytes.len())
}

/// Removes this machine's file of the other form, once the new one is in
/// place, so no readable copy of the data stays in the folder.
fn remove_other_own_file(settings: &SyncSettings) -> Result<()> {
    let path = settings.other_own_file();
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&path).with_context(|| {
                format!(
                    "The new sync file was written, but the old one ({}) could not be removed",
                    path.display()
                )
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Unable to inspect {}", path.display())),
    }
}

impl SyncSettings {
    /// Whether a machine's file is read: not ours (a copy of our own file
    /// under another name), and not on the ignore list (by id or name).
    fn accepts(&self, file: &SyncFile) -> bool {
        file.machine.id != self.machine.id
            && !self.ignore.iter().any(|entry| {
                let entry = entry.trim();
                entry == file.machine.id || entry.eq_ignore_ascii_case(&file.machine.name)
            })
    }
}

/// The other machines' files, as read from the sync folder.
#[derive(Debug, Clone, Default)]
pub(crate) struct PeerScan {
    /// Sorted by machine name.
    pub(crate) peers: Vec<Arc<SyncFile>>,
    /// Files that were skipped, each with the reason.
    pub(crate) problems: Vec<String>,
}

impl PeerScan {
    /// Whether nothing changed since `other`: the same files (the cache hands
    /// out the same `Arc` for an unchanged file) and the same problems.
    pub(crate) fn unchanged_from(&self, other: &PeerScan) -> bool {
        self.problems == other.problems
            && self.peers.len() == other.peers.len()
            && self
                .peers
                .iter()
                .zip(&other.peers)
                .all(|(left, right)| Arc::ptr_eq(left, right))
    }
}

/// Parsed peer files by path, so a file is read again only when its size or
/// modification time changed.
#[derive(Default)]
pub(crate) struct PeerCache {
    entries: HashMap<PathBuf, CachedPeer>,
}

struct CachedPeer {
    len: u64,
    modified_ms: i64,
    result: std::result::Result<Arc<SyncFile>, String>,
}

/// Reads the other machines' files from the sync folder. An unreadable
/// folder is an error; a bad file is skipped and reported in `problems`.
pub(crate) fn read_peers(settings: &SyncSettings, cache: &mut PeerCache) -> Result<PeerScan> {
    let own_name = settings.own_file_name();
    let entries = std::fs::read_dir(&settings.dir)
        .with_context(|| format!("Sync folder {} is not available", settings.dir.display()))?;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut scan = PeerScan::default();
    // Files of the other form: not read (a plain file must not stand in for
    // an encrypted one), but counted so the footer can say why.
    let mut other_form = 0_usize;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_plain_name = name.starts_with(SYNC_FILE_PREFIX) && name.ends_with(SYNC_FILE_SUFFIX);
        let wanted = if settings.key.is_some() {
            is_encrypted_file_name(&name)
        } else {
            is_plain_name
        };
        if !wanted {
            let ours = name == settings.other_own_file_name();
            let other = if settings.key.is_some() {
                is_plain_name
            } else {
                // Only the length a machine id has, so that a stray hex name
                // is not taken for an encrypted file.
                name.len() == 32 && is_encrypted_file_name(&name)
            };
            if other && !ours {
                other_form += 1;
            }
            continue;
        }
        if name == own_name {
            continue;
        }
        let path = entry.path();
        seen.insert(path.clone());
        // A symlink or special file is refused, like everywhere else.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            scan.problems
                .push(format!("{name}: not a regular file, skipped"));
            continue;
        }
        let len = metadata.len();
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(crate::read::scan::system_time_to_epoch_ms)
            .unwrap_or(0);
        let result = match cache.entries.get(&path) {
            Some(cached) if cached.len == len && cached.modified_ms == modified_ms => {
                cached.result.clone()
            }
            _ => {
                let result = load_peer_file(settings, &path, &name, len);
                cache.entries.insert(
                    path,
                    CachedPeer {
                        len,
                        modified_ms,
                        result: result.clone(),
                    },
                );
                result
            }
        };
        match result {
            Ok(file) if settings.accepts(&file) => scan.peers.push(file),
            Ok(_) => {}
            Err(problem) => scan
                .problems
                .push(format!("{}: {problem}", display_file_name(&name))),
        }
    }
    cache.entries.retain(|path, _| seen.contains(path));
    // Two files with one machine id (a copy): the newer one wins.
    scan.peers
        .sort_by(|left, right| right.generated_at.cmp(&left.generated_at));
    let mut ids = HashSet::new();
    scan.peers
        .retain(|peer| ids.insert(peer.machine.id.clone()));
    scan.peers.sort_by(|left, right| {
        left.machine
            .name
            .to_lowercase()
            .cmp(&right.machine.name.to_lowercase())
            .then_with(|| left.machine.id.cmp(&right.machine.id))
    });
    scan.problems.sort();
    if other_form > 0 {
        let note = if settings.key.is_some() {
            format!("{other_form} unencrypted file(s) ignored: set the passphrase on those PCs")
        } else {
            format!("{other_form} encrypted file(s) ignored: set the passphrase on this PC")
        };
        scan.problems.push(note);
    }
    Ok(scan)
}

/// An encrypted file's name is a long random id: shortened for the footer.
fn display_file_name(name: &str) -> String {
    if name.len() > 16 && is_encrypted_file_name(name) {
        format!("{}..", &name[..8])
    } else {
        name.to_string()
    }
}

fn load_peer_file(
    settings: &SyncSettings,
    path: &Path,
    name: &str,
    len: u64,
) -> std::result::Result<Arc<SyncFile>, String> {
    let limit_mib = MAX_SYNC_FILE_BYTES / (1024 * 1024);
    if len > MAX_SYNC_FILE_BYTES {
        return Err(format!("larger than {limit_mib} MiB, skipped"));
    }
    // Read at most one byte past the limit, in case the file grew since the
    // size check.
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_SYNC_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("unreadable ({error})"))?;
    if bytes.len() as u64 > MAX_SYNC_FILE_BYTES {
        return Err(format!("larger than {limit_mib} MiB, skipped"));
    }
    let json = match &settings.key {
        Some(key) => key.open(&bytes).map_err(|error| error.to_string())?,
        None => bytes,
    };
    let mut file: SyncFile = serde_json::from_slice(&json)
        .map_err(|error| format!("not a valid sync file ({error})"))?;
    if file.format != SYNC_FORMAT {
        return Err("not an llmon sync file".to_string());
    }
    if file.version > SYNC_VERSION {
        return Err(format!(
            "format version {} is newer than this llmon understands ({SYNC_VERSION}); update llmon",
            file.version
        ));
    }
    if !is_valid_machine_id(&file.machine.id) {
        return Err("invalid machine id".to_string());
    }
    let expected_name = match settings.key {
        Some(_) => encrypted_file_name(&file.machine.id),
        None => sync_file_name(&file.machine.id),
    };
    if !name.eq_ignore_ascii_case(&expected_name) {
        return Err("file name does not match its machine id".to_string());
    }
    sanitize_file(&mut file);
    Ok(Arc::new(file))
}

/// Text from another machine, as safe to draw: no control characters (a
/// terminal escape sequence must not reach the screen), at most `max`
/// characters.
pub(crate) fn clean_text(text: &str, max: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn clean_option(text: Option<String>, max: usize) -> Option<String> {
    text.map(|text| clean_text(&text, max))
        .filter(|text| !text.is_empty())
}

/// `L:YYYY-MM-DD` or `U:YYYY-MM-DD`.
fn is_valid_day_key(key: &str) -> bool {
    key.len() == 12
        && (key.starts_with("L:") || key.starts_with("U:"))
        && chrono::NaiveDate::parse_from_str(&key[2..], "%Y-%m-%d").is_ok()
}

fn clamp_tokens(tokens: &mut crate::usage::TokenBreakdown) {
    tokens.input = tokens.input.max(0);
    tokens.cache_write = tokens.cache_write.max(0);
    tokens.cache_write_1h = tokens.cache_write_1h.max(0);
    tokens.cache_read = tokens.cache_read.max(0);
    tokens.output = tokens.output.max(0);
}

/// Cleans everything in a peer's file that is shown or summed.
fn sanitize_file(file: &mut SyncFile) {
    let machine = &mut file.machine;
    machine.name = clean_text(&machine.name, 64);
    if machine.name.is_empty() {
        machine.name = "unnamed".to_string();
    }
    machine.os = clean_text(&machine.os, 32);
    machine.llmon = clean_text(&machine.llmon, 32);
    machine.utc_offset_minutes = machine.utc_offset_minutes.clamp(-14 * 60, 14 * 60);
    file.generated_at = file.generated_at.max(0);
    file.usage.retain(|key, _| Harness::from_key(key).is_some());
    for groups in file.usage.values_mut() {
        groups.truncate(MAX_PEER_GROUPS);
        for group in groups.iter_mut() {
            sanitize_group(group);
        }
    }
    file.sessions
        .retain(|key, _| Harness::from_key(key).is_some());
    for sessions in file.sessions.values_mut() {
        sessions.truncate(MAX_PEER_SESSIONS);
        for session in sessions.iter_mut() {
            sanitize_session(session);
        }
        sessions.retain(|session| !session.id.is_empty());
    }
}

fn sanitize_group(group: &mut ExportGroup) {
    group.cwd = clean_option(group.cwd.take(), 1024);
    group.daily.retain(|key, _| is_valid_day_key(key));
    for totals in group.daily.values_mut() {
        clamp_tokens(&mut totals.tokens);
        totals.agent_ms = totals.agent_ms.max(0);
        totals.agent_runs = totals.agent_runs.max(0);
    }
    group.models.retain(|key, _| is_valid_day_key(key));
    for models in group.models.values_mut() {
        let cleaned: BTreeMap<String, crate::usage::TokenBreakdown> = std::mem::take(models)
            .into_iter()
            .filter_map(|(name, mut tokens)| {
                let name = clean_text(&name, 128);
                (!name.is_empty()).then(|| {
                    clamp_tokens(&mut tokens);
                    (name, tokens)
                })
            })
            .take(MAX_MODELS_PER_DAY)
            .collect();
        *models = cleaned;
    }
}

fn sanitize_session(session: &mut ExportSession) {
    session.id = clean_text(&session.id, 128);
    session.cwd = clean_text(&session.cwd, 1024);
    session.started_at = clean_option(session.started_at.take(), 40);
    session.started_ms = session.started_ms.max(0);
    session.model = clean_option(session.model.take(), 128);
    session.provider = clean_option(session.provider.take(), 64);
    session.client_version = clean_option(session.client_version.take(), 64);
    session.branch = clean_option(session.branch.take(), 256);
    session.commit = clean_option(session.commit.take(), 64);
    session.repo = clean_option(session.repo.take(), 512);
    session.title = clean_option(session.title.take(), 256);
    session.subagents = session.subagents.min(10_000);
    session.tokens = session.tokens.map(|tokens| tokens.max(0));
}

/// The other machines' sessions as session summaries for HISTORY. The log is
/// not here, so `file_path` is only a unique key. A session that two machines
/// report belongs to the one with the newer file.
pub(crate) fn remote_sessions(peers: &[Arc<SyncFile>]) -> Vec<SessionSummary> {
    let mut ordered: Vec<&Arc<SyncFile>> = peers.iter().collect();
    ordered.sort_by(|left, right| right.generated_at.cmp(&left.generated_at));
    let mut seen: HashSet<(Harness, String)> = HashSet::new();
    let mut sessions = Vec::new();
    for peer in ordered {
        for (key, exported) in &peer.sessions {
            let Some(harness) = Harness::from_key(key) else {
                continue;
            };
            for session in exported {
                if seen.insert((harness, session.id.clone())) {
                    sessions.push(remote_summary(peer, harness, session));
                }
            }
        }
    }
    sessions
}

fn remote_summary(peer: &SyncFile, harness: Harness, session: &ExportSession) -> SessionSummary {
    let file_path = PathBuf::from(format!(
        "sync://{}/{}/{}",
        peer.machine.id,
        harness.key(),
        session.id
    ));
    let (started_at_raw, started_at_label, started_at_sort_key_ms) =
        session_start_fields(session.started_at.clone(), session.started_ms, &file_path);
    // Without a recorded start the exported sort key is all there is.
    let started_at_sort_key_ms = if started_at_raw.is_some() {
        started_at_sort_key_ms
    } else {
        session.started_ms
    };
    // A title is shared only on request; otherwise name the session by what
    // is not prompt text.
    let title = session.title.clone().unwrap_or_else(|| {
        let short: String = session.id.chars().take(8).collect();
        let mut parts = vec![format!("Session {short}")];
        parts.extend(session.branch.clone());
        parts.extend(
            session
                .model
                .as_deref()
                .map(crate::usage::model_display_name),
        );
        parts.join(" | ")
    });
    SessionSummary {
        harness,
        file_path,
        session_id: session.id.clone(),
        cwd: session.cwd.clone(),
        title,
        started_at_raw,
        started_at_label,
        started_at_sort_key_ms,
        git_branch: session.branch.clone(),
        git_commit: session.commit.clone(),
        repo_url: session.repo.clone(),
        model_provider: session.provider.clone(),
        client_version: session.client_version.clone(),
        model: session.model.clone(),
        subagent_files: Vec::new(),
        remote: Some(RemoteSession {
            machine_id: peer.machine.id.clone(),
            machine_name: peer.machine.name.clone(),
            os: peer.machine.os.clone(),
            tokens: session.tokens,
            subagents: session.subagents,
        }),
    }
}

/// The usage of the other machines for one harness, ready to be added to a
/// snapshot. A machine whose local calendar is in another time zone than
/// ours (`local_utc_offset_minutes`) is counted by its UTC days.
pub(crate) fn remote_usage(
    peers: &[Arc<SyncFile>],
    harness: Harness,
    local_utc_offset_minutes: i32,
) -> Vec<RemoteUsage> {
    peers
        .iter()
        .flat_map(|peer| {
            let same_calendar = peer.machine.utc_offset_minutes == local_utc_offset_minutes;
            peer.usage
                .get(harness.key())
                .into_iter()
                .flatten()
                .map(move |group| RemoteUsage::from_export(group, same_calendar))
        })
        .collect()
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
            remote: None,
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
            key: None,
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

    fn machine_with(id: &str, name: &str, utc_offset_minutes: i32) -> MachineInfo {
        MachineInfo {
            id: id.to_string(),
            name: name.to_string(),
            utc_offset_minutes,
            ..machine()
        }
    }

    fn settings_for(dir: &Path, machine: MachineInfo, ignore: &[&str]) -> SyncSettings {
        SyncSettings {
            dir: dir.to_path_buf(),
            machine,
            share_titles: false,
            ignore: ignore.iter().map(|entry| entry.to_string()).collect(),
            key: None,
        }
    }

    /// Publishes a file for `machine` with one Codex session.
    fn publish_for(dir: &Path, machine: &MachineInfo, session_id: &str) -> SyncFile {
        let settings = settings_for(dir, machine.clone(), &[]);
        let file = build_sync_file(
            machine,
            &usage_export(),
            &catalog_of(vec![session(Harness::Codex, session_id, "/work/app", 5)]),
            true,
            100,
        );
        publish(&settings, &file).expect("publish peer file");
        file
    }

    const ID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ID_C: &str = "cccccccccccccccccccccccccccccccc";

    #[test]
    fn peers_are_read_except_ourselves_and_ignored_machines() {
        let share = make_temp_dir("peers");
        let ours = machine_with(ID_A, "pc-a", 540);
        publish_for(&share, &ours, "a1");
        publish_for(&share, &machine_with(ID_B, "pc-b", 540), "b1");
        publish_for(&share, &machine_with(ID_C, "Laptop", 540), "c1");
        // Files that are not sync files are left alone.
        std::fs::write(share.join("notes.txt"), "hello").expect("write notes");
        std::fs::write(share.join(".llmon-sync-xyz.json.tmp-1"), "partial").expect("write temp");

        let mut cache = PeerCache::default();
        let all = read_peers(&settings_for(&share, ours.clone(), &[]), &mut cache).expect("scan");
        let names: Vec<&str> = all.peers.iter().map(|p| p.machine.name.as_str()).collect();
        assert_eq!(names, vec!["Laptop", "pc-b"], "sorted, without ourselves");
        assert!(all.problems.is_empty(), "{:?}", all.problems);
        assert_eq!(all.peers[1].sessions["codex"][0].id, "b1");
        assert_eq!(
            all.peers[1].sessions["codex"][0].title.as_deref(),
            Some("secret prompt of b1"),
            "a peer that shares titles"
        );

        // Ignored by name (any case) or by id.
        let by_name =
            read_peers(&settings_for(&share, ours.clone(), &["LAPTOP"]), &mut cache).expect("scan");
        assert_eq!(by_name.peers.len(), 1);
        assert_eq!(by_name.peers[0].machine.name, "pc-b");
        let by_id =
            read_peers(&settings_for(&share, ours, &[ID_B, " laptop "]), &mut cache).expect("scan");
        assert!(by_id.peers.is_empty());

        // A folder that is gone is an error, not an empty list.
        let _ = std::fs::remove_dir_all(&share);
        let gone = read_peers(
            &settings_for(&share, machine_with(ID_A, "pc-a", 540), &[]),
            &mut cache,
        );
        assert!(gone.is_err());
    }

    fn encrypted_settings(dir: &Path, machine: MachineInfo, passphrase: &str) -> SyncSettings {
        SyncSettings {
            key: Some(crypto::SyncKey::for_tests(passphrase)),
            ..settings_for(dir, machine, &[])
        }
    }

    /// Publishes a file for `settings`' machine with one Codex session.
    fn publish_with(settings: &SyncSettings, session_id: &str) -> SyncFile {
        let file = build_sync_file(
            &settings.machine,
            &usage_export(),
            &catalog_of(vec![session(Harness::Codex, session_id, "/work/app", 5)]),
            true,
            100,
        );
        publish(settings, &file).expect("publish");
        file
    }

    fn folder_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("list")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_encrypted_file_is_named_by_the_machine_id_and_shows_nothing() {
        let share = make_temp_dir("encrypted-publish");
        let settings = encrypted_settings(&share, machine_with(ID_A, "Visible Name", 540), "pw");
        let file = publish_with(&settings, "a1");

        assert_eq!(folder_names(&share), vec![ID_A.to_string()]);
        let bytes = std::fs::read(settings.own_file()).expect("read back");
        let plain = serde_json::to_vec(&file).expect("encode");
        assert_eq!(bytes.len(), plain.len() + crypto::OVERHEAD);
        for text in ["llmon", "Visible Name", "/work/app", "gpt-test", "{\""] {
            assert!(
                !bytes
                    .windows(text.len())
                    .any(|window| window == text.as_bytes()),
                "{text} is readable in the file"
            );
        }
        let opened = settings
            .key
            .as_ref()
            .expect("key")
            .open(&bytes)
            .expect("open");
        assert_eq!(
            serde_json::from_slice::<SyncFile>(&opened).expect("parse"),
            file
        );
        let _ = std::fs::remove_dir_all(share);
    }

    #[test]
    fn machines_with_one_passphrase_read_each_other_and_others_are_rejected() {
        let share = make_temp_dir("encrypted-peers");
        let ours = encrypted_settings(&share, machine_with(ID_A, "pc-a", 540), "shared");
        publish_with(&ours, "a1");
        publish_with(
            &encrypted_settings(&share, machine_with(ID_B, "pc-b", 540), "shared"),
            "b1",
        );
        publish_with(
            &encrypted_settings(&share, machine_with(ID_C, "pc-c", 540), "other"),
            "c1",
        );

        let scan = read_peers(&ours, &mut PeerCache::default()).expect("scan");
        let names: Vec<&str> = scan.peers.iter().map(|p| p.machine.name.as_str()).collect();
        assert_eq!(names, vec!["pc-b"], "without ourselves and the other group");
        assert_eq!(scan.peers[0].sessions["codex"][0].id, "b1");
        assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
        assert_eq!(
            scan.problems[0],
            "cccccccc..: cannot decrypt (wrong passphrase, or not an llmon file)"
        );
        let _ = std::fs::remove_dir_all(share);
    }

    #[test]
    fn a_changed_or_renamed_encrypted_file_is_not_trusted() {
        let share = make_temp_dir("encrypted-tamper");
        let ours = encrypted_settings(&share, machine_with(ID_A, "pc-a", 0), "shared");
        let theirs = encrypted_settings(&share, machine_with(ID_B, "pc-b", 0), "shared");
        publish_with(&theirs, "b1");

        let mut bytes = std::fs::read(theirs.own_file()).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(theirs.own_file(), &bytes).expect("tamper");
        let scan = read_peers(&ours, &mut PeerCache::default()).expect("scan");
        assert!(scan.peers.is_empty());
        assert!(
            scan.problems[0].contains("cannot decrypt"),
            "{:?}",
            scan.problems
        );

        // A valid file under another machine's name: it opens, but it is not
        // what the name says, so it must not replace that machine.
        publish_with(&theirs, "b1");
        std::fs::copy(theirs.own_file(), share.join(ID_C)).expect("copy");
        let scan = read_peers(&ours, &mut PeerCache::default()).expect("scan");
        assert_eq!(scan.peers.len(), 1, "only the real file of pc-b");
        assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
        assert!(
            scan.problems[0].ends_with("file name does not match its machine id"),
            "{:?}",
            scan.problems
        );
        let _ = std::fs::remove_dir_all(share);
    }

    #[test]
    fn a_pc_reads_only_the_form_it_writes_and_says_what_it_skipped() {
        let share = make_temp_dir("encrypted-forms");
        let plain_a = settings_for(&share, machine_with(ID_A, "pc-a", 0), &[]);
        let plain_b = settings_for(&share, machine_with(ID_B, "pc-b", 0), &[]);
        let secret_c = encrypted_settings(&share, machine_with(ID_C, "pc-c", 0), "shared");
        publish_with(&plain_b, "b1");
        publish_with(&secret_c, "c1");
        // Not a sync file: too short to be a machine id, and not 32 characters.
        std::fs::write(share.join("deadbeef"), "x").expect("write");
        std::fs::write(share.join("notes.txt"), "x").expect("write");

        let plain_scan = read_peers(&plain_a, &mut PeerCache::default()).expect("scan");
        let names: Vec<&str> = plain_scan
            .peers
            .iter()
            .map(|p| p.machine.name.as_str())
            .collect();
        assert_eq!(names, vec!["pc-b"]);
        assert_eq!(
            plain_scan.problems,
            vec!["1 encrypted file(s) ignored: set the passphrase on this PC".to_string()]
        );

        let secret_a = encrypted_settings(&share, machine_with(ID_A, "pc-a", 0), "shared");
        let secret_scan = read_peers(&secret_a, &mut PeerCache::default()).expect("scan");
        let names: Vec<&str> = secret_scan
            .peers
            .iter()
            .map(|p| p.machine.name.as_str())
            .collect();
        assert_eq!(names, vec!["pc-c"], "the plain file of pc-b is not read");
        assert_eq!(secret_scan.problems.len(), 2, "{:?}", secret_scan.problems);
        assert!(
            secret_scan.problems[0].starts_with("deadbeef: "),
            "a short hex name is tried, and a file's own problem comes first: {:?}",
            secret_scan.problems
        );
        assert_eq!(
            secret_scan.problems[1],
            "1 unencrypted file(s) ignored: set the passphrase on those PCs"
        );
        let _ = std::fs::remove_dir_all(share);
    }

    #[test]
    fn changing_the_form_removes_this_pcs_old_file_and_only_that() {
        let share = make_temp_dir("encrypted-switch");
        let plain = settings_for(&share, machine_with(ID_A, "pc-a", 0), &[]);
        let secret = encrypted_settings(&share, machine_with(ID_A, "pc-a", 0), "shared");
        let other = settings_for(&share, machine_with(ID_B, "pc-b", 0), &[]);
        publish_with(&other, "b1");

        publish_with(&plain, "a1");
        assert!(plain.own_file().exists());
        publish_with(&secret, "a1");
        assert_eq!(
            folder_names(&share),
            vec![ID_A.to_string(), sync_file_name(ID_B)],
            "the readable copy is gone; the other PC's file is not ours to touch"
        );

        publish_with(&plain, "a1");
        assert_eq!(
            folder_names(&share),
            vec![sync_file_name(ID_A), sync_file_name(ID_B)]
        );
        let _ = std::fs::remove_dir_all(share);
    }

    #[test]
    fn the_key_file_turns_encryption_on_for_a_run() {
        let home = make_temp_dir("encrypted-resolve");
        let share = home.join("share");
        std::fs::create_dir_all(&share).expect("create share");
        let config = SyncConfig {
            dir: Some(share.clone()),
            ..SyncConfig::default()
        };

        let plain = SyncSettings::resolve(&config, None, &home)
            .expect("resolve")
            .expect("enabled");
        assert_eq!(plain.key, None);
        assert!(plain.own_file().to_string_lossy().ends_with(".json"));

        let key = crypto::SyncKey::for_tests("stored");
        std::fs::write(crypto::key_file_path(&home), key.key_file_text()).expect("write key");
        let secret = SyncSettings::resolve(&config, None, &home)
            .expect("resolve")
            .expect("enabled");
        assert_eq!(secret.key, Some(key));
        assert_eq!(secret.own_file(), share.join(&secret.machine.id));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn bad_peer_files_are_skipped_with_a_reason() {
        let share = make_temp_dir("bad-peers");
        let name = |id: &str| share.join(sync_file_name(id));
        let valid = serde_json::to_value(build_sync_file(
            &machine_with(ID_B, "pc-b", 0),
            &usage_export(),
            &catalog_of(Vec::new()),
            false,
            1,
        ))
        .expect("encode");
        let with = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut value = valid.clone();
            change(&mut value);
            serde_json::to_vec(&value).expect("encode")
        };
        std::fs::write(name(ID_B), "{ this is not json").expect("write");
        std::fs::write(
            name(ID_C),
            with(&|value| value["format"] = serde_json::json!("something-else")),
        )
        .expect("write");
        let newer = "dddddddddddddddddddddddddddddddd";
        std::fs::write(
            name(newer),
            with(&|value| {
                value["version"] = serde_json::json!(99);
                value["machine"]["id"] = serde_json::json!(newer);
            }),
        )
        .expect("write");
        let mismatch = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        std::fs::write(name(mismatch), with(&|_| {})).expect("write");
        let bad_id = "not-hex-at-all-but-long-enough";
        std::fs::write(
            name(bad_id),
            with(&|value| value["machine"]["id"] = serde_json::json!(bad_id)),
        )
        .expect("write");
        let huge = "ffffffffffffffffffffffffffffffff";
        std::fs::File::create(name(huge))
            .and_then(|file| file.set_len(MAX_SYNC_FILE_BYTES + 1))
            .expect("sparse file");
        #[cfg(unix)]
        {
            let target = share.join("target.json");
            std::fs::write(&target, "{}").expect("write target");
            let linked = "99999999999999999999999999999999";
            std::os::unix::fs::symlink(&target, name(linked)).expect("symlink");
        }

        let ours = machine_with(ID_A, "pc-a", 0);
        let scan =
            read_peers(&settings_for(&share, ours, &[]), &mut PeerCache::default()).expect("scan");
        assert!(scan.peers.is_empty(), "{:?}", scan.peers);
        let problems = scan.problems.join("\n");
        assert!(problems.contains("not a valid sync file"), "{problems}");
        assert!(problems.contains("not an llmon sync file"), "{problems}");
        assert!(problems.contains("is newer than this llmon"), "{problems}");
        assert!(
            problems.contains("does not match its machine id"),
            "{problems}"
        );
        assert!(problems.contains("invalid machine id"), "{problems}");
        assert!(problems.contains("larger than 64 MiB"), "{problems}");
        #[cfg(unix)]
        {
            assert!(problems.contains("not a regular file"), "{problems}");
            assert_eq!(scan.problems.len(), 7, "{problems}");
        }
        let _ = std::fs::remove_dir_all(share);
    }

    #[test]
    fn hostile_values_are_cleaned_before_they_reach_the_screen() {
        let share = make_temp_dir("hostile");
        let mut peer = build_sync_file(
            &machine_with(ID_B, "pc\u{1b}[31m-b\nnext", 99_999),
            &usage_export(),
            &catalog_of(vec![
                session(Harness::Codex, "ok", "/work/app", 5),
                session(Harness::Codex, "", "/work/app", 6),
            ]),
            true,
            -5,
        );
        let ok = peer
            .sessions
            .get_mut("codex")
            .expect("sessions")
            .iter_mut()
            .find(|session| session.id == "ok")
            .expect("session ok");
        ok.title = Some("line one\r\nline\u{7} two".to_string());
        ok.tokens = Some(-9);
        let group = &mut peer.usage.get_mut("codex").expect("usage")[0];
        group.cwd = Some("/work/\u{1b}]0;title\u{7}app".to_string());
        group
            .daily
            .insert("L:not-a-day".to_string(), Default::default());
        group.daily.insert(
            "U:2026-09-02".to_string(),
            crate::usage::DailyTotals {
                tokens: TokenBreakdown {
                    input: -5,
                    output: 7,
                    ..Default::default()
                },
                agent_ms: -1,
                agent_runs: 2,
            },
        );
        group.models.insert("junk".to_string(), BTreeMap::new());
        group
            .models
            .get_mut("U:2026-09-01")
            .expect("day")
            .insert("bad\u{1b}model".to_string(), TokenBreakdown::default());
        peer.usage.insert("unknown-harness".to_string(), Vec::new());
        publish(&settings_for(&share, peer.machine.clone(), &[]), &peer).expect("publish");

        let scan = read_peers(
            &settings_for(&share, machine_with(ID_A, "pc-a", 0), &[]),
            &mut PeerCache::default(),
        )
        .expect("scan");
        let file = &scan.peers[0];
        assert_eq!(file.machine.name, "pc[31m-bnext");
        assert_eq!(file.machine.utc_offset_minutes, 14 * 60);
        assert_eq!(file.generated_at, 0);
        assert!(!file.usage.contains_key("unknown-harness"));
        let group = &file.usage["codex"][0];
        assert_eq!(group.cwd.as_deref(), Some("/work/]0;titleapp"));
        assert!(!group.daily.contains_key("L:not-a-day"));
        let day = group.daily["U:2026-09-02"];
        assert_eq!((day.tokens.input, day.tokens.output), (0, 7));
        assert_eq!((day.agent_ms, day.agent_runs), (0, 2));
        assert!(!group.models.contains_key("junk"));
        assert!(group.models["U:2026-09-01"].contains_key("badmodel"));
        let sessions = &file.sessions["codex"];
        assert_eq!(sessions.len(), 1, "a session without an id is dropped");
        assert_eq!(sessions[0].tokens, Some(0));
        assert_eq!(sessions[0].title.as_deref(), Some("line oneline two"));
    }

    #[test]
    fn unchanged_peer_files_are_not_read_again() {
        let share = make_temp_dir("cache");
        let peer = machine_with(ID_B, "pc-b", 0);
        publish_for(&share, &peer, "b1");
        let settings = settings_for(&share, machine_with(ID_A, "pc-a", 0), &[]);
        let mut cache = PeerCache::default();

        let first = read_peers(&settings, &mut cache).expect("scan");
        let second = read_peers(&settings, &mut cache).expect("scan");
        assert!(second.unchanged_from(&first));
        assert!(Arc::ptr_eq(&first.peers[0], &second.peers[0]));

        // A rewritten file (here with another session id, so another size).
        publish_for(&share, &peer, "a-longer-session-id");
        let third = read_peers(&settings, &mut cache).expect("scan");
        assert!(!third.unchanged_from(&second));
        assert_eq!(
            third.peers[0].sessions["codex"][0].id,
            "a-longer-session-id"
        );

        // A deleted file leaves the cache and the list.
        std::fs::remove_file(share.join(sync_file_name(ID_B))).expect("delete");
        let fourth = read_peers(&settings, &mut cache).expect("scan");
        assert!(fourth.peers.is_empty());
        assert!(cache.entries.is_empty());
        let _ = std::fs::remove_dir_all(share);
    }

    fn exported_session(id: &str, started_ms: i64) -> ExportSession {
        ExportSession {
            id: id.to_string(),
            cwd: "/peer/app".to_string(),
            started_at: Some("2026-09-01T10:00:00Z".to_string()),
            started_ms,
            model: Some("claude-opus-5-5".to_string()),
            provider: None,
            client_version: Some("2.1".to_string()),
            branch: Some("feature".to_string()),
            commit: None,
            repo: None,
            title: None,
            subagents: 2,
            tokens: Some(1_234),
        }
    }

    fn peer_with_sessions(
        id: &str,
        name: &str,
        generated_at: i64,
        sessions: Vec<ExportSession>,
    ) -> Arc<SyncFile> {
        let mut file = build_sync_file(
            &machine_with(id, name, 0),
            &BTreeMap::new(),
            &catalog_of(Vec::new()),
            false,
            generated_at,
        );
        file.sessions.insert("claude".to_string(), sessions);
        Arc::new(file)
    }

    #[test]
    fn remote_sessions_become_session_summaries_without_a_log() {
        let older = peer_with_sessions(
            ID_B,
            "pc-b",
            10,
            vec![
                exported_session("s1-aaaaaaaa", 100),
                exported_session("only-b-xxxx", 50),
            ],
        );
        let mut titled = exported_session("s1-aaaaaaaa", 200);
        titled.title = Some("shared title".to_string());
        let newer = peer_with_sessions(ID_C, "pc-c", 20, vec![titled]);

        let sessions = remote_sessions(&[older, newer]);
        assert_eq!(
            sessions.len(),
            2,
            "a session two machines report is listed once"
        );
        let shared = sessions
            .iter()
            .find(|session| session.session_id == "s1-aaaaaaaa")
            .expect("shared session");
        let remote = shared.remote.as_ref().expect("remote");
        assert_eq!(
            remote.machine_name, "pc-c",
            "the machine with the newer file"
        );
        assert_eq!(shared.title, "shared title");
        assert_eq!(
            shared.file_path,
            PathBuf::from(format!("sync://{ID_C}/claude/s1-aaaaaaaa"))
        );
        assert_eq!(shared.harness, Harness::Claude);
        assert_eq!(shared.cwd, "/peer/app");
        assert_eq!(shared.started_at_sort_key_ms, 200);
        assert!(shared.subagent_files.is_empty());
        assert_eq!((remote.tokens, remote.subagents), (Some(1_234), 2));

        // Without a shared title the session is named by what is not prompt text.
        let untitled = sessions
            .iter()
            .find(|session| session.session_id == "only-b-xxxx")
            .expect("untitled");
        assert_eq!(untitled.title, "Session only-b-x | feature | Opus 5.5");
        assert!(sessions.iter().all(|session| session.remote.is_some()));
    }

    #[test]
    fn remote_usage_follows_the_peer_calendar() {
        let share = make_temp_dir("calendar");
        let mut peer = build_sync_file(
            &machine_with(ID_B, "pc-b", 480),
            &usage_export(),
            &catalog_of(Vec::new()),
            false,
            1,
        );
        let group = &mut peer.usage.get_mut("codex").expect("usage")[0];
        group
            .daily
            .insert("L:2026-09-01".to_string(), Default::default());
        let peers = vec![Arc::new(peer)];
        let _ = std::fs::remove_dir_all(share);

        // Same time zone: both calendars are used as they are.
        let same = remote_usage(&peers, Harness::Codex, 480);
        assert_eq!(same.len(), 1);
        assert!(same[0].daily_keys().contains(&"L:2026-09-01".to_string()));
        // Another time zone: only UTC days, which stand in for local ones.
        let other = remote_usage(&peers, Harness::Codex, 540);
        let keys = other[0].daily_keys();
        assert!(keys.contains(&"U:2026-09-01".to_string()), "{keys:?}");
        assert!(keys.contains(&"L:2026-09-01".to_string()), "{keys:?}");
        assert_eq!(
            other[0].model_keys(),
            vec!["L:2026-09-01".to_string(), "U:2026-09-01".to_string()]
        );
        // A harness the peer has no usage for.
        assert!(remote_usage(&peers, Harness::Claude, 480).is_empty());
    }
}
