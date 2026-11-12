//! Antigravity CLI session history.

use crate::read::scan::{SessionDetail, SessionSummary};
use anyhow::{bail, Result};
use std::path::Path;

pub(crate) fn scan_session_summary(_path: &Path) -> Result<SessionSummary> {
    bail!("Antigravity conversations are not read yet")
}

pub(crate) fn load_session_detail(_path: &Path) -> Result<SessionDetail> {
    bail!("Antigravity conversations are not read yet")
}
