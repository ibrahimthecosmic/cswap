//! Where Claude Code and claude-swap keep their files.
//!
//! Mirrors claude-swap 0.25's `paths.py`, which in turn mirrors Claude Code's
//! own resolution, so this binary reads and writes exactly the files the Python
//! tool does — you can run either against the same store.

use std::env;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    #[cfg(unix)]
    let var = "HOME";
    #[cfg(windows)]
    let var = "USERPROFILE";
    env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `CLAUDE_CONFIG_DIR` if set, else `~/.claude`.
pub fn claude_config_home() -> PathBuf {
    match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(".claude"),
    }
}

/// Claude Code's active OAuth credential file (Linux/Windows; on macOS the
/// Keychain holds it and this is only a fallback).
pub fn credentials_path() -> PathBuf {
    claude_config_home().join(".credentials.json")
}

/// The global config: legacy `<config-home>/.config.json` when it exists, else
/// `~/.claude.json`. Note the asymmetry — the default lives at the *home*
/// directory, not inside `~/.claude/`.
pub fn global_config_path() -> PathBuf {
    let legacy = claude_config_home().join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join(".claude.json"),
        _ => home().join(".claude.json"),
    }
}

/// claude-swap's data root. Linux/WSL follow XDG; macOS and Windows keep the
/// legacy `~/.claude-swap-backup`.
pub fn backup_root() -> PathBuf {
    if cfg!(target_os = "linux") {
        if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
            let p = PathBuf::from(&xdg);
            // Per the XDG spec a relative value is ignored, not resolved.
            if p.is_absolute() {
                return p.join("claude-swap");
            }
        }
        return home().join(".local").join("share").join("claude-swap");
    }
    home().join(".claude-swap-backup")
}

pub fn sequence_file() -> PathBuf {
    backup_root().join("sequence.json")
}

pub fn configs_dir() -> PathBuf {
    backup_root().join("configs")
}

pub fn credentials_dir() -> PathBuf {
    backup_root().join("credentials")
}

#[cfg_attr(not(feature = "usage"), allow(dead_code))]
pub fn cache_dir() -> PathBuf {
    backup_root().join("cache")
}
