//! The `export` / `import` envelope: accounts as one self-contained JSON file.
//!
//! ```text
//! {
//!   "schemaVersion": 1,
//!   "kind": "cswap-export",
//!   "exportedAt": "2026-09-18T09:12:44Z",
//!   "accounts": [
//!     {
//!       "slot": 2,
//!       "email": "you@example.com",
//!       "alias": "work",
//!       "uuid": "...", "organizationUuid": "...", "organizationName": "...",
//!       "oauthAccount": { ... },
//!       "credentials": { "claudeAiOauth": { ... } }
//!     }
//!   ]
//! }
//! ```
//!
//! Two deliberate choices about what travels:
//!
//! * **The credential is embedded as JSON, not base64.** The `.enc` backups are
//!   base64 for claude-swap's sake; an export is a file a human moves between
//!   machines, and one they can read is one they can audit before importing.
//!   Either way it is a live OAuth token in plaintext — see `cmd::export`, which
//!   writes it 0600 and says so.
//! * **Only `oauthAccount` travels, not the whole config snapshot.** The stored
//!   snapshot is the user's entire `~/.claude.json` — projects, history, MCP
//!   servers, ~80 KB of state that has nothing to do with the login. `switch`
//!   reads exactly one key out of it, so exporting exactly that key is both
//!   smaller and the only part that is anyone else's business.

use crate::fsx::R;
use crate::json::Json;
use crate::store::Account;
use crate::timefmt;

pub const KIND: &str = "cswap-export";
pub const SCHEMA_VERSION: i64 = 1;

/// One account in transit: who it is, its credential, and the `oauthAccount`
/// block that names it in `~/.claude.json`.
#[derive(Clone, Debug)]
pub struct Portable {
    /// The slot it sat in where it was exported. A hint only — `import` picks
    /// the slot on *this* machine, since the two stores need not agree.
    pub slot: Option<i64>,
    pub email: String,
    pub alias: Option<String>,
    pub uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    /// The credential JSON exactly as it will be written back to disk.
    pub credentials: String,
    pub oauth_account: Json,
}

impl Portable {
    pub fn from_account(account: &Account, credentials: String, oauth_account: Json) -> Portable {
        Portable {
            slot: Some(account.num),
            email: account.email.clone(),
            alias: account.alias.clone(),
            uuid: account.uuid.clone(),
            org_uuid: account.org_uuid.clone(),
            org_name: account.org_name.clone(),
            credentials,
            oauth_account,
        }
    }

    /// The store record this becomes once `import` has chosen its slot.
    pub fn to_account(&self, num: i64, alias: Option<String>) -> Account {
        Account {
            num,
            email: self.email.clone(),
            uuid: self.uuid.clone(),
            org_uuid: self.org_uuid.clone(),
            org_name: self.org_name.clone(),
            alias: alias.or_else(|| self.alias.clone()),
        }
    }
}

pub fn envelope(items: &[Portable]) -> Json {
    let mut root = Json::obj();
    root.set("schemaVersion", Json::num(SCHEMA_VERSION));
    root.set("kind", Json::str(KIND));
    root.set(
        "exportedAt",
        Json::str(timefmt::utc_stamp(timefmt::now_unix())),
    );
    root.set("accounts", Json::Arr(items.iter().map(entry).collect()));
    root
}

fn entry(item: &Portable) -> Json {
    let mut out = Json::obj();
    if let Some(slot) = item.slot {
        out.set("slot", Json::num(slot));
    }
    out.set("email", Json::str(&item.email));
    for (key, value) in [
        ("alias", &item.alias),
        ("uuid", &item.uuid),
        ("organizationUuid", &item.org_uuid),
        ("organizationName", &item.org_name),
    ] {
        if let Some(value) = value {
            out.set(key, Json::str(value));
        }
    }
    out.set("oauthAccount", item.oauth_account.clone());
    out.set("credentials", credential_value(&item.credentials));
    out
}

/// A credential object is embedded as an object so the file reads; anything
/// else — an API key, say — travels as the string it is.
fn credential_value(text: &str) -> Json {
    match Json::parse(text) {
        Ok(value) if value.is_obj() => value,
        _ => Json::str(text),
    }
}

pub fn parse(text: &str) -> R<Vec<Portable>> {
    let root = Json::parse(text).map_err(|e| format!("not a cswap export: {e}"))?;
    match root.get_str("kind") {
        Some(KIND) => {}
        Some(other) => return Err(format!("not a cswap export: kind is '{other}'")),
        None => return Err("not a cswap export: no 'kind' field".into()),
    }
    // An older cswap reading a newer file would drop whatever the new version
    // added — for a credential, silently importing something incomplete.
    let version = root.get_i64("schemaVersion").unwrap_or(SCHEMA_VERSION);
    if version > SCHEMA_VERSION {
        return Err(format!(
            "export is schema version {version}; this cswap understands {SCHEMA_VERSION} \
             — run `cswap upgrade`"
        ));
    }
    let entries = root
        .get("accounts")
        .and_then(Json::as_arr)
        .ok_or("not a cswap export: no 'accounts' array")?;
    if entries.is_empty() {
        return Err("this export contains no accounts".into());
    }
    entries.iter().map(read_entry).collect()
}

fn read_entry(value: &Json) -> R<Portable> {
    let email = value
        .get_str("email")
        .filter(|e| !e.trim().is_empty())
        .ok_or("an exported account has no 'email'")?
        .to_string();

    let credentials = match value.get("credentials") {
        Some(credential @ Json::Obj(_)) => credential.dump(),
        Some(Json::Str(s)) if !s.trim().is_empty() => s.clone(),
        Some(_) => {
            return Err(format!(
                "{email}: 'credentials' must be an object or a string"
            ))
        }
        None => return Err(format!("{email}: no 'credentials' to import")),
    };

    // Without this block `switch` has nothing to write into ~/.claude.json, and
    // a half-named login is worse than a refused import.
    let oauth_account = match value.get("oauthAccount") {
        Some(block) if block.is_obj() => block.clone(),
        Some(_) => return Err(format!("{email}: 'oauthAccount' is not an object")),
        None => return Err(format!("{email}: no 'oauthAccount' block")),
    };

    let text = |key: &str| value.get_str(key).map(str::to_string);
    Ok(Portable {
        slot: value.get_i64("slot"),
        email,
        alias: text("alias"),
        uuid: text("uuid"),
        org_uuid: text("organizationUuid"),
        org_name: text("organizationName"),
        credentials,
        oauth_account,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Portable {
        Portable {
            slot: Some(2),
            email: "a@x.com".into(),
            alias: Some("work".into()),
            uuid: Some("u-1".into()),
            org_uuid: Some("o-1".into()),
            org_name: Some("Acme".into()),
            credentials: r#"{"claudeAiOauth":{"accessToken":"t","expiresAt":1787811738997}}"#
                .into(),
            oauth_account: Json::parse(r#"{"emailAddress":"a@x.com","accountUuid":"u-1"}"#)
                .unwrap(),
        }
    }

    #[test]
    fn round_trips_an_account_through_the_envelope() {
        let text = envelope(&[sample()]).dump_pretty();
        let back = parse(&text).unwrap();
        assert_eq!(back.len(), 1);
        let item = &back[0];
        assert_eq!(item.email, "a@x.com");
        assert_eq!(item.alias.as_deref(), Some("work"));
        assert_eq!(item.slot, Some(2));
        assert_eq!(item.org_name.as_deref(), Some("Acme"));
        // The credential comes back byte-identical — including the 13-digit
        // `expiresAt`, which a float would have rounded into something Claude
        // Code rejects.
        assert_eq!(item.credentials, sample().credentials);
        assert_eq!(item.oauth_account.get_str("accountUuid"), Some("u-1"));
    }

    #[test]
    fn a_non_json_credential_travels_as_a_string() {
        let mut item = sample();
        item.credentials = "sk-ant-api03-xxx".into();
        let back = parse(&envelope(&[item]).dump_pretty()).unwrap();
        assert_eq!(back[0].credentials, "sk-ant-api03-xxx");
    }

    #[test]
    fn rejects_files_that_are_not_ours_or_are_incomplete() {
        let cases = [
            r#"{"accounts":[]}"#,
            r#"{"kind":"something-else","accounts":[]}"#,
            r#"{"kind":"cswap-export"}"#,
            r#"{"kind":"cswap-export","accounts":[]}"#,
            // No email, no credential, no oauthAccount: each on its own is fatal.
            r#"{"kind":"cswap-export","accounts":[{"credentials":{},"oauthAccount":{}}]}"#,
            r#"{"kind":"cswap-export","accounts":[{"email":"a@x.com","oauthAccount":{}}]}"#,
            r#"{"kind":"cswap-export","accounts":[{"email":"a@x.com","credentials":{}}]}"#,
            r#"{"kind":"cswap-export","accounts":[{"email":"a@x.com","credentials":{},"oauthAccount":7}]}"#,
            "not json at all",
        ];
        for case in cases {
            assert!(parse(case).is_err(), "should reject {case}");
        }
    }

    #[test]
    fn refuses_a_schema_from_the_future() {
        let text = r#"{"kind":"cswap-export","schemaVersion":99,"accounts":[]}"#;
        let err = parse(text).unwrap_err();
        assert!(err.contains("upgrade"), "{err}");
    }
}
