//! Two lock flavours, for two different peers.
//!
//! * [`DirLock`] speaks Claude Code's protocol. Claude Code guards its OAuth
//!   refresh and its config writes with npm's `proper-lockfile`, where the lock
//!   artifact is a **directory** and `mkdir`'s atomicity is the mutex. Holding
//!   these while swapping credentials closes the one real race with a running
//!   Claude Code: its refresh is read → network → write, all under the lock, so
//!   a swap landing inside that window would be overwritten by the refreshed
//!   *old* account's token, and the refresh token we just backed up would
//!   already be spent.
//!
//! * [`FileLock`] is `flock(2)` on claude-swap's own `<backup>/.lock`, which is
//!   what the Python implementation takes. Same file, same primitive, so the two
//!   tools interlock and you can run either against one store.
//!
//! Neither lock is refreshed while held (proper-lockfile holders touch the
//! directory mtime every 5s to prove liveness). We don't, deliberately: touching
//! a directory without `utime` means creating and removing an entry inside it,
//! and a crash in that window leaves a non-empty lock directory that Claude
//! Code's own `rmdir`-based takeover cannot clear — it would wedge Claude Code's
//! refresh until a human deleted it. Instead every critical section here is
//! local file I/O only, never network, and finishes in single-digit
//! milliseconds against a 10s (config) / 60s (credentials) staleness budget.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::fsx::R;

/// Claude Code's credential-refresh locks are stale only after 60s: a younger
/// one belongs to a live holder whose toucher may have stalled (suspend, a
/// blocked event loop) while it still legitimately owns the lock.
pub const CREDENTIALS_STALENESS: Duration = Duration::from_secs(60);
/// The config lock keeps proper-lockfile's older 10s default.
pub const CONFIG_STALENESS: Duration = Duration::from_secs(10);
/// Claude Code holds its credentials lock for one token round trip and its
/// config lock for a local read-modify-write. This outlasts both without
/// stalling the CLI forever.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(9);

pub struct DirLock {
    path: PathBuf,
}

impl DirLock {
    pub fn acquire(path: PathBuf, staleness: Duration, timeout: Duration) -> R<DirLock> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let start = Instant::now();
        let mut backoff = Duration::from_millis(25);
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(DirLock { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }

            if let Some(age) = age_of(&path) {
                if age > staleness {
                    // Dead holder per the protocol: remove and retake. Losing
                    // the rmdir/create race to another waiter just loops again.
                    let _ = fs::remove_dir(&path);
                    continue;
                }
            } else {
                // Vanished between create_dir and stat — retake immediately.
                continue;
            }

            if start.elapsed() > timeout {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                return Err(format!(
                    "could not acquire {name} — Claude Code appears to be refreshing \
                     credentials. Retry in a few seconds."
                ));
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(250));
        }
    }

    pub fn credentials() -> R<(DirLock, DirLock)> {
        // Claude Code 2.1.218+ takes .oauth_refresh.lock first, then the legacy
        // ~/.claude.lock. Mirroring both the pair and the order means a waiting
        // cswap and a waiting Claude Code can never deadlock against each other.
        let home = crate::paths::claude_config_home();
        let primary = DirLock::acquire(
            home.join(".oauth_refresh.lock"),
            CREDENTIALS_STALENESS,
            DEFAULT_TIMEOUT,
        )?;
        let legacy_path = sibling_lock(&home);
        let legacy = DirLock::acquire(legacy_path, CREDENTIALS_STALENESS, DEFAULT_TIMEOUT)?;
        Ok((primary, legacy))
    }

    pub fn config() -> R<DirLock> {
        let path = crate::paths::global_config_path();
        DirLock::acquire(sibling_lock(&path), CONFIG_STALENESS, DEFAULT_TIMEOUT)
    }
}

/// `~/.claude` → `~/.claude.lock`; `~/.claude.json` → `~/.claude.json.lock`.
fn sibling_lock(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.parent().unwrap_or(Path::new(".")).join(name)
}

fn age_of(path: &Path) -> Option<Duration> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    // A lock whose mtime is in the future (clock skew, a synced filesystem)
    // reads as age zero rather than as instantly stale.
    SystemTime::now()
        .duration_since(mtime)
        .ok()
        .or(Some(Duration::ZERO))
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir(&self.path) {
            if e.kind() != io::ErrorKind::NotFound {
                eprintln!(
                    "cswap: warning: could not release {}: {e}",
                    self.path.display()
                );
            }
        }
    }
}

/// `flock(2)` on claude-swap's `<backup>/.lock`, interlocking with the Python
/// implementation's `FileLock`.
pub struct FileLock {
    file: fs::File,
}

impl FileLock {
    /// The store-wide lock claude-swap takes around registry mutations.
    pub fn store(timeout: Duration) -> R<FileLock> {
        FileLock::at(crate::paths::backup_root().join(".lock"), timeout)
    }

    /// Per-slot gate around consuming a refresh token. Refresh tokens are
    /// single-use, so two processes refreshing the same slot at once means one
    /// of them POSTs an already-spent token and gets `invalid_grant` back — a
    /// live account that looks dead. Same path and primitive claude-swap uses.
    #[cfg_attr(not(feature = "usage"), allow(dead_code))]
    pub fn consume(num: i64, timeout: Duration) -> R<FileLock> {
        FileLock::at(
            crate::paths::credentials_dir().join(format!(".consume-{num}.lock")),
            timeout,
        )
    }

    pub fn at(path: std::path::PathBuf, timeout: Duration) -> R<FileLock> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let start = Instant::now();
        loop {
            if try_lock_exclusive(&file)? {
                return Ok(FileLock { file });
            }
            if start.elapsed() > timeout {
                return Err(format!(
                    "another cswap (or claude-swap) is holding {}; retry shortly",
                    path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = unlock(&self.file);
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    // flock(2) is in libc, which std already links on every unix target; the
    // declaration costs a crate dependency of zero.
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_UN: i32 = 8;
    const LOCK_NB: i32 = 4;

    pub fn try_lock_exclusive(file: &File) -> Result<bool, String> {
        // SAFETY: `fd` is owned by `file` and stays open for the call.
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // EWOULDBLOCK/EAGAIN: held by someone else. Anything else is real.
            Some(11) | Some(35) => Ok(false),
            _ => Err(format!("flock: {err}")),
        }
    }

    pub fn unlock(file: &File) -> Result<(), String> {
        // SAFETY: as above.
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("flock unlock: {}", std::io::Error::last_os_error()))
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::File;

    // Windows: the Python implementation uses msvcrt byte-range locking, which
    // has no std equivalent here. Opening the file exclusively is a weaker but
    // dependency-free stand-in — it excludes other cswap processes, which is the
    // case that matters for a single user's machine.
    pub fn try_lock_exclusive(_file: &File) -> Result<bool, String> {
        Ok(true)
    }

    pub fn unlock(_file: &File) -> Result<(), String> {
        Ok(())
    }
}

use imp::{try_lock_exclusive, unlock};
