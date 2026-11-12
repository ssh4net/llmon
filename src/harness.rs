/// A coding-agent CLI whose local logs and limits llmon reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Harness {
    Codex,
    Claude,
    Antigravity,
    Grok,
}

impl Harness {
    /// Stable identifier used in the scan cache and config. Never change an
    /// existing value: cache rows and settings are keyed by it.
    pub fn key(self) -> &'static str {
        match self {
            Harness::Codex => "codex",
            Harness::Claude => "claude",
            Harness::Antigravity => "antigravity",
            Harness::Grok => "grok",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        ALL_HARNESSES
            .into_iter()
            .find(|harness| harness.key() == key)
    }

    /// This harness alone as a slice, for iteration.
    pub fn only(self) -> &'static [Harness] {
        match self {
            Harness::Codex => &[Harness::Codex],
            Harness::Claude => &[Harness::Claude],
            Harness::Antigravity => &[Harness::Antigravity],
            Harness::Grok => &[Harness::Grok],
        }
    }

    /// The page of the screens this harness is on.
    pub fn page(self) -> HarnessPage {
        match self {
            Harness::Codex | Harness::Claude => HarnessPage::First,
            Harness::Antigravity | Harness::Grok => HarnessPage::Second,
        }
    }
}

pub const ALL_HARNESSES: [Harness; 4] = [
    Harness::Codex,
    Harness::Claude,
    Harness::Antigravity,
    Harness::Grok,
];

/// The screens show two harnesses at a time, side by side in the combined
/// view. The first page has Codex and Claude Code, the second the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum HarnessPage {
    #[default]
    First,
    Second,
}

impl HarnessPage {
    /// The two harnesses of the page, left and right in the combined view.
    pub fn harnesses(self) -> [Harness; 2] {
        match self {
            HarnessPage::First => [Harness::Codex, Harness::Claude],
            HarnessPage::Second => [Harness::Antigravity, Harness::Grok],
        }
    }

    /// The same two harnesses as a slice, for iteration.
    pub fn slice(self) -> &'static [Harness] {
        match self {
            HarnessPage::First => &[Harness::Codex, Harness::Claude],
            HarnessPage::Second => &[Harness::Antigravity, Harness::Grok],
        }
    }

    pub fn other(self) -> Self {
        match self {
            HarnessPage::First => HarnessPage::Second,
            HarnessPage::Second => HarnessPage::First,
        }
    }

    pub fn number(self) -> u8 {
        match self {
            HarnessPage::First => 1,
            HarnessPage::Second => 2,
        }
    }

    pub fn store_value(self) -> &'static str {
        match self {
            HarnessPage::First => "1",
            HarnessPage::Second => "2",
        }
    }

    pub fn from_store(value: &str) -> Option<Self> {
        match value {
            "1" => Some(HarnessPage::First),
            "2" => Some(HarnessPage::Second),
            _ => None,
        }
    }
}
