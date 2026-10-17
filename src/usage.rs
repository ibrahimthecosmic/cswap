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
use crate::lease::Cluster;
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
    let cluster = Cluster::load().unwrap_or(Cluster {
        borrowed: Vec::new(),
    });

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
        let borrowed = cluster.get(&account).is_some();
        let row = match fetch_one(&account, is_active, live, borrowed) {
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
            Err(state) => {
                let (usage, at) = fallback(&state, cached);
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

fn fetch_one(
    account: &Account,
    is_active: bool,
    live: Option<&str>,
    borrowed: bool,
) -> Result<Usage, State> {
    let credentials = match resolve_credentials(account, is_active, live) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(State::NoBackup),
        Err(e) => return Err(State::Error(e)),
    };
    if is_api_key(&credentials) {
        return Err(State::ApiKey);
    }

    // A borrowed login is never refreshed here: it has no refresh token, and
    // the lineage belongs to the owner. Its `expiresAt` is advisory too — the
    // owner renews it — so the token is simply tried, and a rejection means the
    // owner has rotated it and this machine has not heard yet.
    if borrowed {
        let token = access_token(&credentials)
            .ok_or_else(|| State::Error("borrowed login has no access token".into()))?;
        return match api::fetch_usage(&token) {
            Ok(usage) if usage.is_empty() => {
                Err(State::Error("no usage windows in response".into()))
            }
            Ok(usage) => Ok(usage),
            Err(api::ApiError::Unauthorized) => Err(State::Error(
                "borrowed token rejected — the owner has renewed it; run `cswap lease`".into(),
            )),
            Err(e) => Err(State::Error(e.to_string())),
        };
    }

    let token = match access_token(&credentials) {
        Some(t) if !expires_within(&credentials, EXPIRY_SKEW) => t,
        _ => match ensure_fresh_token(account, is_active, &credentials, EXPIRY_SKEW) {
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
            let token = ensure_fresh_token(account, is_active, &credentials, EXPIRY_SKEW)?;
            api::fetch_usage(&token).map_err(|e| State::Error(e.to_string()))
        }
        Err(e) => Err(State::Error(e.to_string())),
    }
}

/// What a row shows when this run could not fetch. A failure falls back to the
/// last known numbers rather than showing nothing: stale quota beats no quota
/// when you are deciding where to switch. A dead login is the exception —
/// nothing can switch to it until someone logs in again, so its old numbers
/// only make it look usable.
fn fallback(state: &State, cached: Option<(Usage, i64)>) -> (Option<Usage>, Option<i64>) {
    match cached {
        Some((usage, at)) if !matches!(state, State::Dead) => (Some(usage), Some(at)),
        _ => (None, None),
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
    skew: i64,
) -> Result<String, State> {
    let _gate =
        FileLock::consume(account.num, Duration::from_secs(15)).map_err(|e| State::Error(e))?;

    // Re-read inside the gate: another process may have refreshed this slot
    // while we waited, in which case its token is the live one and ours is spent.
    let current = match store::read_backup(account.num, &account.email) {
        Ok(Some(c)) => c,
        _ => credentials.to_string(),
    };
    if !expires_within(&current, skew) {
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

/// The credential an owner lends out: the slot's full login, refreshed first if
/// it expires within `ahead` seconds. `is_live` says Claude Code on this
/// machine is logged in as this account, in which case the live store is the
/// truth and the backup may be a spent generation.
///
/// Returns the whole credential, refresh token included — the caller strips it.
pub fn fresh_credential(account: &Account, is_live: bool, ahead: i64) -> R<String> {
    if !is_live {
        let current = store::read_backup(account.num, &account.email)?
            .ok_or_else(|| format!("account {} has no stored login", account.num))?;
        if !expires_within(&current, ahead) {
            return Ok(current);
        }
        ensure_fresh_token(account, false, &current, ahead).map_err(state_message)?;
        return store::read_backup(account.num, &account.email)?
            .ok_or_else(|| format!("account {} lost its stored login", account.num));
    }

    let current = crate::live::read_credentials()?.ok_or("Claude Code is not logged in")?;
    if !expires_within(&current, ahead) {
        return Ok(current);
    }
    refresh_live(account, ahead)
}

/// Refresh the credential Claude Code is using right now.
///
/// This is the one place cswap holds Claude Code's credential locks across a
/// network call, and it does so on purpose: it is doing exactly what Claude Code
/// does under those locks — read, refresh, write — and serialising with it is
/// the only way to guarantee the two never POST the same single-use refresh
/// token. The loser of that race would be told the lineage is dead, and if the
/// loser were Claude Code, it would log the owner out. The call is bounded by
/// the API timeout (10s), well inside the 60s the lock protocol allows a holder.
fn refresh_live(account: &Account, ahead: i64) -> R<String> {
    // Same order `ensure_fresh_token` takes them — the gate, then Claude Code's
    // locks — so a concurrent `cswap list` cannot deadlock against this.
    let _gate = FileLock::consume(account.num, Duration::from_secs(15))?;
    let _locks = crate::lock::DirLock::credentials()?;

    // Re-read under the locks: Claude Code may have refreshed while we waited,
    // and then its generation is current and ours would be a spent token.
    let current = crate::live::read_credentials()?.ok_or("Claude Code is not logged in")?;
    if !expires_within(&current, ahead) {
        return Ok(current);
    }
    match api::refresh(&current) {
        api::Refresh::Rotated(rotated) => {
            // Live first: that is where the lineage now lives, and a crash
            // between the two writes leaves the backup merely stale, which
            // every path here already tolerates.
            crate::live::write_credentials(&rotated)
                .map_err(|e| format!("could not persist refreshed token: {e}"))?;
            if let Err(e) = store::write_backup(account.num, &account.email, &rotated) {
                eprintln!("cswap: warning: refreshed, but could not update the slot backup: {e}");
            }
            Ok(rotated)
        }
        api::Refresh::Dead => Err(format!(
            "the login for {} has expired — log in as it again on this machine",
            account.email
        )),
        api::Refresh::Transient(msg) => Err(format!("refresh failed: {msg}")),
    }
}

fn state_message(state: State) -> String {
    match state {
        State::Dead => "the stored login has expired — log in as it again on this machine".into(),
        State::Error(msg) => msg,
        other => format!("could not refresh ({})", other.token()),
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

fn expires_within(credentials: &str, secs: i64) -> bool {
    match oauth_block(credentials).and_then(|o| o.get_i64("expiresAt")) {
        // expiresAt is epoch milliseconds.
        Some(ms) => ms / 1000 <= timefmt::now_unix() + secs,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cached() -> Option<(Usage, i64)> {
        let window = Window {
            label: "7d".into(),
            pct: 99.0,
            resets_at: None,
        };
        Some((
            Usage {
                seven_day: Some(window),
                ..Usage::default()
            },
            1_000,
        ))
    }

    #[test]
    fn a_failed_fetch_keeps_the_last_known_numbers() {
        let (usage, at) = fallback(&State::Error("timeout".into()), cached());
        assert!(usage.is_some());
        assert_eq!(at, Some(1_000));
    }

    #[test]
    fn a_dead_login_shows_no_numbers() {
        assert!(fallback(&State::Dead, cached()).0.is_none());
    }
}
