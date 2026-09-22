//! Using one login from several machines: `token` on the machine that owns it,
//! `borrow` and `lease` on the machines that use it.
//!
//! The owner is whichever machine did the browser login. It keeps the refresh
//! token and is the only machine that ever refreshes; see `lease.rs` for why
//! that is not a preference but the whole design. Peers hold an access token
//! and nothing else.
//!
//! The transport is SSH, run as a subprocess: the owner needs no daemon and no
//! open port, host trust and authentication are whatever the user already has,
//! and the only thing that crosses the wire is `cswap token`'s stdout. A
//! refresh on the owner revokes the previous access token *immediately*, so
//! `cswap lease` keeps one `ssh owner cswap token <account> --watch` open per
//! borrowed account and the owner pushes each new token down it within seconds.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::fsx::R;
use crate::lease::{self, Borrowed, Cluster, Lease};
use crate::lock::{DirLock, FileLock};
use crate::store::{self, Account, Store};
use crate::{live, timefmt, Flags};

/// How far ahead of expiry the owner refreshes. The margin buys retries if the
/// token endpoint is having a bad minute; it does not buy peers anything,
/// since a refresh revokes their token whenever it happens.
#[cfg_attr(not(feature = "usage"), allow(dead_code))]
const REFRESH_AHEAD: i64 = 300;
/// How often `token --watch` looks for a new generation. Claude Code on the
/// owner refreshes the live account without telling anyone; this is the upper
/// bound on how long a peer sits on a revoked token afterwards.
const POLL: Duration = Duration::from_secs(3);
/// `token --watch` re-sends an unchanged lease this often. It is the peer's
/// proof the owner is alive, and it heals a peer whose Claude Code wrote an
/// older token back over the one it was given.
const HEARTBEAT: i64 = 60;
/// After a failed refresh, wait this long before the next attempt rather than
/// hitting the token endpoint every poll.
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(30);
/// A one-shot fetch (`borrow`, `switch`) gives up after this.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- owner side

/// `cswap token <account> [--watch]`: print a lease for an account this machine
/// owns. Meant to be run over SSH by a peer, not by hand — its output is a live
/// access token.
pub fn token(selector: &str, flags: &Flags) -> R<ExitCode> {
    let store_ref = Store::load()?;
    let account = store_ref
        .get(store_ref.resolve(selector)?)
        .expect("resolved account exists");
    if let Some(record) = Cluster::load()?.get(&account) {
        return Err(format!(
            "account {} is itself borrowed from {} — only the owner can lend it",
            account.num,
            record.source()
        ));
    }

    let mut out = std::io::stdout();
    if !flags.watch {
        let lease = issue(&account)?;
        writeln!(out, "{}", lease.to_line()).map_err(|e| format!("stdout: {e}"))?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut last_token: Option<String> = None;
    let mut last_sent = 0;
    loop {
        match issue(&account) {
            Ok(lease) => {
                let token = lease.access_token();
                let now = timefmt::now_unix();
                if token != last_token || now - last_sent >= HEARTBEAT {
                    // A write that fails means the peer hung up. That is the
                    // normal way a watch ends, not an error.
                    if writeln!(out, "{}", lease.to_line())
                        .and_then(|()| out.flush())
                        .is_err()
                    {
                        return Ok(ExitCode::SUCCESS);
                    }
                    last_token = token;
                    last_sent = now;
                }
                thread::sleep(POLL);
            }
            Err(e) => {
                // stderr reaches the peer through SSH, which prints it: the
                // person there is the one who needs to know.
                eprintln!("cswap token: {e}");
                thread::sleep(RETRY_AFTER_ERROR);
            }
        }
    }
}

/// A lease for `account` as of now, refreshing first if it is about to expire.
fn issue(account: &Account) -> R<Lease> {
    let identity = live::active_identity()?;
    let is_live = identity
        .as_ref()
        .and_then(|o| o.get_str("emailAddress"))
        .is_some_and(|e| e.eq_ignore_ascii_case(&account.email));

    let credentials = owner_credential(account, is_live)?;
    let oauth_account = match identity.filter(|_| is_live) {
        Some(block) => block,
        None => store::read_config(account.num, &account.email)?
            .and_then(|c| c.get("oauthAccount").cloned())
            .ok_or_else(|| format!("account {} has no stored oauthAccount block", account.num))?,
    };
    Lease::from_owner(account, &credentials, oauth_account)
}

#[cfg(feature = "usage")]
fn owner_credential(account: &Account, is_live: bool) -> R<String> {
    crate::usage::fresh_credential(account, is_live, REFRESH_AHEAD)
}

/// Without network support nothing can refresh, so this lends what is there
/// and relies on Claude Code — or a full build — to keep it current.
#[cfg(not(feature = "usage"))]
fn owner_credential(account: &Account, is_live: bool) -> R<String> {
    let current = if is_live {
        live::read_credentials()?
    } else {
        store::read_backup(account.num, &account.email)?
    };
    current.ok_or_else(|| format!("account {} has no stored login", account.num))
}

// ----------------------------------------------------------------- peer side

/// `cswap borrow <account> --from <host>`, or `cswap borrow -` for a lease on
/// stdin (`ssh host cswap token work | cswap borrow -`).
pub fn borrow(source: &str, flags: &Flags) -> R<ExitCode> {
    let remote_bin = flags.remote_bin.clone();
    if let Some(bin) = &remote_bin {
        check_remote_word(bin, "--remote-bin")?;
    }
    let lease = if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("stdin: {e}"))?;
        let line = text
            .lines()
            .find(|l| !l.trim().is_empty())
            .ok_or("stdin held no lease")?;
        Lease::parse(line)?
    } else {
        let from = flags.from.as_deref().ok_or(
            "borrow needs --from <host>, the SSH destination of the machine that owns the login",
        )?;
        fetch_once(from, remote_bin.as_deref(), source)?
    };

    let (num, updated) = install(&lease, flags.from.clone(), remote_bin, flags)?;

    let verb = if updated { "Renewed" } else { "Borrowed" };
    let origin = flags
        .from
        .as_deref()
        .map(|f| format!(" from {f}"))
        .unwrap_or_default();
    println!("{verb} {} into slot {num}{origin}.", lease.email);
    if flags.from.is_some() {
        println!(
            "The token lasts until the owner next refreshes it — keep `cswap lease` running \
             to follow along."
        );
    } else {
        println!("It arrived on stdin, so nothing here can renew it; pass --from to fix that.");
    }
    if live_email()
        .as_deref()
        .map(|e| e.eq_ignore_ascii_case(&lease.email))
        != Some(true)
    {
        println!("Run `cswap switch {num}` to use it.");
    }
    Ok(ExitCode::SUCCESS)
}

/// File a new borrowed login into the store. Returns its slot and whether the
/// account was already here.
fn install(
    lease: &Lease,
    from: Option<String>,
    remote_bin: Option<String>,
    flags: &Flags,
) -> R<(i64, bool)> {
    let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
    let mut store_ref = Store::load()?;
    let placement = crate::cmd::place(
        &mut store_ref,
        &lease.email,
        lease.uuid.as_deref(),
        flags.slot,
        flags.force,
    )?;
    let num = placement.num;

    // Borrowing over your own login would throw away a refresh token — and
    // with it this machine's independent lineage — for a token that dies the
    // moment the owner refreshes. That is almost never what anyone meant.
    if let Some(current) = store::read_backup(num, &lease.email)? {
        if lease::has_refresh_token(&current) && !flags.force {
            return Err(format!(
                "slot {num} holds this machine's own login for {} — borrowing would replace \
                 it; pass --force if you mean that",
                lease.email
            ));
        }
    }

    let alias = flags
        .alias
        .clone()
        .or_else(|| placement.existing.as_ref().and_then(|a| a.alias.clone()));
    let account = lease.to_account(num, alias);
    store::write_backup(num, &account.email, &lease.credentials)?;
    let mut config = match store::read_config(num, &account.email)? {
        Some(existing) if existing.is_obj() => existing,
        _ => crate::json::Json::obj(),
    };
    config.set("oauthAccount", lease.oauth_account.clone());
    store::write_config(num, &account.email, &config)?;
    store_ref.register(&account);
    if let Some(alias) = &account.alias {
        crate::cmd::set_alias(&mut store_ref, num, alias);
    }
    if placement.keep_active {
        store_ref.set_active(num);
    }

    let mut cluster = Cluster::load()?;
    cluster.upsert(Borrowed {
        num,
        email: account.email.clone(),
        from,
        remote_bin,
    });
    cluster.save()?;
    store_ref.save()?;
    drop(_guard);

    // Re-borrowing the account this machine is already using is how a peer
    // recovers by hand when `lease` is not running; make it take effect.
    let record = Cluster::load()?
        .get(&account)
        .cloned()
        .expect("just recorded");
    apply(lease, &record)?;
    Ok((num, placement.existing.is_some()))
}

/// `cswap lease`: keep every borrowed login current, for as long as it runs.
pub fn lease(_flags: &Flags) -> R<ExitCode> {
    let cluster = Cluster::load()?;
    let records: Vec<Borrowed> = cluster
        .borrowed
        .into_iter()
        .filter(|b| b.from.is_some())
        .collect();
    if records.is_empty() {
        return Err("nothing here is borrowed from another machine — run \
             `cswap borrow <account> --from <host>` first"
            .into());
    }
    for record in &records {
        check_remote_word(&record.email, "account")?;
    }

    let (tx, rx) = mpsc::channel::<(usize, String)>();
    for (idx, record) in records.iter().enumerate() {
        let tx = tx.clone();
        let record = record.clone();
        thread::spawn(move || watch_forever(idx, &record, &tx));
    }
    drop(tx);

    for record in &records {
        log(&format!(
            "following slot {} ({}) from {}",
            record.num,
            record.email,
            record.source()
        ));
    }
    // One thread applies everything, so two leases never race for the store
    // or for Claude Code's credential locks.
    for (idx, line) in rx {
        let record = &records[idx];
        let outcome = Lease::parse(&line).and_then(|lease| {
            let applied = apply(&lease, record)?;
            Ok((lease, applied))
        });
        match outcome {
            Ok((lease, applied)) if applied.changed() => {
                let expiry = lease
                    .expires_at_ms()
                    .map(|ms| {
                        format!(
                            ", good for {}",
                            timefmt::countdown(ms / 1000 - timefmt::now_unix())
                        )
                    })
                    .unwrap_or_default();
                let target = if applied.live {
                    " and the live login"
                } else {
                    ""
                };
                log(&format!(
                    "slot {} ({}): new token from {}{expiry} — updated the slot{target}",
                    record.num,
                    record.email,
                    record.source()
                ));
            }
            Ok(_) => {}
            Err(e) => log(&format!("slot {} ({}): {e}", record.num, record.email)),
        }
    }
    Err("every lease connection has stopped".into())
}

/// Hold one `token --watch` open for a borrowed slot, reconnecting forever.
fn watch_forever(idx: usize, record: &Borrowed, tx: &mpsc::Sender<(usize, String)>) {
    let from = record
        .from
        .as_deref()
        .expect("filtered to renewable records");
    let mut backoff = Duration::from_secs(5);
    loop {
        let started = Instant::now();
        match spawn_token(from, record.remote_bin.as_deref(), &record.email, true) {
            Ok(mut child) => {
                forward_stderr(&mut child, record.num);
                if let Some(stdout) = child.stdout.take() {
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else { break };
                        if line.trim().is_empty() {
                            continue;
                        }
                        if tx.send((idx, line)).is_err() {
                            let _ = child.kill();
                            return;
                        }
                    }
                }
                let status = child.wait().map(|s| s.to_string()).unwrap_or_default();
                log(&format!(
                    "slot {}: connection to {from} ended ({status})",
                    record.num
                ));
            }
            Err(e) => log(&format!("slot {}: {e}", record.num)),
        }
        // A connection that held for a while was a healthy one that dropped;
        // start the backoff over. One that died at once is a config problem,
        // and hammering the owner's sshd will not fix it.
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(5);
        }
        log(&format!(
            "slot {}: reconnecting in {}s",
            record.num,
            backoff.as_secs()
        ));
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

fn forward_stderr(child: &mut Child, num: i64) {
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    log(&format!("slot {num}: owner says: {}", line.trim()));
                }
            }
        });
    }
}

#[derive(Default)]
struct Applied {
    slot: bool,
    live: bool,
}

impl Applied {
    fn changed(&self) -> bool {
        self.slot || self.live
    }
}

/// Put a renewed lease where it belongs: the slot's backup, and — if Claude
/// Code on this machine is logged in as this account — the live store.
fn apply(lease: &Lease, record: &Borrowed) -> R<Applied> {
    if !lease.email.eq_ignore_ascii_case(&record.email) {
        return Err(format!(
            "the owner sent a lease for {}, but slot {} borrows {} — ignoring it",
            lease.email, record.num, record.email
        ));
    }
    let token = lease.access_token();
    let mut applied = Applied::default();

    {
        let _guard = FileLock::store(STORE_LOCK_TIMEOUT)?;
        let store_ref = Store::load()?;
        let account = store_ref
            .get(record.num)
            .filter(|a| a.email.eq_ignore_ascii_case(&record.email))
            .ok_or_else(|| {
                format!(
                    "slot {} no longer holds {} — run `cswap borrow` again",
                    record.num, record.email
                )
            })?;
        let current = store::read_backup(account.num, &account.email)?;
        if current.as_deref().is_some_and(lease::has_refresh_token) {
            // Someone logged in as this account here and ran `add`: the slot
            // is this machine's own now. Stop treating it as borrowed.
            lease::forget_slot(account.num)?;
            return Err(format!(
                "slot {} now holds this machine's own login — no longer borrowing it",
                account.num
            ));
        }
        if current.as_deref().and_then(lease::access_token) != token {
            store::write_backup(account.num, &account.email, &lease.credentials)?;
            applied.slot = true;
        }
    }

    if live_email().is_some_and(|e| e.eq_ignore_ascii_case(&record.email)) {
        let _locks = DirLock::credentials()?;
        let current = live::read_credentials()?;
        match current.as_deref() {
            // Logged in here for real — a lineage of its own. Leave it be.
            Some(c) if lease::has_refresh_token(c) => {}
            Some(c) if lease::access_token(c) == token => {}
            _ => {
                let prepared = live::prepare_for_activation(&lease.credentials, current.as_deref());
                live::write_credentials(&prepared)?;
                applied.live = true;
            }
        }
    }
    Ok(applied)
}

/// Before `switch` activates a borrowed slot, fetch its current token: a
/// stored one is revoked as soon as the owner refreshes, and without `lease`
/// running nothing else will have noticed. Best effort — a switch to a stale
/// token is still a switch, and the warning says what to do.
pub fn renew_before_switch(selector: Option<&str>) {
    let Ok(store_ref) = Store::load() else { return };
    let target = match selector {
        Some(sel) => store_ref.resolve(sel).ok(),
        None => store_ref.next_after(store_ref.active()),
    };
    let Some(account) = target.and_then(|n| store_ref.get(n)) else {
        return;
    };
    let Ok(cluster) = Cluster::load() else { return };
    let Some(record) = cluster.get(&account).cloned() else {
        return;
    };
    let Some(from) = record.from.as_deref() else {
        return;
    };
    let result = fetch_once(from, record.remote_bin.as_deref(), &record.email)
        .and_then(|lease| apply(&lease, &record));
    if let Err(e) = result {
        eprintln!(
            "cswap: warning: could not renew {} from {from} ({e}); switching to the token \
             stored here, which fails if the owner has refreshed since",
            record.email
        );
    }
}

// ----------------------------------------------------------------- transport

/// `ssh <from> cswap token <selector>`, once, with a deadline.
fn fetch_once(from: &str, remote_bin: Option<&str>, selector: &str) -> R<Lease> {
    let mut child = spawn_token(from, remote_bin, selector, false)?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out_reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + FETCH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "no answer from {from} after {}s",
                    FETCH_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("waiting for ssh: {e}")),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = err.trim();
        return Err(if detail.is_empty() {
            format!("`cswap token` on {from} failed ({status})")
        } else {
            format!("{from}: {}", detail.lines().last().unwrap_or(detail))
        });
    }
    let line = out
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| format!("{from} sent nothing back"))?;
    Lease::parse(line)
}

fn spawn_token(from: &str, remote_bin: Option<&str>, selector: &str, watch: bool) -> R<Child> {
    check_remote_word(selector, "account")?;
    let program = std::env::var("CSWAP_SSH").unwrap_or_else(|_| "ssh".to_string());
    let mut cmd = Command::new(&program);
    let is_ssh = std::path::Path::new(&program)
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("ssh"));
    if is_ssh {
        // BatchMode: never stop to ask for a password or a host key — there is
        // nobody at the prompt when `lease` reconnects at 3am. Keepalives turn
        // a silently dead link into a closed pipe within ~45s.
        cmd.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
    }
    cmd.arg(from)
        .arg(remote_bin.unwrap_or("cswap"))
        .arg("token")
        .arg(selector);
    if watch {
        cmd.arg("--watch");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {program}: {e}"))
}

/// SSH hands the remote command to the owner's login shell — `sh` on one
/// machine, PowerShell on the next — as a single string. Rather than quote for
/// every shell, accept only words that need no quoting in any of them.
fn check_remote_word(word: &str, what: &str) -> R<()> {
    let ok = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._+-/\\:~".contains(c));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{what} '{word}' has characters that cannot be passed safely over SSH"
        ))
    }
}

fn live_email() -> Option<String> {
    live::active_identity()
        .ok()
        .flatten()
        .and_then(|o| o.get_str("emailAddress").map(str::to_string))
}

fn log(message: &str) {
    let now = timefmt::utc_stamp(timefmt::now_unix());
    eprintln!("{now}  {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_shell_neutral_words_go_over_ssh() {
        for ok in [
            "you@example.com",
            "a.b+tag@x.io",
            "3",
            "work",
            "/home/me/.local/bin/cswap",
            r"C:\Users\me\cswap.exe",
            "~/bin/cswap",
        ] {
            assert!(check_remote_word(ok, "x").is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "a;rm -rf ~",
            "$(id)",
            "`id`",
            "a|b",
            "'x'",
            "a&b",
        ] {
            assert!(check_remote_word(bad, "x").is_err(), "{bad}");
        }
    }
}
