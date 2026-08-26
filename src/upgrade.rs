//! `cswap upgrade` — replace this binary with the latest GitHub release.
//!
//! Releases ship raw binaries rather than archives, so upgrading needs no
//! tar/zip code and stays inside the one dependency the project already has.
//! The download is authenticated by TLS to github.com; there is no signature
//! check, so an upgrade trusts GitHub exactly as much as the original install did.

use std::fs;
use std::path::Path;

use crate::fsx::R;
use crate::json::Json;

const REPO: &str = "ibrahimthecosmic/cswap";
const RELEASES_API: &str = "https://api.github.com/repos/ibrahimthecosmic/cswap/releases/latest";
const USER_AGENT: &str = concat!("cswap/", env!("CARGO_PKG_VERSION"));

/// The release asset for the platform this binary was built for. `None` on a
/// platform we do not publish binaries for — a source build should not be
/// silently replaced by one for a different target.
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("cswap-linux-x86_64"),
        ("windows", "x86_64") => Some("cswap-windows-x86_64.exe"),
        _ => None,
    }
}

pub fn run(check_only: bool) -> R<bool> {
    let current = env!("CARGO_PKG_VERSION");
    let release = fetch_latest()?;
    let latest = release.tag.trim_start_matches('v');

    if compare_versions(latest, current) != std::cmp::Ordering::Greater {
        println!("cswap {current} is already the latest release.");
        return Ok(false);
    }

    println!("cswap {current} → {latest}");
    if check_only {
        println!("Run `cswap upgrade` to install it.");
        return Ok(true);
    }

    let wanted = asset_name().ok_or_else(|| {
        format!(
            "no published binary for {}-{} — upgrade from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let url = release
        .assets
        .iter()
        .find(|(name, _)| name == wanted)
        .map(|(_, url)| url.clone())
        .ok_or_else(|| format!("release {} has no asset named {wanted}", release.tag))?;

    let bytes = download(&url)?;
    validate(&bytes)?;
    replace_self(&bytes)?;

    println!("Upgraded to cswap {latest}.");
    println!("https://github.com/{REPO}/releases/tag/{}", release.tag);
    Ok(true)
}

struct Release {
    tag: String,
    /// (asset name, download URL)
    assets: Vec<(String, String)>,
}

fn fetch_latest() -> R<Release> {
    let mut response = agent()
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("could not reach GitHub: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("could not read GitHub's response: {e}"))?;
    if status == 404 {
        return Err(format!("{REPO} has no published releases yet"));
    }
    if !(200..300).contains(&status) {
        return Err(format!("GitHub returned HTTP {status}"));
    }

    let data = Json::parse(&body).map_err(|e| format!("GitHub returned malformed JSON: {e}"))?;
    let tag = data
        .get_str("tag_name")
        .ok_or("GitHub's release payload has no tag_name")?
        .to_string();
    let assets = data
        .get("assets")
        .and_then(Json::as_arr)
        .map(|items| {
            items
                .iter()
                .filter_map(|a| {
                    Some((
                        a.get_str("name")?.to_string(),
                        a.get_str("browser_download_url")?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

fn download(url: &str) -> R<Vec<u8>> {
    let mut response = agent()
        .get(url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("download failed: HTTP {status}"));
    }
    response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("download failed: {e}"))
}

/// Refuse anything that is not an executable for this platform. A truncated
/// download or an HTML error page rendered as a binary would otherwise be
/// renamed over a working `cswap`.
fn validate(bytes: &[u8]) -> R<()> {
    if bytes.len() < 100_000 {
        return Err(format!(
            "downloaded file is only {} bytes — refusing it",
            bytes.len()
        ));
    }
    let magic_ok = if cfg!(windows) {
        bytes.starts_with(b"MZ")
    } else {
        bytes.starts_with(b"\x7fELF")
    };
    if !magic_ok {
        return Err("downloaded file is not an executable for this platform".into());
    }
    Ok(())
}

/// Swap the new binary in beside the old one, then rename it into place.
///
/// The temp file sits in the same directory as the target so the final step is
/// an atomic same-filesystem rename: a `cswap` invoked mid-upgrade sees either
/// the old binary or the new one, never a half-written file.
fn replace_self(bytes: &[u8]) -> R<()> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe.parent().ok_or("this binary has no parent directory")?;
    let staged = dir.join(format!(".cswap-upgrade-{}.tmp", std::process::id()));

    write_executable(&staged, bytes).map_err(|e| {
        let _ = fs::remove_file(&staged);
        format!(
            "cannot write to {} ({e}) — if cswap is installed system-wide, re-run with elevated \
             privileges or download the binary manually",
            dir.display()
        )
    })?;

    // Windows refuses to rename over a running executable, so move the running
    // one aside first. The leftover is deleted on the next upgrade if the OS
    // still has it open now.
    #[cfg(windows)]
    {
        let retired = exe.with_extension("old");
        let _ = fs::remove_file(&retired);
        if let Err(e) = fs::rename(&exe, &retired) {
            let _ = fs::remove_file(&staged);
            return Err(format!("cannot move the running binary aside: {e}"));
        }
        if let Err(e) = fs::rename(&staged, &exe) {
            // Put the working binary back rather than leaving nothing at all.
            let _ = fs::rename(&retired, &exe);
            let _ = fs::remove_file(&staged);
            return Err(format!("cannot install the new binary: {e}"));
        }
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        fs::rename(&staged, &exe).map_err(|e| {
            let _ = fs::remove_file(&staged);
            format!("cannot install the new binary at {}: {e}", exe.display())
        })
    }
}

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// Compare dotted numeric versions, ignoring any pre-release suffix. Enough for
/// the `v0.1.0` tags this project publishes, and it treats anything unparseable
/// as equal rather than as an upgrade.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|p| p.parse::<u64>().ok())
            .collect()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        match a
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&b.get(i).copied().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn newer_versions_win_and_equal_ones_do_not() {
        assert_eq!(compare_versions("0.2.0", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.10", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "0.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.0", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.1.0", "0.1.1"), Ordering::Less);
        // A shorter version is padded, not treated as smaller.
        assert_eq!(compare_versions("0.2", "0.2.0"), Ordering::Equal);
    }

    #[test]
    fn unparseable_versions_never_trigger_an_upgrade() {
        assert_eq!(compare_versions("nightly", "0.1.0"), Ordering::Less);
        assert_eq!(compare_versions("", "0.1.0"), Ordering::Less);
    }

    #[test]
    fn validate_rejects_short_and_non_executable_payloads() {
        assert!(validate(b"<html>404</html>").is_err());
        let mut html = vec![b'<'; 200_000];
        html[0] = b'<';
        assert!(validate(&html).is_err());
        let mut elf = vec![0u8; 200_000];
        elf[..4].copy_from_slice(b"\x7fELF");
        if cfg!(unix) {
            assert!(validate(&elf).is_ok());
        }
    }
}
