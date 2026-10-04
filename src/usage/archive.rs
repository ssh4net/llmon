//! Durable usage of session logs that no longer exist.
//!
//! Harnesses delete or move old logs (Claude Code after `cleanupPeriodDays`,
//! Codex when a session is archived). When a cached log disappears, its
//! aggregates move from the scan cache into this file, so history outlives
//! the raw logs. Unlike `llmon.db`, this file is never rebuilt: cache schema
//! changes and `--rebuild-cache-on-start` leave it untouched.
//!
//! The JSON columns use the serde formats of `DailyTotals` and
//! `TokenBreakdown`. They are part of the archive layout: a field may be
//! added with `#[serde(default)]`, but renaming or removing one needs an
//! archive layout migration.

use super::{
    enforce_private_db_files, prepare_private_db_path, CachedFileScanEntry, DailyTotals,
    FileUsageRef, TokenBreakdown,
};
use crate::harness::Harness;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) const USAGE_ARCHIVE_DB_FILE_NAME: &str = "llmon-archive.db";
/// Layout of the archive tables (`archive_meta` key `layout_version`).
const ARCHIVE_LAYOUT_VERSION: i64 = 1;

/// Aggregates of one log file that no longer exists.
#[derive(Debug, Clone)]
pub(crate) struct ArchivedUsage {
    pub(crate) file_path: String,
    pub(crate) session_cwd: Option<String>,
    pub(crate) daily: HashMap<String, DailyTotals>,
    pub(crate) model_totals_by_day: HashMap<String, HashMap<String, TokenBreakdown>>,
}

impl ArchivedUsage {
    pub(crate) fn usage(&self) -> FileUsageRef<'_> {
        FileUsageRef {
            session_cwd: self.session_cwd.as_deref(),
            daily: &self.daily,
            model_totals_by_day: &self.model_totals_by_day,
        }
    }
}

pub(crate) struct UsageArchive {
    path: PathBuf,
    conn: Connection,
}

impl UsageArchive {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        prepare_private_db_path(path, "usage archive")?;
        let mut conn = Connection::open(path)
            .with_context(|| format!("Unable to open usage archive {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .with_context(|| format!("Unable to set WAL journal mode for {}", path.display()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .with_context(|| format!("Unable to set synchronous mode for {}", path.display()))?;
        init_archive_tables(&mut conn)
            .with_context(|| format!("Unable to initialize usage archive {}", path.display()))?;
        enforce_private_db_files(path, "usage archive")?;
        Ok(Self {
            path: path.to_path_buf(),
            conn,
        })
    }

    /// All archived rows of one harness. Rows that do not decode are skipped
    /// but never deleted.
    pub(crate) fn load(&self, harness: Harness) -> Result<Vec<ArchivedUsage>> {
        let mut stmt = self
            .conn
            .prepare(
                "
                SELECT file_path, session_cwd, daily_json, model_daily_json
                FROM archived_usage
                WHERE harness = ?1
                ORDER BY file_path;
                ",
            )
            .with_context(|| format!("Unable to query usage archive {}", self.path.display()))?;
        let mut rows = stmt
            .query(params![harness.key()])
            .with_context(|| format!("Unable to read usage archive {}", self.path.display()))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .with_context(|| format!("Unable to read usage archive {}", self.path.display()))?
        {
            let file_path: String = row.get(0)?;
            let session_cwd: Option<String> = row.get(1)?;
            let daily_json: String = row.get(2)?;
            let model_daily_json: String = row.get(3)?;
            let Ok(daily) = serde_json::from_str(&daily_json) else {
                continue;
            };
            let Ok(model_totals_by_day) = serde_json::from_str(&model_daily_json) else {
                continue;
            };
            out.push(ArchivedUsage {
                file_path,
                session_cwd,
                daily,
                model_totals_by_day,
            });
        }
        Ok(out)
    }

    /// Stores the aggregates of cache rows whose log files are gone.
    pub(crate) fn archive(
        &mut self,
        harness: Harness,
        rows: &[(&str, &CachedFileScanEntry)],
        archived_at: i64,
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO archived_usage(
                    harness, file_path, session_cwd, daily_json, model_daily_json, archived_at
                )
                VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(harness, file_path) DO UPDATE SET
                    session_cwd=excluded.session_cwd,
                    daily_json=excluded.daily_json,
                    model_daily_json=excluded.model_daily_json,
                    archived_at=excluded.archived_at;
                ",
            )?;
            for (file_path, entry) in rows {
                stmt.execute(params![
                    harness.key(),
                    file_path,
                    entry.session_cwd.as_deref(),
                    serde_json::to_string(&entry.daily)?,
                    serde_json::to_string(&entry.model_totals_by_day)?,
                    archived_at,
                ])?;
            }
        }
        tx.commit()?;
        enforce_private_db_files(&self.path, "usage archive")?;
        Ok(())
    }

    /// Drops archived rows whose log file exists again; the file is counted
    /// from the scan cache instead.
    pub(crate) fn forget(&mut self, harness: Harness, file_paths: &[&str]) -> Result<()> {
        if file_paths.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt =
                tx.prepare("DELETE FROM archived_usage WHERE harness = ?1 AND file_path = ?2;")?;
            for file_path in file_paths {
                stmt.execute(params![harness.key(), file_path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn init_archive_tables(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS archive_meta (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS archived_usage (
            harness TEXT NOT NULL,
            file_path TEXT NOT NULL,
            session_cwd TEXT,
            daily_json TEXT NOT NULL,
            model_daily_json TEXT NOT NULL,
            archived_at INTEGER NOT NULL,
            PRIMARY KEY (harness, file_path)
        );
        ",
    )?;
    let layout_version: Option<i64> = tx
        .query_row(
            "SELECT value FROM archive_meta WHERE key = 'layout_version';",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match layout_version {
        Some(ARCHIVE_LAYOUT_VERSION) => {}
        Some(other) => anyhow::bail!(
            "Unsupported usage archive layout version: {other} (expected {ARCHIVE_LAYOUT_VERSION})"
        ),
        None => {
            tx.execute(
                "INSERT INTO archive_meta(key, value) VALUES('layout_version', ?1);",
                params![ARCHIVE_LAYOUT_VERSION],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
