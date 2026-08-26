//! Filesystem helpers: atomic replace and owner-only file modes.
//!
//! Every write that lands on a credential or on `~/.claude.json` goes through
//! `write_atomic`: temp file in the same directory, fsync, rename. A crash or a
//! full disk then leaves the previous file intact rather than a truncated one.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

pub type R<T> = Result<T, String>;

pub fn read_json_file(path: &Path) -> R<Option<crate::json::Json>> {
    match fs::read_to_string(path) {
        Ok(text) => crate::json::Json::parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write `contents` to `path` atomically, owner-read/write only.
///
/// The temp file is created in the destination directory so the final step is a
/// same-filesystem `rename`, which is atomic. It is fsynced first: without that,
/// a crash between rename and writeback can leave a correctly-named empty file,
/// which for a credential means a silently lost account.
pub fn write_atomic(path: &Path, contents: &str) -> R<()> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = tmp_path(dir, path);

    let result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        set_owner_only(&f)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

fn tmp_path(dir: &Path, target: &Path) -> PathBuf {
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    // pid + a monotonic counter: unique within and across processes without
    // needing a random source.
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{name}.{}.{n}.tmp", process::id()))
}

#[cfg(unix)]
fn set_owner_only(f: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only(_f: &fs::File) -> std::io::Result<()> {
    // Windows inherits the parent directory's ACL; the backup root lives under
    // the user profile, which is already owner-scoped.
    Ok(())
}
