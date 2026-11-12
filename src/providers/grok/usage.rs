//! Grok CLI usage parsing.

use crate::usage::{CachedFileScanEntry, FileScanSummary, HarnessParserState};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;

/// Incremental parser state persisted with each cache row.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParserState {}

pub(crate) fn parse_file_summary(
    _path: &Path,
    _max_jsonl_line_bytes: usize,
    _existing: Option<&CachedFileScanEntry>,
    _deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    Ok(FileScanSummary::empty(HarnessParserState::Grok(
        ParserState::default(),
    )))
}
