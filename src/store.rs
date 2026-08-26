//! claude-swap's on-disk account store, read and written in its native layout.
//!
//! ```text
//! <backup-root>/
//!   sequence.json                                  registry + rotation order
//!   configs/.claude-config-{n}-{email}.json         the slot's ~/.claude.json snapshot
//!   credentials/.creds-{n}-{email}.enc              base64 of the slot's credential JSON
//!   credentials/.creds-{n}-{email}.enc.prev         the generation before it
//! ```
//!
//! The `.enc` suffix is claude-swap's and predates this port; the bytes are
//! base64, not ciphertext. On Linux and Windows that is also how Claude Code
//! itself stores the live credential (plaintext, mode 0600), so the backup is no
//! weaker than the thing it backs up. On macOS the live credential lives in the
//! Keychain and these files are the fallback path only.

use std::fs;
use std::path::PathBuf;

use crate::fsx::{self, R};
use crate::json::Json;
use crate::paths;
use crate::timefmt;

#[derive(Clone, Debug)]
pub struct Account {
    pub num: i64,
    pub email: String,
    pub uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub alias: Option<String>,
}

impl Account {
    /// How the account is named in output: alias if it has one, else email.
    pub fn label(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.email)
    }
}

pub struct Store {
    pub data: Json,
}

impl Store {
    pub fn load() -> R<Store> {
        let path = paths::sequence_file();
        let data = match fsx::read_json_file(&path)? {
            Some(v) if v.is_obj() => v,
            Some(_) => return Err(format!("{} is not a JSON object", path.display())),
            None => {
                let mut fresh = Json::obj();
                fresh.set("activeAccountNumber", Json::Null);
                fresh.set(
                    "lastUpdated",
                    Json::str(timefmt::utc_stamp(timefmt::now_unix())),
                );
                fresh.set("sequence", Json::Arr(Vec::new()));
                fresh.set("accounts", Json::obj());
                fresh
            }
        };
        Ok(Store { data })
    }

    pub fn save(&mut self) -> R<()> {
        self.data.set(
            "lastUpdated",
            Json::str(timefmt::utc_stamp(timefmt::now_unix())),
        );
        fsx::write_atomic(&paths::sequence_file(), &self.data.dump_pretty())
    }

    pub fn active(&self) -> Option<i64> {
        self.data.get_i64("activeAccountNumber")
    }

    pub fn set_active(&mut self, num: i64) {
        self.data.set("activeAccountNumber", Json::num(num));
    }

    /// Accounts in rotation order (`sequence`), with any registered but
    /// unsequenced slot appended so nothing is invisible.
    pub fn accounts(&self) -> Vec<Account> {
        let registry = match self.data.get("accounts") {
            Some(Json::Obj(entries)) => entries,
            _ => return Vec::new(),
        };
        let order: Vec<i64> = self
            .data
            .get("sequence")
            .and_then(Json::as_arr)
            .map(|items| items.iter().filter_map(Json::as_i64).collect())
            .unwrap_or_default();

        let build = |key: &str, value: &Json| -> Option<Account> {
            let num: i64 = key.parse().ok()?;
            Some(Account {
                num,
                email: value.get_str("email").unwrap_or("(unknown)").to_string(),
                uuid: value.get_str("uuid").map(str::to_string),
                org_uuid: value.get_str("organizationUuid").map(str::to_string),
                org_name: value.get_str("organizationName").map(str::to_string),
                alias: value.get_str("alias").map(str::to_string),
            })
        };

        let mut out = Vec::new();
        for num in &order {
            let key = num.to_string();
            if let Some((k, v)) = registry.iter().find(|(k, _)| *k == key) {
                if let Some(account) = build(k, v) {
                    out.push(account);
                }
            }
        }
        for (k, v) in registry {
            if !order.iter().any(|n| n.to_string() == *k) {
                if let Some(account) = build(k, v) {
                    out.push(account);
                }
            }
        }
        out
    }

    pub fn get(&self, num: i64) -> Option<Account> {
        self.accounts().into_iter().find(|a| a.num == num)
    }

    /// Resolve a user-supplied selector: slot number, email, or alias.
    /// Email and alias match case-insensitively; a bare number always wins.
    pub fn resolve(&self, selector: &str) -> R<i64> {
        let accounts = self.accounts();
        if let Ok(num) = selector.parse::<i64>() {
            return match accounts.iter().find(|a| a.num == num) {
                Some(a) => Ok(a.num),
                None => Err(format!("no account in slot {num}")),
            };
        }
        let needle = selector.to_ascii_lowercase();
        let hits: Vec<&Account> = accounts
            .iter()
            .filter(|a| {
                a.email.to_ascii_lowercase() == needle
                    || a.alias.as_deref().map(str::to_ascii_lowercase).as_deref() == Some(&needle)
            })
            .collect();
        match hits.len() {
            1 => Ok(hits[0].num),
            0 => Err(format!("no account matching '{selector}'")),
            _ => Err(format!(
                "'{selector}' matches several accounts; use the slot number"
            )),
        }
    }

    /// The slot after `from` in rotation order, wrapping. `None` when there is
    /// nowhere else to go.
    pub fn next_after(&self, from: Option<i64>) -> Option<i64> {
        let accounts = self.accounts();
        if accounts.len() < 2 {
            return accounts.first().map(|a| a.num).filter(|n| Some(*n) != from);
        }
        let idx = from.and_then(|f| accounts.iter().position(|a| a.num == f));
        Some(match idx {
            Some(i) => accounts[(i + 1) % accounts.len()].num,
            None => accounts[0].num,
        })
    }

    pub fn register(&mut self, account: &Account) {
        let key = account.num.to_string();
        let mut record = match self.data.get("accounts").and_then(|a| a.get(&key)) {
            Some(existing) => existing.clone(),
            None => Json::obj(),
        };
        record.set("email", Json::str(&account.email));
        if let Some(v) = &account.uuid {
            record.set("uuid", Json::str(v));
        }
        if let Some(v) = &account.org_uuid {
            record.set("organizationUuid", Json::str(v));
        }
        if let Some(v) = &account.org_name {
            record.set("organizationName", Json::str(v));
        }
        if record.get("added").is_none() {
            record.set("added", Json::str(timefmt::utc_stamp(timefmt::now_unix())));
        }

        if self.data.get("accounts").map(Json::is_obj) != Some(true) {
            self.data.set("accounts", Json::obj());
        }
        if let Some(Json::Obj(entries)) = self.data.get_mut("accounts") {
            match entries.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = record,
                None => entries.push((key.clone(), record)),
            }
        }

        let mut order: Vec<i64> = self
            .data
            .get("sequence")
            .and_then(Json::as_arr)
            .map(|items| items.iter().filter_map(Json::as_i64).collect())
            .unwrap_or_default();
        if !order.contains(&account.num) {
            order.push(account.num);
        }
        order.sort_unstable();
        self.data.set(
            "sequence",
            Json::Arr(order.into_iter().map(Json::num).collect()),
        );
    }

    pub fn unregister(&mut self, num: i64) {
        let key = num.to_string();
        if let Some(Json::Obj(entries)) = self.data.get_mut("accounts") {
            entries.retain(|(k, _)| *k != key);
        }
        let order: Vec<Json> = self
            .data
            .get("sequence")
            .and_then(Json::as_arr)
            .map(|items| {
                items
                    .iter()
                    .filter(|v| v.as_i64() != Some(num))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.data.set("sequence", Json::Arr(order));
        if self.active() == Some(num) {
            self.data.set("activeAccountNumber", Json::Null);
        }
    }

    /// The lowest unused slot number, 1-based.
    pub fn free_slot(&self) -> i64 {
        let used: Vec<i64> = self.accounts().iter().map(|a| a.num).collect();
        (1..).find(|n| !used.contains(n)).unwrap_or(1)
    }
}

pub fn backup_path(num: i64, email: &str) -> PathBuf {
    paths::credentials_dir().join(format!(".creds-{num}-{email}.enc"))
}

pub fn config_path(num: i64, email: &str) -> PathBuf {
    paths::configs_dir().join(format!(".claude-config-{num}-{email}.json"))
}

/// Read a slot's stored credential blob. `Ok(None)` means the slot has no
/// backup; an unreadable or corrupt one is an error, never a silent miss —
/// treating "I could not read it" as "there is nothing there" is how an account
/// gets overwritten.
pub fn read_backup(num: i64, email: &str) -> R<Option<String>> {
    let path = backup_path(num, email);
    let encoded = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if encoded.trim().is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    let bytes = b64_decode(&encoded, &path)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{} is not UTF-8", path.display()))
}

fn b64_decode(encoded: &str, path: &std::path::Path) -> R<Vec<u8>> {
    crate::b64::decode(encoded.trim()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write a slot's credential backup, keeping the previous generation as
/// `.enc.prev`. The rename is atomic, so a slot is never left without a
/// credential: either the new bytes or the old ones are on disk.
pub fn write_backup(num: i64, email: &str, credentials: &str) -> R<()> {
    let path = backup_path(num, email);
    if path.exists() {
        let prev = path.with_extension("enc.prev");
        // Best effort: losing the previous generation is not a reason to refuse
        // to store the current one.
        let _ = fs::copy(&path, &prev);
    }
    fsx::write_atomic(&path, &crate::b64::encode(credentials.as_bytes()))
}

pub fn remove_backup(num: i64, email: &str) {
    let path = backup_path(num, email);
    let _ = fs::remove_file(path.with_extension("enc.prev"));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(config_path(num, email));
}

pub fn read_config(num: i64, email: &str) -> R<Option<Json>> {
    fsx::read_json_file(&config_path(num, email))
}

pub fn write_config(num: i64, email: &str, config: &Json) -> R<()> {
    fsx::write_atomic(&config_path(num, email), &config.dump_pretty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(json: &str) -> Store {
        Store {
            data: Json::parse(json).unwrap(),
        }
    }

    fn two_accounts() -> Store {
        store_with(
            r#"{"activeAccountNumber":2,"sequence":[1,2],"accounts":{
                "1":{"email":"a@x.com","alias":"work"},
                "2":{"email":"b@x.com"}}}"#,
        )
    }

    #[test]
    fn resolves_by_slot_email_and_alias() {
        let store = two_accounts();
        assert_eq!(store.resolve("1").unwrap(), 1);
        assert_eq!(store.resolve("B@X.COM").unwrap(), 2);
        assert_eq!(store.resolve("work").unwrap(), 1);
        assert!(store.resolve("nobody").is_err());
        assert!(store.resolve("9").is_err());
    }

    #[test]
    fn rotation_wraps_and_a_lone_account_has_nowhere_to_go() {
        let store = two_accounts();
        assert_eq!(store.next_after(Some(1)), Some(2));
        assert_eq!(store.next_after(Some(2)), Some(1));
        // Unknown current account: start at the top of the sequence.
        assert_eq!(store.next_after(None), Some(1));

        let single = store_with(r#"{"sequence":[1],"accounts":{"1":{"email":"a@x.com"}}}"#);
        assert_eq!(single.next_after(Some(1)), None);
        assert_eq!(single.next_after(None), Some(1));
    }

    #[test]
    fn accounts_registered_outside_the_sequence_stay_visible() {
        let store = store_with(
            r#"{"sequence":[2],"accounts":{"1":{"email":"a@x.com"},"2":{"email":"b@x.com"}}}"#,
        );
        let nums: Vec<i64> = store.accounts().iter().map(|a| a.num).collect();
        assert_eq!(nums, vec![2, 1]);
    }

    #[test]
    fn register_keeps_the_sequence_sorted_and_free_slot_fills_gaps() {
        let mut store = store_with(
            r#"{"sequence":[1,3],"accounts":{
            "1":{"email":"a@x.com"},"3":{"email":"c@x.com"}}}"#,
        );
        assert_eq!(store.free_slot(), 2);
        store.register(&Account {
            num: 2,
            email: "b@x.com".into(),
            uuid: None,
            org_uuid: None,
            org_name: None,
            alias: None,
        });
        let nums: Vec<i64> = store.accounts().iter().map(|a| a.num).collect();
        assert_eq!(nums, vec![1, 2, 3]);
        assert!(store.get(2).unwrap().email == "b@x.com");
    }

    #[test]
    fn unregister_clears_the_active_marker_only_for_that_slot() {
        let mut store = two_accounts();
        store.unregister(1);
        assert_eq!(store.active(), Some(2));
        store.unregister(2);
        assert_eq!(store.active(), None);
        assert!(store.accounts().is_empty());
    }
}
