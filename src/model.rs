//! The shapes `list` renders. Defined here rather than next to the code that
//! fetches them so a `--no-default-features` build — no network, no
//! dependencies — still has rows to print.

use crate::store::Account;
use crate::timefmt;

/// A quota window: a percentage used, and when it resets.
#[derive(Clone, Debug)]
pub struct Window {
    pub label: String,
    pub pct: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Per-model weekly windows, e.g. "Fable". Present only on newer responses.
    pub scoped: Vec<Window>,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none() && self.scoped.is_empty()
    }

    #[cfg_attr(not(feature = "usage"), allow(dead_code))]
    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.five_hour
            .iter()
            .chain(self.seven_day.iter())
            .chain(self.scoped.iter())
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "usage"), allow(dead_code))]
pub enum State {
    /// Numbers we stand behind: fetched this run, or cached and still young.
    Live,
    /// Cached numbers past their freshness window, or a build with no network.
    Cached,
    /// The slot has no stored credential — added on another machine, or purged.
    NoBackup,
    /// A managed API key. It has no subscription quota, so there is nothing to show.
    ApiKey,
    /// The refresh lineage is dead; only a fresh login recovers this slot.
    Dead,
    Error(String),
}

impl State {
    /// The `usageStatus` token in `--json` output.
    pub fn token(&self) -> &'static str {
        match self {
            State::Live => "ok",
            State::Cached => "stale",
            State::NoBackup => "no_credentials",
            State::ApiKey => "api_key",
            State::Dead => "token_expired",
            State::Error(_) => "unavailable",
        }
    }
}

pub struct Row {
    pub account: Account,
    pub active: bool,
    pub usage: Option<Usage>,
    pub fetched_at: Option<i64>,
    pub state: State,
}

impl Row {
    pub fn age(&self) -> Option<i64> {
        self.fetched_at.map(|t| (timefmt::now_unix() - t).max(0))
    }

    pub fn plain(account: Account, active: bool) -> Row {
        Row {
            account,
            active,
            usage: None,
            fetched_at: None,
            state: State::Cached,
        }
    }
}
