/// A coding-agent CLI whose local logs and limits llmon reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Harness {
    Codex,
}

impl Harness {
    /// Stable identifier used in the scan cache and config. Never change an
    /// existing value: cache rows and settings are keyed by it.
    pub fn key(self) -> &'static str {
        match self {
            Harness::Codex => "codex",
        }
    }
}

pub const ALL_HARNESSES: [Harness; 1] = [Harness::Codex];
