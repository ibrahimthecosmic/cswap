//! The five commands.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::fsx::R;
use crate::json::Json;
use crate::lock::{DirLock, FileLock};
use crate::model::Row;
use crate::render;
use crate::store::{self, Account, Store};
use crate::{live, Flags};

const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn list(flags: &Flags) -> R<ExitCode> {
    let store_ref = Store::load()?;
    let accounts = store_ref.accounts();
    if accounts.is_empty() {
        if flags.json {
            println!("{}", render::json_list(&store_ref, &[]).dump_pretty());
        } else {
            println!("No accounts yet. Log into Claude Code, then run `cswap add`.");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let rows = rows_for(&store_ref, flags);
    if flags.json {
        println!("{}", render::json_list(&store_ref, &rows).dump_pretty());
    } else {
        render::human_list(&store_ref, &rows);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn status(flags: &Flags) -> R<ExitCode> {
    let store_ref = Store::load()?;
    let identity = live::active_identity()?;
    let email = identity
        .as_ref()
        .and_then(|o| o.get_str("emailAddress"))
        .map(str::to_string);
    let managed = email.as_deref().and_then(|e| {
        store_ref
            .accounts()
            .into_iter()
            .find(|a| a.email.eq_ignore_ascii_case(e))
    });

    if flags.json {
        let mut out = Json::obj();
        out.set("schemaVersion", Json::num(1));
        out.set(
            "activeAccountNumber",
            managed.as_ref().map_or(Json::Null, |a| Json::num(a.num)),
        );
        out.set("email", email.clone().map_or(Json::Null, Json::str));
        out.set("managed", Json::Bool(managed.is_some()));
        println!("{}", out.dump_pretty());
        return Ok(ExitCode::SUCCESS);
    }

    match (&email, &managed) {
        (Some(email), Some(account)) => {
            println!("Account {}  {email}", account.num);
            if let Some(org) = &account.org_name {
                println!("  organization  {org}");
            }
        }
        (Some(email), None) => {
            println!("{email}  (not managed by cswap — run `cswap add` to store it)");
        }
        (None, _) => println!("Claude Code is not logged in."),
    }
    Ok(ExitCode::SUCCESS)
}

pub fn switch(selector: Option<&str>, flags: &Flags) -> R<ExitCode> {
    let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
    let mut store_ref = Store::load()?;
    let accounts = store_ref.accounts();
    if accounts.is_empty() {
        return Err("no accounts stored — run `cswap add` first".into());
    }

    let from = store_ref.active();
    let to = match selector {
        Some(sel) => store_ref.resolve(sel)?,
        None => store_ref
            .next_after(from)
            .ok_or("only one account is stored; there is nowhere to switch to")?,
    };

    if Some(to) == from && !flags.force {
        let account = store_ref.get(to).expect("resolved account exists");
        if flags.json {
            println!(
                "{}",
                render::json_switch(false, from, to, "already active").dump_pretty()
            );
        } else {
            println!("Already on account {} ({}).", to, account.label());
        }
        return Ok(ExitCode::SUCCESS);
    }

    let target = store_ref.get(to).expect("resolved account exists");
    let target_credentials = store::read_backup(target.num, &target.email)?.ok_or_else(|| {
        format!(
            "account {} has no stored login — log in as it and run `cswap add --slot {}`",
            target.num, target.num
        )
    })?;
    let target_config = store::read_config(target.num, &target.email)?
        .ok_or_else(|| format!("account {} has no stored config snapshot", target.num))?;
    let oauth_account = target_config
        .get("oauthAccount")
        .ok_or_else(|| {
            format!(
                "account {}'s stored config has no oauthAccount block",
                target.num
            )
        })?
        .clone();

    // Snapshot what is live now, so it can be filed under the outgoing account
    // and restored if the switch comes apart halfway.
    let original_credentials = live::read_credentials()?;
    if let (Some(from), Some(credentials)) = (from, original_credentials.as_deref()) {
        if let Some(outgoing) = store_ref.get(from) {
            back_up(&outgoing, credentials)?;
        }
    }

    {
        let _credential_locks = DirLock::credentials()?;
        let prepared =
            live::prepare_for_activation(&target_credentials, original_credentials.as_deref());
        live::write_credentials(&prepared)?;
    }

    {
        let _config_lock = DirLock::config()?;
        if let Err(e) = live::set_oauth_account(&oauth_account) {
            // The credential is already swapped but the config still names the
            // old account, which Claude Code would show as a mismatched login.
            // Put the previous credential back rather than leave that.
            if let Some(original) = original_credentials.as_deref() {
                let _locks = DirLock::credentials();
                let _ = live::write_credentials(original);
            }
            return Err(format!(
                "could not update {}: {e}",
                crate::paths::global_config_path().display()
            ));
        }
    }

    store_ref.set_active(to);
    store_ref.save()?;

    if flags.json {
        println!(
            "{}",
            render::json_switch(true, from, to, "switched").dump_pretty()
        );
    } else {
        println!("Switched to account {} ({}).", target.num, target.label());
        println!("{}", restart_note());
    }
    Ok(ExitCode::SUCCESS)
}

pub fn add(flags: &Flags) -> R<ExitCode> {
    let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
    let mut store_ref = Store::load()?;

    let credentials = live::read_credentials()?
        .ok_or("Claude Code is not logged in — run `claude` and log in first")?;
    let config = live::read_global_config()?.ok_or_else(|| {
        format!(
            "{} does not exist",
            crate::paths::global_config_path().display()
        )
    })?;
    let oauth_account = config
        .get("oauthAccount")
        .ok_or("the live Claude Code config has no oauthAccount block — log in first")?
        .clone();
    let email = oauth_account
        .get_str("emailAddress")
        .ok_or("the live login has no email address")?
        .to_string();
    let uuid = oauth_account.get_str("accountUuid").map(str::to_string);

    // Re-adding an account that is already stored updates it in place — that is
    // how an expired login is refreshed — so match on identity before falling
    // back to a new slot.
    let existing = store_ref
        .accounts()
        .into_iter()
        .find(|a| a.email.eq_ignore_ascii_case(&email) || (a.uuid.is_some() && a.uuid == uuid));
    let num = match (flags.slot, &existing) {
        (Some(slot), _) => slot,
        (None, Some(account)) => account.num,
        (None, None) => store_ref.free_slot(),
    };

    if let Some(occupant) = store_ref.get(num) {
        if !occupant.email.eq_ignore_ascii_case(&email) && !flags.force {
            return Err(format!(
                "slot {num} already holds {} — pass --force to overwrite it",
                occupant.email
            ));
        }
    }

    let account = Account {
        num,
        email: email.clone(),
        uuid,
        org_uuid: oauth_account
            .get_str("organizationUuid")
            .map(str::to_string),
        org_name: oauth_account
            .get_str("organizationName")
            .map(str::to_string),
        alias: flags.alias.clone(),
    };

    store::write_backup(account.num, &account.email, &credentials)?;
    store::write_config(account.num, &account.email, &config)?;
    store_ref.register(&account);
    if let Some(alias) = &flags.alias {
        set_alias(&mut store_ref, num, alias);
    }
    store_ref.set_active(num);
    store_ref.save()?;

    let verb = if existing.is_some() {
        "Updated"
    } else {
        "Added"
    };
    println!("{verb} account {num} ({email}).");
    Ok(ExitCode::SUCCESS)
}

pub fn remove(selector: &str, flags: &Flags) -> R<ExitCode> {
    let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
    let mut store_ref = Store::load()?;
    let num = store_ref.resolve(selector)?;
    let account = store_ref.get(num).expect("resolved account exists");

    if !flags.yes {
        if !std::io::stdin().is_terminal() {
            return Err(
                "refusing to remove an account without --yes when input is not a terminal".into(),
            );
        }
        print!(
            "Remove account {} ({})? Its stored login is deleted. [y/N] ",
            account.num, account.email
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Left alone.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let was_active = store_ref.active() == Some(num);
    store::remove_backup(account.num, &account.email);
    store_ref.unregister(num);
    store_ref.save()?;

    println!("Removed account {} ({}).", account.num, account.email);
    if was_active {
        println!(
            "It was the active account; Claude Code is still logged in as it until you switch."
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(feature = "usage")]
pub fn upgrade(flags: &Flags) -> R<ExitCode> {
    crate::upgrade::run(flags.check)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(feature = "usage"))]
pub fn upgrade(_flags: &Flags) -> R<ExitCode> {
    Err("this build has no network support — upgrade by rebuilding from source".into())
}

fn back_up(account: &Account, credentials: &str) -> R<()> {
    store::write_backup(account.num, &account.email, credentials)?;
    if let Some(config) = live::read_global_config()? {
        // Only file the config under this account if it still names it; a config
        // that has drifted to another identity is not this slot's snapshot.
        let names_it = config
            .get("oauthAccount")
            .and_then(|o| o.get_str("emailAddress"))
            .is_some_and(|e| e.eq_ignore_ascii_case(&account.email));
        if names_it {
            store::write_config(account.num, &account.email, &config)?;
        }
    }
    Ok(())
}

fn set_alias(store_ref: &mut Store, num: i64, alias: &str) {
    if let Some(accounts) = store_ref.data.get_mut("accounts") {
        if let Some(record) = accounts.get_mut(&num.to_string()) {
            record.set("alias", Json::str(alias));
        }
    }
}

fn restart_note() -> &'static str {
    if cfg!(target_os = "macos") {
        "Claude Code picks this up once its ~30s Keychain cache expires; restart it to apply now."
    } else {
        "Claude Code picks this up on your next message — no restart needed."
    }
}

#[cfg(feature = "usage")]
fn rows_for(store_ref: &Store, flags: &Flags) -> Vec<Row> {
    if flags.no_usage {
        return plain_rows(store_ref);
    }
    crate::usage::collect(store_ref, flags.refresh, flags.offline)
}

#[cfg(not(feature = "usage"))]
fn rows_for(store_ref: &Store, _flags: &Flags) -> Vec<Row> {
    plain_rows(store_ref)
}

fn plain_rows(store_ref: &Store) -> Vec<Row> {
    let active = store_ref.active();
    store_ref
        .accounts()
        .into_iter()
        .map(|account| {
            let is_active = Some(account.num) == active;
            Row::plain(account, is_active)
        })
        .collect()
}
