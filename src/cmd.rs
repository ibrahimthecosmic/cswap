//! The commands.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::fsx::{self, R};
use crate::json::Json;
use crate::lock::{DirLock, FileLock};
use crate::model::Row;
use crate::portable::{self, Portable};
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

/// Write one account, or every account, to a portable JSON file.
///
/// The file it writes holds live OAuth tokens in the clear — the same bytes the
/// store already keeps, so this is not a new exposure, but it is one that leaves
/// the directory the store protects. Hence 0600, a refusal to clobber an
/// existing file without `--force`, and a warning on stderr.
pub fn export(selector: Option<&str>, flags: &Flags) -> R<ExitCode> {
    let destination = flags
        .output
        .as_deref()
        .ok_or("export needs --output <file> (use `--output -` for stdout)")?;

    let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
    let store_ref = Store::load()?;
    let selected = match selector {
        Some(sel) => vec![store_ref
            .get(store_ref.resolve(sel)?)
            .expect("resolved account exists")],
        None => store_ref.accounts(),
    };
    if selected.is_empty() {
        return Err("no accounts stored — run `cswap add` first".into());
    }

    // The account Claude Code is logged in as right now has a credential that
    // moves without the store hearing about it: Claude Code refreshes in place,
    // and only `add` and `switch` file the result. Its backup is therefore a
    // past generation, and a refresh token from a past generation is dead the
    // moment the live one rotates — so exporting the backup would ship a
    // credential that fails on the other machine while this one keeps working.
    // Re-file the live state under its slot first, exactly as `switch` does.
    let live_email =
        live::active_identity()?.and_then(|o| o.get_str("emailAddress").map(str::to_string));
    let live_credentials = live::read_credentials()?;

    let mut items = Vec::new();
    for account in &selected {
        let live = live_email
            .as_deref()
            .filter(|e| e.eq_ignore_ascii_case(&account.email))
            .and(live_credentials.as_deref());
        match portable_for(account, live) {
            Ok(item) => items.push(item),
            // Asking for one account and not getting it is an error; sweeping up
            // all of them steps over the ones that are not exportable, which is
            // the only way a store with one broken slot can be backed up at all.
            Err(e) if selector.is_none() => eprintln!("cswap: skipping {e}"),
            Err(e) => return Err(e),
        }
    }
    if items.is_empty() {
        return Err("no stored account has a login to export".into());
    }

    let text = portable::envelope(&items).dump_pretty();
    if destination == "-" {
        println!("{text}");
        return Ok(ExitCode::SUCCESS);
    }

    let path = std::path::Path::new(destination);
    if path.exists() && !flags.force {
        return Err(format!(
            "{destination} already exists — pass --force to overwrite it"
        ));
    }
    fsx::write_atomic(path, &text)?;
    eprintln!(
        "cswap: warning: {destination} holds live login tokens in plaintext. \
         Keep it off shared storage and delete it once imported."
    );

    if flags.json {
        let mut out = Json::obj();
        out.set("schemaVersion", Json::num(1));
        out.set("output", Json::str(destination));
        out.set(
            "accounts",
            Json::Arr(
                items
                    .iter()
                    .map(|item| {
                        let mut entry = Json::obj();
                        entry.set("slot", item.slot.map_or(Json::Null, Json::num));
                        entry.set("email", Json::str(&item.email));
                        entry
                    })
                    .collect(),
            ),
        );
        println!("{}", out.dump_pretty());
    } else {
        println!(
            "Exported {} to {destination}.",
            plural(items.len(), "account")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Read accounts back out of a file written by `export`.
///
/// Unlike `add`, this does not touch the active marker: importing a login is not
/// logging into it. `cswap switch` still does that, and only that.
pub fn import(source: &str, flags: &Flags) -> R<ExitCode> {
    let text = read_source(source)?;
    let items = portable::parse(&text)?;
    if items.len() > 1 {
        if flags.slot.is_some() {
            return Err(format!(
                "--slot takes a single account, but {source} holds {}",
                items.len()
            ));
        }
        if flags.alias.is_some() {
            return Err(format!(
                "--alias takes a single account, but {source} holds {}",
                items.len()
            ));
        }
    }

    let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
    let mut store_ref = Store::load()?;
    let mut imported: Vec<(i64, String, bool)> = Vec::new();

    for item in &items {
        // Matching on identity first means re-importing an account updates the
        // slot it already has — that is how a refreshed export replaces a login
        // that expired here.
        let existing = store_ref.accounts().into_iter().find(|a| {
            a.email.eq_ignore_ascii_case(&item.email) || (a.uuid.is_some() && a.uuid == item.uuid)
        });
        let num = match (flags.slot, &existing) {
            (Some(slot), _) => slot,
            (None, Some(account)) => account.num,
            (None, None) => store_ref.free_slot(),
        };

        if let Some(occupant) = store_ref.get(num) {
            if !occupant.email.eq_ignore_ascii_case(&item.email) {
                if !flags.force {
                    return Err(format!(
                        "slot {num} already holds {} — pass --force to overwrite it",
                        occupant.email
                    ));
                }
                // The store keys its files by slot *and* email, so overwriting
                // with a different identity would otherwise strand the old
                // account's credential on disk under nobody's name.
                store::remove_backup(occupant.num, &occupant.email);
            }
        }

        // The same account arriving at a different slot is a move, not a copy:
        // two slots claiming one login make `switch <email>` ambiguous.
        let mut keep_active = false;
        if let Some(previous) = existing.as_ref().filter(|a| a.num != num) {
            if !flags.force {
                return Err(format!(
                    "{} is already stored in slot {} — pass --force to move it to slot {num}",
                    item.email, previous.num
                ));
            }
            keep_active = store_ref.active() == Some(previous.num);
            store::remove_backup(previous.num, &previous.email);
            store_ref.unregister(previous.num);
        }

        let alias = if items.len() == 1 {
            flags.alias.clone()
        } else {
            None
        };
        let account = item.to_account(num, alias);

        store::write_backup(num, &account.email, &item.credentials)?;
        // `switch` reads exactly one key out of the stored snapshot, so a slot
        // that has none only needs that key. One that already has a full
        // snapshot keeps it — the import is a new login for the same account,
        // not a reason to drop its config.
        let mut config = match store::read_config(num, &account.email)? {
            Some(existing_config) if existing_config.is_obj() => existing_config,
            _ => Json::obj(),
        };
        config.set("oauthAccount", item.oauth_account.clone());
        store::write_config(num, &account.email, &config)?;

        store_ref.register(&account);
        if let Some(alias) = &account.alias {
            set_alias(&mut store_ref, num, alias);
        }
        if keep_active {
            store_ref.set_active(num);
        }
        imported.push((num, account.email.clone(), existing.is_some()));
    }
    let active = store_ref.active();
    store_ref.save()?;

    if flags.json {
        let mut out = Json::obj();
        out.set("schemaVersion", Json::num(1));
        out.set(
            "imported",
            Json::Arr(
                imported
                    .iter()
                    .map(|(num, email, updated)| {
                        let mut entry = Json::obj();
                        entry.set("slot", Json::num(*num));
                        entry.set("email", Json::str(email));
                        entry.set("updated", Json::Bool(*updated));
                        entry
                    })
                    .collect(),
            ),
        );
        println!("{}", out.dump_pretty());
        return Ok(ExitCode::SUCCESS);
    }

    println!("Imported {}:", plural(imported.len(), "account"));
    for (num, email, updated) in &imported {
        let verb = if *updated { "updated" } else { "new" };
        println!("  {num}  {email}  ({verb})");
    }
    // Importing a login is not logging into it, so say so — unless one of these
    // slots is the account already in use.
    if !imported.iter().any(|(num, _, _)| Some(*num) == active) {
        if let Some((num, _, _)) = imported.first() {
            println!("Nothing is logged in as these yet — run `cswap switch {num}` to use one.");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Gather one account for export. `live` is the credential Claude Code is using
/// right now, and is passed only when it belongs to *this* account: it is then
/// filed under the slot before anything is read back, so what travels is the
/// generation in use rather than whatever the store last saw.
fn portable_for(account: &Account, live: Option<&str>) -> R<Portable> {
    if let Some(live) = live {
        back_up(account, live).map_err(|e| {
            format!(
                "account {} ({}): could not file its live login: {e}",
                account.num, account.email
            )
        })?;
    }
    let credentials = store::read_backup(account.num, &account.email)?.ok_or_else(|| {
        format!(
            "account {} ({}): no stored login — log in as it and run `cswap add --slot {}`",
            account.num, account.email, account.num
        )
    })?;
    let config = store::read_config(account.num, &account.email)?.ok_or_else(|| {
        format!(
            "account {} ({}): no stored config snapshot",
            account.num, account.email
        )
    })?;
    let oauth_account = config
        .get("oauthAccount")
        .ok_or_else(|| {
            format!(
                "account {} ({}): its stored config has no oauthAccount block",
                account.num, account.email
            )
        })?
        .clone();
    Ok(Portable::from_account(account, credentials, oauth_account))
}

fn read_source(source: &str) -> R<String> {
    if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("stdin: {e}"))?;
        return Ok(text);
    }
    std::fs::read_to_string(source).map_err(|e| format!("{source}: {e}"))
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
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
