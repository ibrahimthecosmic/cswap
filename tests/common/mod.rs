//! A throwaway machine for driving the binary end to end: its own
//! claude-swap store and its own Claude Code config directory, both under one
//! temp root. Shared by the integration tests; each uses a different subset.

#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

pub const BIN: &str = env!("CARGO_BIN_EXE_cswap");

/// A throwaway machine: its own claude-swap store and its own Claude Code
/// config directory, both under one temp root.
pub struct Machine {
    pub root: PathBuf,
}

impl Machine {
    pub fn new(name: &str) -> Machine {
        let root = std::env::temp_dir().join(format!(
            "cswap-it-{}-{name}-{}",
            std::process::id(),
            // Two machines in one test must not collide, and neither must two
            // tests: cargo runs them in threads of one process.
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("config")).expect("temp dir");
        Machine { root }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The binary, pointed at this machine's store and config.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("CLAUDE_CONFIG_DIR", self.config_dir())
            // The store root is XDG on Linux only; elsewhere it follows $HOME.
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root);
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run cswap")
    }

    pub fn run_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run cswap");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait cswap")
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cswap {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Put a credential and a matching `oauthAccount` in place, as a login does.
    pub fn log_in_as(&self, email: &str, generation: &str) {
        self.log_in_until(email, generation, 1787811738997);
    }

    /// As `log_in_as`, with a chosen `expiresAt`. Anything that may refresh —
    /// `token` does — needs one in the future, or it goes to the network.
    pub fn log_in_until(&self, email: &str, generation: &str, expires_at_ms: i64) {
        fs::write(
            self.config_dir().join(".credentials.json"),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"access-{generation}",
                   "refreshToken":"refresh-{generation}","expiresAt":{expires_at_ms}}}}}"#
            ),
        )
        .expect("write credential");
        fs::write(
            self.config_dir().join(".claude.json"),
            format!(
                r#"{{"numStartups":4,"projects":{{"/w":{{"history":["private"]}}}},
                   "oauthAccount":{{"accountUuid":"uuid-{email}","emailAddress":"{email}",
                   "organizationUuid":"org","organizationName":"Org"}}}}"#
            ),
        )
        .expect("write config");
    }

    pub fn live_credential(&self) -> String {
        fs::read_to_string(self.config_dir().join(".credentials.json")).expect("live credential")
    }

    pub fn live_config(&self) -> String {
        fs::read_to_string(self.config_dir().join(".claude.json")).expect("live config")
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
