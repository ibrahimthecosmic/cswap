//! End-to-end `export` / `import`, driven through the binary against a store
//! built from scratch in a temp directory.
//!
//! These live out here rather than next to the code because what they pin down
//! is the interaction between three things the unit tests each mock away: the
//! live credential, the slot's backup, and the file that travels between
//! machines. The regression they exist for — exporting a superseded credential —
//! passed every in-module test while shipping a login that was dead on arrival.

mod common;

use std::fs;
use std::path::Path;

use common::Machine;

fn exported_file(path: &Path) -> String {
    fs::read_to_string(path).expect("export file")
}

/// The regression: Claude Code refreshes its credential in place, so the slot's
/// backup is a *past* generation of a token lineage that is single-use. Export
/// must ship what is live, or the copy lands on the other machine already spent
/// — dead there while the source machine carries on working.
#[test]
fn export_ships_the_live_generation_not_the_stale_backup() {
    let pc = Machine::new("source");
    pc.log_in_as("me@mail.com", "GEN1");
    pc.ok(&["add"]);

    // Time passes; Claude Code refreshes itself and rotates the token.
    pc.log_in_as("me@mail.com", "GEN2");

    let out = pc.root.join("creds.json");
    pc.ok(&["export", "me@mail.com", "--output", out.to_str().unwrap()]);
    let text = exported_file(&out);

    assert!(
        text.contains("refresh-GEN2"),
        "export shipped a superseded credential:\n{text}"
    );
    assert!(
        !text.contains("refresh-GEN1"),
        "export shipped GEN1 as well"
    );
}

/// Exporting the live account must not smear its credential over the others.
#[test]
fn a_slot_that_is_not_live_keeps_its_own_credential() {
    let pc = Machine::new("two-slots");
    pc.log_in_as("one@mail.com", "ONE");
    pc.ok(&["add"]);
    pc.log_in_as("two@mail.com", "TWO");
    pc.ok(&["add"]);
    pc.log_in_as("two@mail.com", "TWO-REFRESHED");

    let out = pc.root.join("all.json");
    pc.ok(&["export", "--output", out.to_str().unwrap()]);
    let text = exported_file(&out);

    assert!(
        text.contains("refresh-ONE"),
        "slot 1 lost its own credential"
    );
    assert!(text.contains("refresh-TWO-REFRESHED"), "slot 2 is stale");
}

/// The whole point, end to end: an account exported on one machine and imported
/// on another becomes a login that `switch` can activate there.
#[test]
fn an_imported_account_switches_on_the_other_machine() {
    let source = Machine::new("a");
    source.log_in_as("me@mail.com", "GEN1");
    source.ok(&["add"]);
    let file = source.root.join("creds.json");
    source.ok(&["export", "me@mail.com", "-o", file.to_str().unwrap()]);

    let target = Machine::new("b");
    target.log_in_as("someone-else@mail.com", "OTHER");
    target.ok(&["add"]);
    let imported = target.ok(&["import", file.to_str().unwrap()]);
    assert!(imported.contains("me@mail.com"), "{imported}");

    // Importing is not logging in: the other account is still the live one.
    assert!(target.live_credential().contains("refresh-OTHER"));

    target.ok(&["switch", "me@mail.com"]);
    assert!(
        target.live_credential().contains("refresh-GEN1"),
        "switch did not activate the imported credential"
    );
    let config = target.live_config();
    assert!(
        config.contains("me@mail.com"),
        "oauthAccount was not updated"
    );
    // The target machine's own state is untouched by an import or a switch.
    assert!(config.contains("private"), "the local config was clobbered");
}

/// A file we did not write, or one missing the parts a login needs, is refused
/// rather than half-imported.
#[test]
fn a_malformed_export_is_refused() {
    let pc = Machine::new("bad-input");
    let path = pc.root.join("bad.json");

    for body in [
        r#"{"hello":1}"#,
        r#"{"kind":"cswap-export","accounts":[{"email":"a@x.com"}]}"#,
        r#"{"kind":"cswap-export","schemaVersion":99,"accounts":[]}"#,
    ] {
        fs::write(&path, body).expect("write");
        let out = pc.run(&["import", path.to_str().unwrap()]);
        assert!(!out.status.success(), "should have refused: {body}");
    }
}
