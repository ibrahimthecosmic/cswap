//! Turning stored credentials into quota numbers: token freshness, the one
//! dangerous write in this program, and a small on-disk cache.
//!
//! The dangerous write is refreshing an inactive slot. Refresh tokens are
//! single-use — a successful POST invalidates the token that was sent — so the
//! rotated credential must reach disk before anything else can fail, and two
//! processes must never POST the same token. Hence: a per-slot `flock` gate, a
//! re-read inside it, and a durable write before the token is used for anything.

use std::time::Duration;

use crate::api;
use crate::fsx::{self, R};
use crate::json::Json;
use crate::lock::FileLock;
use crate::model::{Row, State, Usage, Window};
use crate::paths;
use crate::store::{self, Account};
use crate::timefmt;

/// How long a cached reading is served before we go back to the API. Usage
/// moves in minutes, not seconds, and every account polled on every `list` is
/// how you end up rate-limited by your own dashboard.
const CACHE_TTL: i64 = 120;
/// Refresh a token that is already expired or about to be.
const EXPIRY_SKEW: i64 = 60;

/// Collect one row per account, hitting the network only where the cache cannot
/// answer. `force` bypasses the cache; `offline` never touches the network.
pub fn collect(store_ref: &store::Store, force: bool, offline: bool) -> Vec<Row> {
    let accounts = store_ref.accounts();
    let active = store_ref.active();
    let mut cache = Cache::load();
    let live_credentials = crate::live::read_credentials().ok().flatten();

    let mut rows = Vec::new();
    for account in accounts {
        let is_active = Some(account.num) == active;
        let cached = cache.get(account.num);

        if offline {
            rows.push(match cached {
                Some((usage, at)) => Row {
                    account,
                    active: is_active,
                    usage: Some(usage),
                    fetched_at: Some(at),
                    state: State::Cached,
                },
                None => Row {
                    account,
                    active: is_active,
                    usage: None,
                    fetched_at: None,
                    state: State::Cached,
                },
            });
            continue;
        }

        if !force {
            if let Some((usage, at)) = &cached {
                if timefmt::now_unix() - at < CACHE_TTL {
                    rows.push(Row {
                        account,
                        active: is_active,
                        usage: Some(usage.clone()),
                        fetched_at: Some(*at),
                        state: State::Live,
                    });
                    continue;
                }
            }
        }

        let live = if is_active {
            live_credentials.as_deref()
        } else {
            None
        };
        let row = match fetch_one(&account, is_active, live) {
            Ok(usage) => {
                cache.put(account.num, &account.email, &usage);
                Row {
                    account,
                    active: is_active,
                    usage: Some(usage),
                    fetched_at: Some(timefmt::now_unix()),
                    state: State::Live,
                }
            }
            // A failed fetch falls back to the last known numbers rather than
            // showing nothing: stale quota beats no quota when you are deciding
            // where to switch.
            Err(state) => {
                let (usage, at) = match cached {
                    Some((u, t)) => (Some(u), Some(t)),
                    None => (None, None),
                };
                Row {
                    account,
                    active: is_active,
                    usage,
                    fetched_at: at,
                    state,
                }
            }
        };
        rows.push(row);
    }

    cache.save();
    rows
}

fn fetch_one(account: &Account, is_active: bool, live: Option<&str>) -> Result<Usage, State> {
    let credentials = match resolve_credentials(account, is_active, live) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(State::NoBackup),
        Err(e) => return Err(State::Error(e)),
    };
    if is_api_key(&credentials) {
        return Err(State::ApiKey);
    }

    let token = match access_token(&credentials) {
        Some(t) if !is_expired(&credentials) => t,
        _ => match ensure_fresh_token(account, is_active, &credentials) {
            Ok(t) => t,
            Err(state) => return Err(state),
        },
    };

    match api::fetch_usage(&token) {
        Ok(usage) if usage.is_empty() => Err(State::Error("no usage windows in response".into())),
        Ok(usage) => Ok(usage),
        // The token was valid by its own clock but the server disagreed —
        // refresh once and retry, which is the whole of our retry policy.
        Err(api::ApiError::Unauthorized) => {
            let token = ensure_fresh_token(account, is_active, &credentials)?;
            api::fetch_usage(&token).map_err(|e| State::Error(e.to_string()))
        }
        Err(e) => Err(State::Error(e.to_string())),
    }
}

fn resolve_credentials(
    account: &Account,
    is_active: bool,
    live: Option<&str>,
) -> R<Option<String>> {
    if is_active {
        if let Some(live) = live {
            return Ok(Some(live.to_string()));
        }
    }
    store::read_backup(account.num, &account.email)
}

/// Refresh, persist, and hand back the new access token.
///
/// Ordering is the point of this function: the rotated credential is written to
/// the slot's backup *before* it is used, and for the active slot it is then
/// compare-and-swapped into Claude Code's live store under Claude Code's own
/// lock. The network call happens outside every lock — holding Claude Code's
/// credential lock across a round trip would stall its own refresh.
fn ensure_fresh_token(
    account: &Account,
    is_active: bool,
    credentials: &str,
) -> Result<String, State> {
    let _gate =
        FileLock::consume(account.num, Duration::from_secs(15)).map_err(|e| State::Error(e))?;

    // Re-read inside the gate: another process may have refreshed this slot
    // while we waited, in which case its token is the live one and ours is spent.
    let current = match store::read_backup(account.num, &account.email) {
        Ok(Some(c)) => c,
        _ => credentials.to_string(),
    };
    if !is_expired(&current) {
        if let Some(token) = access_token(&current) {
            return Ok(token);
        }
    }

    let consumed = refresh_token_of(&current);
    match api::refresh(&current) {
        api::Refresh::Rotated(rotated) => {
            let token = access_token(&rotated)
                .ok_or_else(|| State::Error("refresh returned no access token".into()))?;
            store::write_backup(account.num, &account.email, &rotated)
                .map_err(|e| State::Error(format!("could not persist refreshed token: {e}")))?;
            if is_active {
                if let Err(e) = swap_live_credential(&rotated, consumed.as_deref()) {
                    // The backup already holds the good token, so the account is
                    // not lost; say so rather than failing the whole row.
                    eprintln!("cswap: warning: {e}");
                }
            }
            Ok(token)
        }
        api::Refresh::Dead => {
            // Losing a race means POSTing a token another process already spent,
            // which the server answers exactly like a genuinely dead lineage.
            // Only call it dead if the credential on disk is still the one we sent.
            if refresh_token_of(&current) == consumed
                && !rotated_elsewhere(account, consumed.as_deref())
            {
                Err(State::Dead)
            } else {
                Err(State::Error("token rotated concurrently; retry".into()))
            }
        }
        api::Refresh::Transient(msg) => Err(State::Error(msg)),
    }
}

/// Write a refreshed credential into Claude Code's live store, but only if
/// Claude Code has not refreshed it itself in the meantime.
fn swap_live_credential(rotated: &str, consumed: Option<&str>) -> R<()> {
    let _locks = crate::lock::DirLock::credentials()?;
    let current = crate::live::read_credentials()?;
    if let (Some(current), Some(consumed)) = (&current, consumed) {
        if refresh_token_of(current).as_deref() != Some(consumed) {
            // Claude Code rotated it first; its token is the live generation and
            // ours is already superseded. Leave the live store alone.
            return Ok(());
        }
    }
    crate::live::write_credentials(rotated)
}

fn rotated_elsewhere(account: &Account, consumed: Option<&str>) -> bool {
    match store::read_backup(account.num, &account.email) {
        Ok(Some(current)) => refresh_token_of(&current).as_deref() != consumed,
        _ => false,
    }
}

pub fn is_api_key(credentials: &str) -> bool {
    let text = credentials.trim();
    text.starts_with("sk-ant-api") && !text.starts_with('{')
}

fn oauth_block(credentials: &str) -> Option<Json> {
    Json::parse(credentials).ok()?.get("claudeAiOauth").cloned()
}

fn access_token(credentials: &str) -> Option<String> {
    oauth_block(credentials)?
        .get_str("accessToken")
        .map(str::to_string)
}

fn refresh_token_of(credentials: &str) -> Option<String> {
    oauth_block(credentials)?
        .get_str("refreshToken")
        .map(str::to_string)
}

fn is_expired(credentials: &str) -> bool {
    match oauth_block(credentials).and_then(|o| o.get_i64("expiresAt")) {
        // expiresAt is epoch milliseconds.
        Some(ms) => ms / 1000 <= timefmt::now_unix() + EXPIRY_SKEW,
        // No expiry recorded: treat as fresh and let the API decide, rather than
        // spending a single-use refresh token on a guess.
        None => false,
    }
}

/// Last-known usage per slot. Deliberately a separate file from claude-swap's
/// own `cache/usage.json`: that one carries poll scheduling and quarantine state
/// this port does not implement, and writing a partial version of it would
/// confuse the Python tool if both are installed.
struct Cache {
    data: Json,
    dirty: bool,
}

impl Cache {
    fn path() -> std::path::PathBuf {
        paths::cache_dir().join("usage-cswap-rs.json")
    }

    fn load() -> Cache {
        let data = fsx::read_json_file(&Cache::path())
            .ok()
            .flatten()
            .filter(Json::is_obj)
            .unwrap_or_else(|| {
                let mut fresh = Json::obj();
                fresh.set("schemaVersion", Json::num(1));
                fresh.set("accounts", Json::obj());
                fresh
            });
        Cache { data, dirty: false }
    }

    fn get(&self, num: i64) -> Option<(Usage, i64)> {
        let entry = self.data.get("accounts")?.get(&num.to_string())?;
        let at = entry.get_i64("fetchedAt")?;
        let usage = entry.get("usage")?;
        let window = |key: &str| -> Option<Window> {
            let w = usage.get(key)?;
            Some(Window {
                label: w.get_str("label").unwrap_or(key).to_string(),
                pct: w.get_f64("pct")?,
                resets_at: w.get_i64("resetsAt"),
            })
        };
        let scoped = usage
            .get("scoped")
            .and_then(Json::as_arr)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|w| {
                        Some(Window {
                            label: w.get_str("label")?.to_string(),
                            pct: w.get_f64("pct")?,
                            resets_at: w.get_i64("resetsAt"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some((
            Usage {
                five_hour: window("fiveHour"),
                seven_day: window("sevenDay"),
                scoped,
            },
            at,
        ))
    }

    fn put(&mut self, num: i64, email: &str, usage: &Usage) {
        let encode = |w: &Window| {
            let mut out = Json::obj();
            out.set("label", Json::str(&w.label));
            out.set("pct", Json::Num(format!("{:.1}", w.pct)));
            if let Some(at) = w.resets_at {
                out.set("resetsAt", Json::num(at));
            }
            out
        };
        let mut encoded = Json::obj();
        if let Some(w) = &usage.five_hour {
            encoded.set("fiveHour", encode(w));
        }
        if let Some(w) = &usage.seven_day {
            encoded.set("sevenDay", encode(w));
        }
        if !usage.scoped.is_empty() {
            encoded.set(
                "scoped",
                Json::Arr(usage.scoped.iter().map(encode).collect()),
            );
        }

        let mut entry = Json::obj();
        entry.set("email", Json::str(email));
        entry.set("fetchedAt", Json::num(timefmt::now_unix()));
        entry.set("usage", encoded);

        if self.data.get("accounts").map(Json::is_obj) != Some(true) {
            self.data.set("accounts", Json::obj());
        }
        if let Some(accounts) = self.data.get_mut("accounts") {
            accounts.set(&num.to_string(), entry);
        }
        self.dirty = true;
    }

    fn save(&self) {
        if !self.dirty {
            return;
        }
        // A cache is a convenience; failing to write one is not worth an error.
        let _ = fsx::write_atomic(&Cache::path(), &self.data.dump_pretty());
    }
}
