//! In-app update checker and self-updater via GitHub Releases.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const CURRENT_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
pub const GITHUB_REPO: &str = "LinCastr/linmin-vpn";

#[derive(Deserialize, Debug)]
struct ReleaseInfo {
    tag_name: String,
}

/// Parses "v0.1.2" or "0.1.2" into (major, minor, patch).
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let s = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let mut parts = s.split('.');
    let maj = parts.next()?.parse().ok()?;
    let min = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((maj, min, patch))
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    match (parse_version(remote), parse_version(current)) {
        (Some(r), Some(c)) => r > c,
        _ => false,
    }
}

/// Checks the latest GitHub release. Returns Some(latest_tag) if newer than current.
pub fn check_for_update() -> Result<Option<String>> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", &format!("linmin-vpn/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(5))
        .call()?;
    let info: ReleaseInfo = serde_json::from_reader(resp.into_reader())?;

    if is_newer(&info.tag_name, CURRENT_VERSION) {
        Ok(Some(info.tag_name))
    } else {
        Ok(None)
    }
}

/// Downloads the target release binary and atomically replaces the current executable.
pub fn apply_update(target_version: &str, progress: impl Fn(f32)) -> Result<String> {
    use sha2::{Digest, Sha256};

    let release_url = if target_version.is_empty() || target_version == "latest" {
        format!("https://github.com/{GITHUB_REPO}/releases/latest/download/linmin-vpn-x86_64-linux")
    } else {
        format!(
            "https://github.com/{GITHUB_REPO}/releases/download/{target_version}/linmin-vpn-x86_64-linux"
        )
    };

    let agent = crate::setup::http_agent();
    let ua = format!("linmin-vpn/{}", env!("CARGO_PKG_VERSION"));

    // The checksum is required: never install a binary we could not verify.
    let mut expected = String::new();
    agent
        .get(&format!("{release_url}.sha256"))
        .set("User-Agent", &ua)
        .call()
        .context("download checksum")?
        .into_reader()
        .take(4096)
        .read_to_string(&mut expected)?;
    let expected_hash = expected.split_whitespace().next().unwrap_or("").to_lowercase();
    if expected_hash.len() != 64 {
        anyhow::bail!("invalid checksum file");
    }

    let resp = agent.get(&release_url).set("User-Agent", &ua).call()?;
    let total = resp.header("Content-Length").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let mut reader = resp.into_reader();

    let exe = std::env::current_exe()?;
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".linmin-vpn-update.{}", std::process::id()));

    let mut download = || -> Result<String> {
        let mut out = fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        let mut hasher = Sha256::new();
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            done += n as u64;
            if total > 0 {
                progress(done as f32 / total as f32);
            }
        }
        out.flush()?;
        if total > 0 && done != total {
            anyhow::bail!("download interrupted ({done} of {total} bytes)");
        }
        Ok(format!("{:x}", hasher.finalize()))
    };
    let downloaded_hash = match download() {
        Ok(h) => h,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if downloaded_hash != expected_hash {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("Checksum mismatch: expected {expected_hash}, got {downloaded_hash}");
    }

    // Make executable, then atomically replace the running binary.
    let replaced = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .and_then(|_| fs::rename(&tmp, &exe))
        .with_context(|| format!("replacing {}", exe.display()));
    if replaced.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    replaced?;

    // Also update ~/.local/bin/linmin-vpn if it exists and differs from exe
    let user_bin = crate::home().join(".local/bin/linmin-vpn");
    if user_bin.exists() && user_bin != exe && !user_bin.is_symlink() {
        let _ = fs::copy(&exe, &user_bin);
    }

    let final_ver = if !target_version.is_empty() && target_version != "latest" {
        target_version.to_string()
    } else {
        CURRENT_VERSION.to_string()
    };

    Ok(final_ver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("v0.1.2", "v0.1.1"));
        assert!(is_newer("v0.2.0", "v0.1.9"));
        assert!(is_newer("v1.0.0", "v0.9.9"));
        assert!(!is_newer("v0.1.0", "v0.1.1"));
        assert!(!is_newer("v0.1.1", "v0.1.1"));
    }
}
