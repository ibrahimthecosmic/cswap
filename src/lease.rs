//! Borrowed logins: the lease that travels from an owner to a peer, and the
//! peer's record of which slots are borrowed and from where.
//!
//! A lease is one line of JSON — `cswap token` prints one, `--watch` prints one
//! per change — so it streams over an SSH pipe and can be read line by line:
//!
//! ```text
//! {"kind":"cswap-lease","schemaVersion":1,"issuedAt":"2026-09-22T05:24:51Z",
//!  "email":"you@example.com","uuid":"...","organizationUuid":"...",
//!  "organizationName":"...","oauthAccount":{...},
//!  "credentials":{"claudeAiOauth":{"accessToken":"...","expiresAt":...,
//!                                   "scopes":[...],"subscriptionType":"..."}}}
//! ```
//!
//! What a lease leaves out is the point of it. Refresh tokens are single-use and
//! a refresh revokes the access token before it, so exactly one machine — the
//! owner — may ever refresh a lineage. A peer gets the access token and nothing
//! that could spend the lineage: no `refreshToken`, and none of the credential's
//! other members, which are either machine-bound (`mcpOAuth` and friends) or
//! device-bound (`trustedDeviceToken`) and are nobody else's business.
//!
//! Verified against Claude Code 2.1.278: given a credential with no refresh
//! token, Claude Code never tries to refresh it and ignores `expiresAt`; given a
//! revoked one, it fails the request with a 401 and keeps the file; and a
//! running session that hit that 401 picks up a new token written to the file
//! on its next message. That is what makes pushing tokens to peers sufficient.

use crate::fsx::{self, R};
use crate::json::Json;
use crate::paths;
use crate::store::Account;
use crate::timefmt;

pub const KIND: &str = "cswap-lease";
pub const SCHEMA_VERSION: i64 = 1;

/// The members of `claudeAiOauth` a peer receives. An allowlist, not a
/// denylist: a field Claude Code adds tomorrow might be the next refresh
/// secret, and the safe default for an unknown field is to keep it home.
const LEASED_OAUTH_KEYS: &[&str] = &[
    "accessToken",
    "expiresAt",
    "scopes",
    "subscriptionType",
    "rateLimitTier",
];

#[derive(Clone, Debug)]
pub struct Lease {
    pub email: String,
    pub uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub oauth_account: Json,
    /// The credential JSON exactly as it is written on the peer: a
    /// `claudeAiOauth` block holding an access token and no refresh token.
    pub credentials: String,
}

impl Lease {
    /// Build a lease from an owner's full credential. Fails on anything that is
    /// not an OAuth login — an API key has no lineage to protect, but it also
    /// has nothing to lease: it would be handing over the key itself.
    pub fn from_owner(account: &Account, credentials: &str, oauth_account: Json) -> R<Lease> {
        Ok(Lease {
            email: account.email.clone(),
            uuid: account.uuid.clone(),
            org_uuid: account.org_uuid.clone(),
            org_name: account.org_name.clone(),
            oauth_account,
            credentials: strip(credentials)?,
        })
    }

    pub fn access_token(&self) -> Option<String> {
        access_token(&self.credentials)
    }

    pub fn expires_at_ms(&self) -> Option<i64> {
        Json::parse(&self.credentials)
            .ok()?
            .get("claudeAiOauth")?
            .get_i64("expiresAt")
    }

    /// The store record this becomes on the peer.
    pub fn to_account(&self, num: i64, alias: Option<String>) -> Account {
        Account {
            num,
            email: self.email.clone(),
            uuid: self.uuid.clone(),
            org_uuid: self.org_uuid.clone(),
            org_name: self.org_name.clone(),
            alias,
        }
    }

    /// One line, no trailing newline.
    pub fn to_line(&self) -> String {
        let mut out = Json::obj();
        out.set("kind", Json::str(KIND));
        out.set("schemaVersion", Json::num(SCHEMA_VERSION));
        out.set(
            "issuedAt",
            Json::str(timefmt::utc_stamp(timefmt::now_unix())),
        );
        out.set("email", Json::str(&self.email));
        for (key, value) in [
            ("uuid", &self.uuid),
            ("organizationUuid", &self.org_uuid),
            ("organizationName", &self.org_name),
        ] {
            if let Some(value) = value {
                out.set(key, Json::str(value));
            }
        }
        out.set("oauthAccount", self.oauth_account.clone());
        // Stripped credentials always parse: `strip` built them.
        out.set(
            "credentials",
            Json::parse(&self.credentials).unwrap_or(Json::Null),
        );
        out.dump()
    }

    pub fn parse(line: &str) -> R<Lease> {
        let root = Json::parse(line.trim()).map_err(|e| format!("not a cswap lease: {e}"))?;
        match root.get_str("kind") {
            Some(KIND) => {}
            Some(other) => return Err(format!("not a cswap lease: kind is '{other}'")),
            None => return Err("not a cswap lease: no 'kind' field".into()),
        }
        let version = root.get_i64("schemaVersion").unwrap_or(SCHEMA_VERSION);
        if version > SCHEMA_VERSION {
            return Err(format!(
                "lease is schema version {version}; this cswap understands {SCHEMA_VERSION} \
                 — run `cswap upgrade`"
            ));
        }
        let email = root
            .get_str("email")
            .filter(|e| !e.trim().is_empty())
            .ok_or("lease has no 'email'")?
            .to_string();
        let oauth_account = match root.get("oauthAccount") {
            Some(block) if block.is_obj() => block.clone(),
            _ => return Err(format!("{email}: lease has no 'oauthAccount' block")),
        };
        let credentials = match root.get("credentials") {
            Some(c) if c.is_obj() => c.dump(),
            _ => return Err(format!("{email}: lease has no 'credentials' object")),
        };
        // Re-strip on the way in. An owner running an older or modified cswap
        // must not be able to plant a refresh token here: the moment one exists
        // on a peer, the peer's Claude Code will spend it and break the owner.
        let credentials = strip(&credentials).map_err(|e| format!("{email}: {e}"))?;
        let text = |key: &str| root.get_str(key).map(str::to_string);
        Ok(Lease {
            email,
            uuid: text("uuid"),
            org_uuid: text("organizationUuid"),
            org_name: text("organizationName"),
            oauth_account,
            credentials,
        })
    }
}

/// Reduce a credential to what a peer may hold: `claudeAiOauth`, allowlisted.
pub fn strip(credentials: &str) -> R<String> {
    let parsed =
        Json::parse(credentials).map_err(|_| "the credential is not an OAuth login".to_string())?;
    let oauth = parsed
        .get("claudeAiOauth")
        .filter(|o| o.is_obj())
        .ok_or("the credential has no claudeAiOauth block — only OAuth logins can be lent")?;
    if oauth.get_str("accessToken").is_none() {
        return Err("the credential has no access token".into());
    }
    let mut kept = Json::obj();
    for key in LEASED_OAUTH_KEYS {
        if let Some(value) = oauth.get(key) {
            kept.set(key, value.clone());
        }
    }
    let mut out = Json::obj();
    out.set("claudeAiOauth", kept);
    Ok(out.dump())
}

pub fn access_token(credentials: &str) -> Option<String> {
    Json::parse(credentials)
        .ok()?
        .get("claudeAiOauth")?
        .get_str("accessToken")
        .map(str::to_string)
}

/// Whether a credential can refresh itself — i.e. it is somebody's own login
/// rather than a borrowed one.
pub fn has_refresh_token(credentials: &str) -> bool {
    Json::parse(credentials)
        .ok()
        .and_then(|c| {
            c.get("claudeAiOauth")?
                .get_str("refreshToken")
                .map(|t| !t.is_empty())
        })
        .unwrap_or(false)
}

/// A slot on this machine that holds a borrowed login.
#[derive(Clone, Debug, PartialEq)]
pub struct Borrowed {
    pub num: i64,
    pub email: String,
    /// The owner's SSH destination, e.g. `me@desktop`. `None` for a lease that
    /// arrived on stdin: it is marked borrowed, but nothing can renew it.
    pub from: Option<String>,
    /// The `cswap` to run on the owner, for a machine where it is not on the
    /// non-interactive `PATH`.
    pub remote_bin: Option<String>,
}

impl Borrowed {
    pub fn source(&self) -> &str {
        self.from.as_deref().unwrap_or("stdin")
    }
}

/// `<backup-root>/cluster-cswap-rs.json`. A file of this port's own, like the
/// usage cache: `sequence.json` is claude-swap's, and a key it does not know is
/// a key it may drop on its next write.
pub struct Cluster {
    pub borrowed: Vec<Borrowed>,
}

impl Cluster {
    fn path() -> std::path::PathBuf {
        paths::backup_root().join("cluster-cswap-rs.json")
    }

    pub fn load() -> R<Cluster> {
        let Some(root) = fsx::read_json_file(&Cluster::path())? else {
            return Ok(Cluster {
                borrowed: Vec::new(),
            });
        };
        let mut borrowed = Vec::new();
        if let Some(Json::Obj(entries)) = root.get("borrowed") {
            for (key, value) in entries {
                let (Ok(num), Some(email)) = (key.parse::<i64>(), value.get_str("email")) else {
                    continue;
                };
                borrowed.push(Borrowed {
                    num,
                    email: email.to_string(),
                    from: value.get_str("from").map(str::to_string),
                    remote_bin: value.get_str("remoteBin").map(str::to_string),
                });
            }
        }
        borrowed.sort_by_key(|b| b.num);
        Ok(Cluster { borrowed })
    }

    pub fn save(&self) -> R<()> {
        let mut entries = Json::obj();
        for b in &self.borrowed {
            let mut entry = Json::obj();
            entry.set("email", Json::str(&b.email));
            if let Some(from) = &b.from {
                entry.set("from", Json::str(from));
            }
            if let Some(bin) = &b.remote_bin {
                entry.set("remoteBin", Json::str(bin));
            }
            entries.set(&b.num.to_string(), entry);
        }
        let mut root = Json::obj();
        root.set("schemaVersion", Json::num(1));
        root.set("borrowed", entries);
        fsx::write_atomic(&Cluster::path(), &root.dump_pretty())
    }

    /// The borrow record for a slot, if the slot still holds the account it was
    /// borrowed as. Slots are keyed by number *and* email, as the store's own
    /// files are: a slot since reused for someone else is not borrowed.
    pub fn get(&self, account: &Account) -> Option<&Borrowed> {
        self.borrowed
            .iter()
            .find(|b| b.num == account.num && b.email.eq_ignore_ascii_case(&account.email))
    }

    pub fn upsert(&mut self, record: Borrowed) {
        self.borrowed.retain(|b| b.num != record.num);
        self.borrowed.push(record);
        self.borrowed.sort_by_key(|b| b.num);
    }

    /// Forget a slot's borrow record. Returns whether there was one.
    pub fn forget(&mut self, num: i64) -> bool {
        let before = self.borrowed.len();
        self.borrowed.retain(|b| b.num != num);
        before != self.borrowed.len()
    }
}

/// Drop a slot's borrow record, if it has one. For the paths that put a real
/// login into a slot — `add`, `import` — after which it is owned, not borrowed.
pub fn forget_slot(num: i64) -> R<()> {
    let mut cluster = Cluster::load()?;
    if cluster.forget(num) {
        cluster.save()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = r#"{"claudeAiOauth":{"accessToken":"at-1","refreshToken":"rt-1","expiresAt":1790058662077,"refreshTokenExpiresAt":1792330842077,"scopes":["user:inference"],"subscriptionType":"pro","rateLimitTier":"default_claude_ai","somethingNew":"secret"},"mcpOAuth":{"s":{"accessToken":"mcp"}},"trustedDeviceToken":"device"}"#;

    fn account() -> Account {
        Account {
            num: 2,
            email: "a@x.com".into(),
            uuid: Some("u-1".into()),
            org_uuid: Some("o-1".into()),
            org_name: Some("Acme".into()),
            alias: Some("work".into()),
        }
    }

    fn oauth_account() -> Json {
        Json::parse(r#"{"emailAddress":"a@x.com","accountUuid":"u-1"}"#).unwrap()
    }

    #[test]
    fn a_lease_carries_the_access_token_and_nothing_that_can_refresh() {
        let lease = Lease::from_owner(&account(), OWNER, oauth_account()).unwrap();
        let creds = Json::parse(&lease.credentials).unwrap();
        let oauth = creds.get("claudeAiOauth").unwrap();
        assert_eq!(oauth.get_str("accessToken"), Some("at-1"));
        // The expiry survives byte-exactly; Claude Code rejects a float.
        assert_eq!(lease.expires_at_ms(), Some(1790058662077));
        assert!(lease.credentials.contains("1790058662077"));
        for gone in ["refreshToken", "refreshTokenExpiresAt", "somethingNew"] {
            assert!(oauth.get(gone).is_none(), "{gone} must not travel");
        }
        assert!(creds.get("mcpOAuth").is_none());
        assert!(creds.get("trustedDeviceToken").is_none());
        assert!(!has_refresh_token(&lease.credentials));
        assert!(has_refresh_token(OWNER));
    }

    #[test]
    fn a_lease_round_trips_through_one_line() {
        let lease = Lease::from_owner(&account(), OWNER, oauth_account()).unwrap();
        let line = lease.to_line();
        assert!(!line.contains('\n'));
        let back = Lease::parse(&line).unwrap();
        assert_eq!(back.email, "a@x.com");
        assert_eq!(back.org_name.as_deref(), Some("Acme"));
        assert_eq!(back.credentials, lease.credentials);
        assert_eq!(back.access_token().as_deref(), Some("at-1"));
    }

    #[test]
    fn a_refresh_token_planted_in_a_lease_is_stripped_on_arrival() {
        let line = format!(
            r#"{{"kind":"cswap-lease","email":"a@x.com","oauthAccount":{{}},"credentials":{OWNER}}}"#
        );
        let lease = Lease::parse(&line).unwrap();
        assert!(!has_refresh_token(&lease.credentials));
        assert!(!lease.credentials.contains("rt-1"));
    }

    #[test]
    fn only_oauth_logins_can_be_lent() {
        assert!(Lease::from_owner(&account(), "sk-ant-api03-xxx", oauth_account()).is_err());
        assert!(Lease::from_owner(&account(), r#"{"mcpOAuth":{}}"#, oauth_account()).is_err());
        assert!(Lease::from_owner(
            &account(),
            r#"{"claudeAiOauth":{"refreshToken":"r"}}"#,
            oauth_account()
        )
        .is_err());
    }

    #[test]
    fn rejects_lines_that_are_not_leases() {
        for case in [
            "",
            "not json",
            r#"{"kind":"cswap-export","accounts":[]}"#,
            r#"{"kind":"cswap-lease","oauthAccount":{},"credentials":{}}"#,
            r#"{"kind":"cswap-lease","email":"a@x.com","credentials":{"claudeAiOauth":{"accessToken":"t"}}}"#,
            r#"{"kind":"cswap-lease","email":"a@x.com","oauthAccount":{},"credentials":"t"}"#,
            r#"{"kind":"cswap-lease","schemaVersion":99,"email":"a@x.com","oauthAccount":{},"credentials":{}}"#,
        ] {
            assert!(Lease::parse(case).is_err(), "should reject {case}");
        }
    }

    #[test]
    fn a_borrow_record_belongs_to_a_slot_and_an_email() {
        let mut cluster = Cluster {
            borrowed: Vec::new(),
        };
        cluster.upsert(Borrowed {
            num: 2,
            email: "A@x.com".into(),
            from: Some("me@desk".into()),
            remote_bin: None,
        });
        assert!(cluster.get(&account()).is_some());
        let mut reused = account();
        reused.email = "someone-else@x.com".into();
        assert!(cluster.get(&reused).is_none());
        assert!(cluster.forget(2));
        assert!(!cluster.forget(2));
    }
}
