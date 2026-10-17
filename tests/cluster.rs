//! End-to-end `token` / `borrow` / `lease`: an owner machine and a peer, each
//! with its own store and Claude Code config, driven through the binary.
//!
//! The SSH hop is replaced by a tiny script (`CSWAP_SSH`) that drops the host
//! argument and runs the rest against the owner's directories — everything on
//! either side of the pipe is the real code path.

mod common;

use std::fs;

use common::Machine;

/// 2100-01-01. `token` refreshes anything close to expiry, which would mean
/// the network; these logins must never be close.
const FUTURE: i64 = 4102444800000;

fn lease_from(owner: &Machine, account: &str) -> String {
    owner.ok(&["token", account])
}

fn backup_files(machine: &Machine) -> String {
    let dir = machine.store_dir().join("credentials");
    let mut all = String::new();
    for entry in fs::read_dir(&dir).expect("credentials dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".enc") {
            all.push_str(&name);
            all.push('\n');
        }
    }
    all
}

#[test]
fn a_lease_carries_the_access_token_and_never_the_refresh_token() {
    let owner = Machine::new("owner");
    owner.log_in_until("me@mail.com", "GEN1", FUTURE);
    owner.ok(&["add"]);

    let line = lease_from(&owner, "me@mail.com");
    assert!(line.contains("access-GEN1"), "{line}");
    assert!(
        !line.contains("refresh"),
        "a lease leaked a refresh token: {line}"
    );

    let peer = Machine::new("peer");
    peer.log_in_until("mine@mail.com", "LOCAL", FUTURE);
    peer.ok(&["add"]);
    let out = peer.run_stdin(&["borrow", "-"], &line);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let listed = peer.ok(&["list", "--no-usage", "--json"]);
    assert!(listed.contains(r#""borrowedFrom": "stdin""#), "{listed}");

    // Borrowing is not logging in: the peer's own account is still live.
    assert!(peer.live_credential().contains("refresh-LOCAL"));

    peer.ok(&["switch", "me@mail.com"]);
    let live = peer.live_credential();
    assert!(live.contains("access-GEN1"), "{live}");
    assert!(!live.contains("refresh"), "{live}");
    assert!(peer.live_config().contains("me@mail.com"));

    // And back: the peer's own login comes back intact.
    peer.ok(&["switch", "mine@mail.com"]);
    assert!(peer.live_credential().contains("refresh-LOCAL"));
    // Its snapshot of the borrowed slot never picked up a refresh token.
    assert!(backup_files(&peer).contains("me@mail.com"));
}

#[test]
fn a_borrowed_login_is_not_exported_and_a_real_login_reclaims_the_slot() {
    let owner = Machine::new("owner");
    owner.log_in_until("me@mail.com", "GEN1", FUTURE);
    owner.ok(&["add"]);

    let peer = Machine::new("peer");
    let out = peer.run_stdin(&["borrow", "-"], &lease_from(&owner, "me@mail.com"));
    assert!(out.status.success());

    let file = peer.root.join("x.json");
    let refused = peer.run(&["export", "me@mail.com", "-o", file.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("borrowed"));

    // Logging in as the account on the peer and adding it makes the slot the
    // peer's own; it is no longer borrowed, and exports like any other.
    peer.log_in_until("me@mail.com", "PEER-OWN", FUTURE);
    peer.ok(&["add"]);
    let listed = peer.ok(&["list", "--no-usage", "--json"]);
    assert!(!listed.contains("borrowedFrom"), "{listed}");
    peer.ok(&["export", "me@mail.com", "-o", file.to_str().unwrap()]);

    // Borrowing over that own login is refused without --force: it would
    // throw away a refresh token for one that dies on the owner's next refresh.
    let over = peer.run_stdin(&["borrow", "-"], &lease_from(&owner, "me@mail.com"));
    assert!(!over.status.success());
    assert!(String::from_utf8_lossy(&over.stderr).contains("own login"));
}

#[test]
fn a_borrowed_login_cannot_be_lent_on() {
    let owner = Machine::new("owner");
    owner.log_in_until("me@mail.com", "GEN1", FUTURE);
    owner.ok(&["add"]);
    let peer = Machine::new("peer");
    assert!(peer
        .run_stdin(&["borrow", "-"], &lease_from(&owner, "me@mail.com"))
        .status
        .success());
    let out = peer.run(&["token", "me@mail.com"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only the owner"));
}

#[cfg(unix)]
mod over_ssh {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// Stand-in for `ssh`: `fake-ssh <host> <cmd...>` runs `<cmd...>` as the
    /// owner machine would.
    fn fake_ssh(peer: &Machine, owner: &Machine) -> std::path::PathBuf {
        let path = peer.root.join("fake-ssh");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nshift\nexec env XDG_DATA_HOME='{data}' CLAUDE_CONFIG_DIR='{config}' \
                 HOME='{home}' \"$@\"\n",
                data = owner.root.join("data").display(),
                config = owner.config_dir().display(),
                home = owner.root.display(),
            ),
        )
        .expect("write fake ssh");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn peer_cmd(peer: &Machine, ssh: &std::path::Path, args: &[&str]) -> std::process::Output {
        peer.command(args)
            .env("CSWAP_SSH", ssh)
            .output()
            .expect("run cswap")
    }

    fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn owner_refreshes_reach_the_peer_through_switch_and_lease() {
        let owner = Machine::new("owner");
        owner.log_in_until("me@mail.com", "GEN1", FUTURE);
        owner.ok(&["add", "--alias", "work"]);

        let peer = Machine::new("peer");
        let ssh = fake_ssh(&peer, &owner);
        let bin = common::BIN;

        let out = peer_cmd(
            &peer,
            &ssh,
            &["borrow", "work", "--from", "owner-box", "--remote-bin", bin],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = peer_cmd(&peer, &ssh, &["switch", "me@mail.com"]);
        assert!(out.status.success());
        assert!(peer.live_credential().contains("access-GEN1"));

        // Claude Code on the owner refreshes: GEN1 is now revoked everywhere.
        owner.log_in_until("me@mail.com", "GEN2", FUTURE);

        // `switch` renews first, even onto the account already in use.
        let out = peer_cmd(&peer, &ssh, &["switch", "me@mail.com"]);
        assert!(out.status.success());
        assert!(
            peer.live_credential().contains("access-GEN2"),
            "switch did not renew: {}",
            peer.live_credential()
        );

        // `lease` follows further refreshes without being asked.
        let mut lease = peer
            .command(&["lease"])
            .env("CSWAP_SSH", &ssh)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start lease");
        // Give the watch a moment to connect before the owner moves.
        std::thread::sleep(Duration::from_millis(500));
        owner.log_in_until("me@mail.com", "GEN3", FUTURE);
        wait_for("lease to push GEN3", || {
            peer.live_credential().contains("access-GEN3")
        });
        let _ = lease.kill();
        let _ = lease.wait();
        assert!(!peer.live_credential().contains("refresh"));
    }
}
