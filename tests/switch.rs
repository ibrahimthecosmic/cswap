//! End-to-end `switch` when the live login has moved without cswap: a `/login`
//! inside Claude Code, `claude auth login`, or any other tool that writes the
//! credential and `oauthAccount` directly.
//!
//! The regression: the store's active marker still named the account cswap last
//! switched to, so switching back to it was "already active" and did nothing,
//! rotating landed on the account that was in fact live, and every switch ended
//! on the login made behind cswap's back.

mod common;

use common::Machine;

/// Two stored accounts, switched to `a`, then logged into `b` behind cswap's
/// back — the state a peer machine was found in.
fn drifted_to_b() -> Machine {
    let pc = Machine::new("drift");
    pc.log_in_as("a@mail.com", "A1");
    pc.ok(&["add"]);
    pc.log_in_as("b@mail.com", "B1");
    pc.ok(&["add"]);
    pc.ok(&["switch", "a@mail.com"]);
    assert!(pc.live_credential().contains("access-A1"));

    pc.log_in_as("b@mail.com", "B2");
    pc
}

#[test]
fn switching_to_the_last_switched_account_after_a_login_elsewhere_switches() {
    let pc = drifted_to_b();

    let out = pc.ok(&["switch", "a@mail.com"]);
    assert!(out.contains("Switched"), "switch was a no-op: {out}");
    assert!(pc.live_credential().contains("access-A1"));
    assert!(pc.live_config().contains("a@mail.com"));
}

#[test]
fn the_login_made_elsewhere_is_filed_under_its_own_slot() {
    let pc = drifted_to_b();
    pc.ok(&["switch", "a@mail.com"]);

    // B2 went to b's slot, not a's — so each switch lands on its own account.
    pc.ok(&["switch", "b@mail.com"]);
    assert!(pc.live_credential().contains("access-B2"));
    pc.ok(&["switch", "a@mail.com"]);
    assert!(pc.live_credential().contains("access-A1"));
}

#[test]
fn list_marks_the_account_claude_code_is_actually_using() {
    let pc = drifted_to_b();

    let out = pc.ok(&["list", "--json", "--no-usage"]);
    assert!(
        out.contains(r#""activeAccountNumber": 2"#),
        "list trusted the stale marker: {out}"
    );
}

#[test]
fn a_login_cswap_never_stored_is_not_filed_under_any_slot() {
    let pc = drifted_to_b();
    pc.log_in_as("stranger@mail.com", "S1");

    let out = pc.run(&["switch", "b@mail.com"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stranger@mail.com"));

    // a's slot still holds a's login, not the stranger's.
    pc.ok(&["switch", "a@mail.com"]);
    assert!(pc.live_credential().contains("access-A1"));
}
