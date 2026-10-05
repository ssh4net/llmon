/// A coding-agent CLI whose local logs and limits llmon reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Harness {
    Codex,
    Claude,
}

impl Harness {
    /// Stable identifier used in the scan cache and config. Never change an
    /// existing value: cache rows and settings are keyed by it.
    pub fn key(self) -> &'static str {
        match self {
            Harness::Codex => "codex",
            Harness::Claude => "claude",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        ALL_HARNESSES
            .into_iter()
            .find(|harness| harness.key() == key)
    }
}

pub const ALL_HARNESSES: [Harness; 2] = [Harness::Codex, Harness::Claude];
