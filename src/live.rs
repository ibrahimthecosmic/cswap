//! Claude Code's *live* login: the credential it is using right now, and the
//! `oauthAccount` block in `~/.claude.json` that names who that is.
//!
//! Two backends. On Linux and Windows the credential is a plaintext file,
//! `<config-home>/.credentials.json`, which Claude Code re-reads whenever it
//! changes — so a switch takes effect on the next message with no restart. On
//! macOS it lives in the login Keychain, reached through the `security` CLI, and
//! Claude Code caches its reads for ~30s, which is why a switch there appears to
//! take a moment to land.

#[cfg(target_os = "macos")]
use std::process::Command;

use crate::fsx::{self, R};
use crate::json::Json;
use crate::paths;

/// Service name of Claude Code's active OAuth credential in the macOS Keychain.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Siblings of `claudeAiOauth` that belong to the *machine*, not to an account.
/// These hold OAuth integrations that rotate independently of any slot, so on
/// activation the live copy wins — including by being absent. Everything else,
/// recognised (`trustedDeviceToken` is enrolled per account at login) or not,
/// travels with the slot: restoring a stale shared field costs one re-auth
/// prompt, while carrying a live account-bound field across a switch would
/// present one account's credential under another.
const SHARED_CREDENTIAL_KEYS: &[&str] = &[
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];

/// Read the credential Claude Code is currently using. `Ok(None)` means there is
/// none (logged out); an unreadable one is an error.
pub fn read_credentials() -> R<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        match keychain_get(KEYCHAIN_SERVICE, &keychain_account())? {
            Some(v) => return Ok(Some(v)),
            // Fall through: a Keychain miss is a genuine "no item", and some
            // installs (headless, or after a Keychain failure) keep the
            // plaintext file instead.
            None => {}
        }
    }
    match std::fs::read_to_string(paths::credentials_path()) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", paths::credentials_path().display())),
    }
}

/// Write the credential Claude Code will use next. Callers must hold the
/// credentials lock (see [`crate::lock::DirLock::credentials`]).
pub fn write_credentials(credentials: &str) -> R<()> {
    #[cfg(target_os = "macos")]
    {
        if keychain_set(KEYCHAIN_SERVICE, &keychain_account(), credentials).is_ok() {
            // Claude Code reads the Keychain before the file, but a stale file
            // left behind is a loaded gun for the fallback path — keep them in
            // step rather than letting them disagree.
            let _ = fsx::write_atomic(&paths::credentials_path(), credentials);
            return Ok(());
        }
        eprintln!("cswap: warning: Keychain write failed, falling back to the credentials file");
    }
    fsx::write_atomic(&paths::credentials_path(), credentials)
}

/// Compose the credential to activate from its two owners: machine-shared keys
/// from the live credential, everything else from the slot's stored one.
pub fn prepare_for_activation(target: &str, live: Option<&str>) -> String {
    let (Some(live), Ok(mut target_obj)) = (live, Json::parse(target)) else {
        return target.to_string();
    };
    let Ok(live_obj) = Json::parse(live) else {
        return target.to_string();
    };
    if !target_obj.is_obj() || !live_obj.is_obj() {
        return target.to_string();
    }
    for key in SHARED_CREDENTIAL_KEYS {
        match live_obj.get(key) {
            Some(value) => target_obj.set(key, value.clone()),
            None => {
                target_obj.remove(key);
            }
        }
    }
    target_obj.dump()
}

/// `~/.claude.json`. `Ok(None)` means the file does not exist; a file that
/// exists but will not parse is an error — this is 80 KB of the user's Claude
/// Code state, and overwriting an unreadable one loses their projects, MCP
/// servers and settings.
pub fn read_global_config() -> R<Option<Json>> {
    fsx::read_json_file(&paths::global_config_path())
}

/// Replace only `oauthAccount`, preserving every other key and its position.
pub fn set_oauth_account(oauth_account: &Json) -> R<()> {
    let path = paths::global_config_path();
    let mut config = match read_global_config() {
        Ok(Some(existing)) if existing.is_obj() => existing,
        Ok(Some(_)) | Err(_) if path.exists() => {
            return Err(format!(
                "{} exists but could not be read — refusing to overwrite it. \
                 Move or repair the file, then retry.",
                path.display()
            ))
        }
        // Genuinely absent: a fresh machine has nothing to preserve.
        _ => Json::obj(),
    };
    config.set("oauthAccount", oauth_account.clone());
    fsx::write_atomic(&path, &config.dump_pretty())
}

/// The identity Claude Code currently shows as logged in.
pub fn active_identity() -> R<Option<Json>> {
    Ok(read_global_config()?.and_then(|c| c.get("oauthAccount").cloned()))
}

#[cfg(target_os = "macos")]
fn keychain_account() -> String {
    // Mirrors Claude Code's getUsername(): $USER first, then a stable fallback.
    // Diverging here would key a *different* Keychain item than Claude Code, so
    // the two would not see each other's credential.
    std::env::var("USER").unwrap_or_else(|_| "claude-code-user".to_string())
}

#[cfg(target_os = "macos")]
fn keychain_get(service: &str, account: &str) -> R<Option<String>> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-a", account, "-w", "-s", service])
        .output()
        .map_err(|e| format!("security find-generic-password: {e}"))?;
    match out.status.code() {
        Some(0) => {
            let value = String::from_utf8_lossy(&out.stdout)
                .trim_end_matches('\n')
                .to_string();
            Ok(if value.is_empty() { None } else { Some(value) })
        }
        // 44 is errSecItemNotFound — a real "no such item", not a failure.
        Some(44) => Ok(None),
        _ => Err(format!(
            "security find-generic-password failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

#[cfg(target_os = "macos")]
fn keychain_set(service: &str, account: &str, value: &str) -> R<()> {
    use std::io::Write;
    use std::process::Stdio;

    // `security -i` reads stdin with a 4096-byte fgets() buffer, so a large
    // credential (several MCP OAuth logins will do it) would be silently
    // truncated mid-line. Past that, fall back to argv — which publishes the
    // secret to `ps` for the life of the call, but a truncated credential is a
    // lost account.
    let escaped_len = value.len() * 2 + account.len() + service.len() + 64;
    if escaped_len >= 4000 {
        return keychain_set_via_argv(service, account, value);
    }

    // `security -i` re-parses each stdin line shell-style, so the value is
    // double-quoted. Passing it on argv instead would publish the credential to
    // every `ps` on the machine.
    let mut child = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("security -i: {e}"))?;
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    let line =
        format!("add-generic-password -U -a \"{account}\" -s \"{service}\" -w \"{escaped}\"\n");
    child
        .stdin
        .take()
        .ok_or("security -i: no stdin")?
        .write_all(line.as_bytes())
        .map_err(|e| format!("security -i: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("security -i: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "security add-generic-password failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: &str = r#"{"claudeAiOauth":{"accessToken":"slot"},"trustedDeviceToken":"device-A","mcpOAuth":{"s":"stale"}}"#;
    const LIVE: &str = r#"{"claudeAiOauth":{"accessToken":"live"},"trustedDeviceToken":"device-B","mcpOAuth":{"s":"current"},"pluginSecrets":{"p":1}}"#;

    #[test]
    fn machine_shared_keys_come_from_the_live_credential() {
        let merged = Json::parse(&prepare_for_activation(SLOT, Some(LIVE))).unwrap();
        // The slot owns the login itself and its per-account device enrolment.
        assert_eq!(
            merged.get("claudeAiOauth").unwrap().get_str("accessToken"),
            Some("slot")
        );
        assert_eq!(merged.get_str("trustedDeviceToken"), Some("device-A"));
        // The machine owns the MCP/plugin OAuth state, including keys the slot
        // never stored.
        assert_eq!(
            merged.get("mcpOAuth").unwrap().get_str("s"),
            Some("current")
        );
        assert!(merged.get("pluginSecrets").is_some());
    }

    #[test]
    fn a_shared_key_absent_from_the_live_credential_is_dropped() {
        // Absence is authoritative: restoring a slot's frozen copy would
        // resurrect an MCP login the machine has since revoked.
        let live = r#"{"claudeAiOauth":{"accessToken":"live"}}"#;
        let merged = Json::parse(&prepare_for_activation(SLOT, Some(live))).unwrap();
        assert!(merged.get("mcpOAuth").is_none());
        assert_eq!(merged.get_str("trustedDeviceToken"), Some("device-A"));
    }

    #[test]
    fn a_non_json_live_credential_activates_the_slot_unchanged() {
        // An API key is live but is not a credential object to merge from.
        assert_eq!(prepare_for_activation(SLOT, Some("sk-ant-api03-xxx")), SLOT);
        assert_eq!(prepare_for_activation(SLOT, None), SLOT);
    }
}

#[cfg(target_os = "macos")]
fn keychain_set_via_argv(service: &str, account: &str, value: &str) -> R<()> {
    let out = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            account,
            "-s",
            service,
            "-w",
            value,
        ])
        .output()
        .map_err(|e| format!("security add-generic-password: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "security add-generic-password failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
