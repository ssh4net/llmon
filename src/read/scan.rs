use crate::harness::Harness;
use crate::providers::{claude, codex};
use crate::usage::{normalize_project_key, TokenBreakdown};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const MAX_TITLE_CHARS: usize = 96;
pub(crate) const MAX_TURN_PREVIEW_CHARS: usize = 220;
pub(crate) const UNRESOLVED_SESSION_OWNER: &str = "<unresolved session owner>";

#[derive(Debug, Clone)]
pub(crate) struct Catalog {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) projects: Vec<ProjectRecord>,
    pub(crate) files_scanned: usize,
    pub(crate) files_skipped: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectRecord {
    pub(crate) stable_id: String,
    pub(crate) logical_name: String,
    pub(crate) display_path: String,
    pub(crate) checkouts: Vec<String>,
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) owner_session_count: usize,
    pub(crate) confidence: u8,
    pub(crate) source_flags: u32,
    pub(crate) missing: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSummary {
    pub(crate) harness: Harness,
    pub(crate) file_path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) title: String,
    pub(crate) started_at_raw: Option<String>,
    pub(crate) started_at_label: String,
    pub(crate) started_at_sort_key_ms: i64,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_commit: Option<String>,
    pub(crate) repo_url: Option<String>,
    pub(crate) model_provider: Option<String>,
    /// Version of the CLI that wrote the log (Claude Code).
    pub(crate) client_version: Option<String>,
    pub(crate) model: Option<String>,
    /// Subagent transcripts of this session (Claude Code).
    pub(crate) subagent_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionDetail {
    pub(crate) meaningful_user_turns: Vec<String>,
    pub(crate) all_user_turns: Vec<String>,
    pub(crate) assistant_messages: usize,
    pub(crate) tool_calls: usize,
    pub(crate) tool_outputs: usize,
    pub(crate) input_images: usize,
    /// Encrypted reasoning items were present (Codex).
    pub(crate) reasoning_encrypted: bool,
    /// Thinking content blocks (Claude Code).
    pub(crate) thinking_blocks: usize,
    /// Token usage of the session.
    pub(crate) usage: Option<TokenBreakdown>,
    /// Total the log itself reports, when it does (Codex). It can differ from
    /// `usage.total()`, so the browser shows it as the session total.
    pub(crate) total_tokens: Option<i64>,
}

#[derive(Debug, Default)]
struct ProjectBuilder {
    sessions: Vec<SessionSummary>,
}

pub(crate) fn build_catalog(harness: Harness, sessions_dir: &Path) -> Result<Catalog> {
    if !sessions_dir.exists() {
        return Ok(Catalog {
            sessions_dir: sessions_dir.to_path_buf(),
            projects: Vec::new(),
            files_scanned: 0,
            files_skipped: 0,
        });
    }

    let mut candidates = Vec::new();
    collect_session_files(sessions_dir, &mut candidates)?;
    candidates.sort();

    // Claude Code subagent transcripts are attached to their parent session
    // instead of being listed on their own.
    let (top_level, mut subagents) = match harness {
        Harness::Codex => (candidates, BTreeMap::new()),
        Harness::Claude => claude::history::split_subagent_files(candidates),
    };

    let mut grouped: BTreeMap<String, ProjectBuilder> = BTreeMap::new();
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;

    for path in top_level {
        match scan_session_summary(harness, &path) {
            Ok(mut summary) => {
                files_scanned += 1;
                let file_session_id = path.file_stem().and_then(|stem| stem.to_str());
                for key in [Some(summary.session_id.as_str()), file_session_id]
                    .into_iter()
                    .flatten()
                {
                    if let Some(files) = subagents.remove(key) {
                        summary.subagent_files.extend(files);
                    }
                }
                let key = normalize_project_key(&summary.cwd);
                grouped.entry(key).or_default().sessions.push(summary);
            }
            Err(_) => {
                files_skipped += 1;
            }
        }
    }

    // Subagents whose parent transcript is gone are still listed.
    for path in subagents.into_values().flatten() {
        match scan_session_summary(harness, &path) {
            Ok(summary) => {
                files_scanned += 1;
                let key = normalize_project_key(&summary.cwd);
                grouped.entry(key).or_default().sessions.push(summary);
            }
            Err(_) => {
                files_skipped += 1;
            }
        }
    }

    let projects = finish_project_builders(grouped);

    Ok(Catalog {
        sessions_dir: sessions_dir.to_path_buf(),
        projects,
        files_scanned,
        files_skipped,
    })
}

/// The catalogs of several harnesses as one: every session is grouped by its
/// own cwd, as `build_catalog` does, so a project used from both harnesses
/// is one project. `sessions_dir` is the first catalog's.
pub(crate) fn merge_catalogs(catalogs: Vec<Catalog>) -> Catalog {
    let mut sessions_dir = None;
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;
    let mut grouped: BTreeMap<String, ProjectBuilder> = BTreeMap::new();
    for catalog in catalogs {
        sessions_dir.get_or_insert(catalog.sessions_dir);
        files_scanned += catalog.files_scanned;
        files_skipped += catalog.files_skipped;
        for session in catalog
            .projects
            .into_iter()
            .flat_map(|project| project.sessions)
        {
            grouped
                .entry(normalize_project_key(&session.cwd))
                .or_default()
                .sessions
                .push(session);
        }
    }
    Catalog {
        sessions_dir: sessions_dir.unwrap_or_default(),
        projects: finish_project_builders(grouped),
        files_scanned,
        files_skipped,
    }
}

/// The sessions of one harness, grouped by project like `build_catalog`.
pub(crate) fn filter_catalog(catalog: &Catalog, harness: Harness) -> Catalog {
    let mut grouped: BTreeMap<String, ProjectBuilder> = BTreeMap::new();
    for session in catalog
        .projects
        .iter()
        .flat_map(|project| project.sessions.iter())
        .filter(|session| session.harness == harness)
    {
        grouped
            .entry(normalize_project_key(&session.cwd))
            .or_default()
            .sessions
            .push(session.clone());
    }
    Catalog {
        sessions_dir: catalog.sessions_dir.clone(),
        projects: finish_project_builders(grouped),
        files_scanned: catalog.files_scanned,
        files_skipped: catalog.files_skipped,
    }
}

fn scan_session_summary(harness: Harness, path: &Path) -> Result<SessionSummary> {
    match harness {
        Harness::Codex => codex::history::scan_session_summary(path),
        Harness::Claude => claude::history::scan_session_summary(path),
    }
}

pub(crate) fn load_session_detail(harness: Harness, path: &Path) -> Result<SessionDetail> {
    match harness {
        Harness::Codex => codex::history::load_session_detail(path),
        Harness::Claude => claude::history::load_session_detail(path),
    }
}

/// Start time fields of a session summary: the recorded timestamp, or the
/// file modification time (with a `--` label) when the log has none.
pub(crate) fn session_start_fields(
    started_at_raw: Option<String>,
    started_at_sort_key_ms: i64,
    file_path: &Path,
) -> (Option<String>, String, i64) {
    if let Some(raw) = started_at_raw {
        let label = format_timestamp_label(&raw);
        return (Some(raw), label, started_at_sort_key_ms);
    }
    let file_time = std::fs::metadata(file_path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(system_time_to_epoch_ms)
        .unwrap_or(0);
    (None, "--".to_string(), file_time)
}

fn finish_project_builders(grouped: BTreeMap<String, ProjectBuilder>) -> Vec<ProjectRecord> {
    let mut projects = Vec::with_capacity(grouped.len());
    for (_, mut builder) in grouped {
        builder.sessions.sort_by(|left, right| {
            right
                .started_at_sort_key_ms
                .cmp(&left.started_at_sort_key_ms)
                .then_with(|| left.file_path.cmp(&right.file_path))
        });
        let display_path = builder
            .sessions
            .first()
            .map(|session| session.cwd.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        let stable_id = format!("path:{}", normalize_project_key(&display_path));
        let logical_name = Path::new(&display_path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&display_path)
            .to_string();
        let owner_session_count = builder.sessions.len();
        projects.push(ProjectRecord {
            stable_id,
            logical_name,
            checkouts: vec![display_path.clone()],
            display_path,
            sessions: builder.sessions,
            owner_session_count,
            confidence: 100,
            source_flags: crate::read::catalog::SOURCE_OWNER,
            missing: false,
        });
    }

    projects.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    projects
}

fn collect_session_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Unable to read directory {}", dir.display()))?;
        for entry_result in entries {
            let entry = entry_result
                .with_context(|| format!("Unable to read entry in {}", dir.display()))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("Unable to inspect {}", path.display()))?;
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_rfc3339_to_epoch_ms(raw: &str) -> Option<i64> {
    let parsed = DateTime::parse_from_rfc3339(raw).ok()?;
    Some(parsed.timestamp_millis())
}

pub(crate) fn format_timestamp_label(raw: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(raw).ok();
    if let Some(parsed) = parsed {
        let utc: DateTime<Utc> = parsed.with_timezone(&Utc);
        let local = utc.with_timezone(&Local);
        return local.format("%Y-%m-%d %H:%M").to_string();
    }
    raw.to_string()
}

pub(crate) fn system_time_to_epoch_ms(time: SystemTime) -> Option<i64> {
    let duration = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

/// The catalog with every session's metadata and detail counts as JSON, for
/// `--dump-history`. Like `--dump-usage`, the field set is a regression
/// contract. Turn texts are reduced to counts; titles are kept because title
/// selection is part of what the check covers.
pub(crate) fn catalog_dump_json(catalog: &Catalog) -> Value {
    let projects: Vec<Value> = catalog
        .projects
        .iter()
        .map(|project| {
            let sessions: Vec<Value> = project
                .sessions
                .iter()
                .map(|session| {
                    let detail = load_session_detail(session.harness, &session.file_path)
                        .ok()
                        .map(|detail| {
                            serde_json::json!({
                                "meaningful_user_turns": detail.meaningful_user_turns.len(),
                                "all_user_turns": detail.all_user_turns.len(),
                                "assistant_messages": detail.assistant_messages,
                                "tool_calls": detail.tool_calls,
                                "tool_outputs": detail.tool_outputs,
                                "input_images": detail.input_images,
                                "reasoning_encrypted": detail.reasoning_encrypted,
                                "thinking_blocks": detail.thinking_blocks,
                                "total_tokens": detail.total_tokens,
                                "usage": detail.usage.map(|usage| serde_json::json!({
                                    "input": usage.input,
                                    "cache_write": usage.cache_write,
                                    "cache_read": usage.cache_read,
                                    "output": usage.output,
                                    "total": usage.total(),
                                })),
                            })
                        });
                    serde_json::json!({
                        "harness": session.harness.key(),
                        "file": session.file_path.display().to_string(),
                        "session_id": session.session_id,
                        "cwd": session.cwd,
                        "title": session.title,
                        "started_at_raw": session.started_at_raw,
                        "started_at_label": session.started_at_label,
                        "started_at_sort_key_ms": session.started_at_sort_key_ms,
                        "git_branch": session.git_branch,
                        "git_commit": session.git_commit,
                        "repo_url": session.repo_url,
                        "model_provider": session.model_provider,
                        "client_version": session.client_version,
                        "model": session.model,
                        "subagents": session
                            .subagent_files
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>(),
                        "detail": detail,
                    })
                })
                .collect();
            serde_json::json!({
                "stable_id": project.stable_id,
                "logical_name": project.logical_name,
                "display_path": project.display_path,
                "checkouts": project.checkouts,
                "owner_session_count": project.owner_session_count,
                "sessions": sessions,
            })
        })
        .collect();
    serde_json::json!({
        "sessions_dir": catalog.sessions_dir.display().to_string(),
        "files_scanned": catalog.files_scanned,
        "files_skipped": catalog.files_skipped,
        "projects": projects,
    })
}

pub(crate) fn truncate_single_line(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output = String::new();
    for (idx, ch) in compact.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        output.push(ch);
    }
    if compact.chars().count() > max_chars {
        let suffix = if max_chars >= 3 {
            "..."
        } else if max_chars == 2 {
            ".."
        } else {
            "."
        };
        while output.len() + suffix.len() > max_chars && !output.is_empty() {
            output.pop();
        }
        output.push_str(suffix);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("llmon-read-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_session(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write session");
    }

    #[cfg(not(windows))]
    #[test]
    fn build_catalog_keeps_case_distinct_linux_paths_separate() {
        let root = make_temp_dir("group");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(sessions.join("2026/03/16")).expect("create tree");
        let session_a = sessions.join("2026/03/16/a.jsonl");
        let session_b = sessions.join("2026/03/16/b.jsonl");

        write_session(
            &session_a,
            r##"{"type":"session_meta","payload":{"id":"a","timestamp":"2026-03-16T08:30:22.974Z","cwd":"/mnt/e/Work/demo-builder","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /mnt/e/Work/demo-builder"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"show me the session history"}]}}
"##,
        );
        write_session(
            &session_b,
            r##"{"type":"session_meta","payload":{"id":"b","timestamp":"2026-03-16T09:30:22.974Z","cwd":"/mnt/e/work/demo-builder","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list prompts"}]}}
"##,
        );

        let catalog = build_catalog(Harness::Codex, &sessions).expect("catalog");
        assert_eq!(catalog.projects.len(), 2);
        assert_eq!(catalog.projects[0].display_path, "/mnt/e/Work/demo-builder");
        assert_eq!(catalog.projects[0].sessions.len(), 1);
        assert_eq!(
            catalog.projects[0].sessions[0].title,
            "show me the session history"
        );
        assert_eq!(catalog.projects[1].display_path, "/mnt/e/work/demo-builder");
        assert_eq!(catalog.projects[1].sessions[0].title, "list prompts");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_keeps_session_at_origin_when_tools_access_external_projects() {
        let root = make_temp_dir("tool-workdir-owner");
        let sessions = root.join("sessions");
        let external = root.join("nativefiledialog-extended");
        std::fs::create_dir_all(sessions.join("2026/06/23")).expect("create sessions");
        std::fs::create_dir_all(&external).expect("create external dir");
        let session = sessions.join("2026/06/23/a.jsonl");
        write_session(
            &session,
            &format!(
                "{}\n{}\n{}\n",
                r#"{"type":"session_meta","payload":{"id":"a","timestamp":"2026-06-23T08:30:22.974Z","cwd":"/outside/Lantern","model_provider":"openai"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"work on the project"}]}}"#,
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": serde_json::json!({
                            "workdir": external,
                            "cmd": format!("sed -n '1,20p' {}/CMakeLists.txt", external.display()),
                            "sandbox_permissions": "require_escalated"
                        }).to_string()
                    }
                })
            ),
        );

        let catalog = build_catalog(Harness::Codex, &sessions).expect("catalog");
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].display_path, "/outside/Lantern");
        assert_eq!(catalog.projects[0].sessions.len(), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_keeps_late_referenced_paths_under_session_origin() {
        let root = make_temp_dir("late-external-reference");
        let sessions = root.join("sessions/2026/07/26");
        std::fs::create_dir_all(&sessions).expect("create sessions");

        let session = sessions.join("session.jsonl");
        let mut body = String::from(
            r#"{"type":"session_meta","payload":{"id":"session","timestamp":"2026-07-26T08:00:00Z","cwd":"/outside/Lantern"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"work in Lantern"}]}}
"#,
        );
        for _ in 0..140 {
            body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"noop\"}}\n");
        }
        body.push_str(
            &serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "arguments": serde_json::json!({
                        "workdir": "/outside/nativefiledialog-extended",
                        "cmd": "sed -n '1,20p' /outside/nativefiledialog-extended/CMakeLists.txt"
                    }).to_string()
                }
            })
            .to_string(),
        );
        body.push('\n');
        write_session(&session, &body);

        let catalog = build_catalog(Harness::Codex, &root.join("sessions")).expect("catalog");
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].display_path, "/outside/Lantern");
        assert_eq!(catalog.projects[0].sessions[0].file_path, session);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_recovers_late_meta_without_rehoming_to_settings() {
        let root = make_temp_dir("late-owner-recovery");
        let sessions = root.join("sessions/2026/07/29");
        std::fs::create_dir_all(&sessions).expect("create sessions");
        let session = sessions.join("late.jsonl");
        let mut body = String::from(
            r#"{"type":"turn_context","payload":{"cwd":"/outside/fallback"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"recover this session"}]}}
"#,
        );
        for _ in 0..128 {
            body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"noop\"}}\n");
        }
        body.push_str(
            r#"{"type":"session_meta","payload":{"id":"late","timestamp":"2026-07-29T08:00:00Z","cwd":"/outside/Lantern"}}
{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"cwd":"/outside/nativefiledialog-extended"}}}
"#,
        );
        write_session(&session, &body);

        let catalog = build_catalog(Harness::Codex, &root.join("sessions")).expect("catalog");
        assert_eq!(catalog.files_scanned, 1);
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].display_path, "/outside/Lantern");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_keeps_sessions_with_no_owner_visible() {
        let root = make_temp_dir("unresolved-owner");
        let sessions = root.join("sessions/2026/07/29");
        std::fs::create_dir_all(&sessions).expect("create sessions");
        let session = sessions.join("unknown.jsonl");
        write_session(
            &session,
            r#"{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"cwd":"/outside/not-an-owner"}}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"orphaned session"}]}}
"#,
        );

        let catalog = build_catalog(Harness::Codex, &root.join("sessions")).expect("catalog");
        assert_eq!(catalog.files_scanned, 1);
        assert_eq!(catalog.files_skipped, 0);
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].display_path, UNRESOLVED_SESSION_OWNER);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_session_detail_extracts_turns_and_tokens() {
        let root = make_temp_dir("detail");
        let path = root.join("session.jsonl");
        write_session(
            &path,
            r##"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-03-16T08:30:22.974Z","cwd":"/mnt/d/projects/demo-cli","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /mnt/d/projects/demo-cli"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"show all prompts"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{}"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"1","output":"done"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12,"output_tokens":4,"total_tokens":16}}}}
"##,
        );

        let detail = load_session_detail(Harness::Codex, &path).expect("detail");
        assert_eq!(detail.meaningful_user_turns, vec!["show all prompts"]);
        assert_eq!(detail.all_user_turns.len(), 2);
        assert_eq!(detail.assistant_messages, 1);
        assert_eq!(detail.tool_calls, 1);
        assert_eq!(detail.tool_outputs, 1);
        assert_eq!(detail.total_tokens, Some(16));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn truncate_single_line_collapses_whitespace() {
        let text = "one\n\n two\tthree";
        assert_eq!(truncate_single_line(text, 64), "one two three");
    }
}
