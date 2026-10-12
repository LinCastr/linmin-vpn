//! One-time setup: the polkit helper (no more password prompts), the
//! radmin-vpn-linux AppImage, the app config and the login autostart entry.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::vpn::data_dir;

pub const HELPER_PATH: &str = "/usr/local/libexec/radmin-wired/radmin-net";
pub const POLICY_PATH: &str = "/usr/share/polkit-1/actions/io.github.valcones47.radmin-wired.policy";
const HELPER: &str = include_str!("../data/radmin-net");
const POLICY: &str = include_str!("../data/io.github.valcones47.radmin-wired.policy");
/// The Wine pipe relay, cross-compiled by `build.rs`.
const RELAY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rvrelay.exe"));

/// Pinned radmin-vpn-linux release. Bump both together.
pub const APPIMAGE_URL: &str = "https://github.com/baptisterajaut/radmin-vpn-linux/releases/download/v1.1.0/RadminVPN-Linux-x86_64.AppImage";
const APPIMAGE_SHA256: &str = "965ea3976bf2d59987f864efd72811b5df333391c7a45521217b8f57ef285744";
const APPIMAGE_NAME: &str = "RadminVPN-Linux-x86_64.AppImage";

// Config ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// "default" or "wallust".
    pub theme: String,
    /// "auto", "en" or "pt".
    pub language: String,
    /// Start hidden in the tray when the session starts.
    pub autostart: bool,
    /// Turn the VPN on when the app starts from the session.
    pub connect_on_login: bool,
    /// An AppImage the user picked instead of the downloaded one.
    pub appimage: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            language: "auto".into(),
            autostart: false,
            connect_on_login: false,
            appimage: None,
        }
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".config"))
        .join("radmin-wired/config.json")
}

pub fn load() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(c: &Config) -> Result<()> {
    let p = config_path();
    fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(c)?)?;
    fs::rename(tmp, p)?;
    Ok(())
}

// Helper ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HelperState {
    Missing,
    /// Installed by an older version of this app.
    Outdated,
    Ok,
}

pub fn helper_state() -> HelperState {
    match (fs::read_to_string(HELPER_PATH), fs::read_to_string(POLICY_PATH)) {
        (Ok(h), Ok(p)) if h == HELPER && p == POLICY => HelperState::Ok,
        (Ok(_), _) | (_, Ok(_)) => HelperState::Outdated,
        _ => HelperState::Missing,
    }
}

/// Installs (or removes) the helper and its polkit action with one
/// administrator prompt.
pub fn set_helper(install: bool) -> Result<()> {
    let status = if install {
        let dir = data_dir().join("install");
        fs::create_dir_all(&dir)?;
        let (h, p) = (dir.join("radmin-net"), dir.join("radmin-wired.policy"));
        fs::write(&h, HELPER)?;
        fs::write(&p, POLICY)?;
        let st = Command::new("pkexec")
            .args([
                "/bin/sh",
                "-c",
                r#"install -D -m 0755 -o root -g root "$1" "$3" && install -D -m 0644 -o root -g root "$2" "$4""#,
                "sh",
            ])
            .args([&h, &p])
            .args([HELPER_PATH, POLICY_PATH])
            .status();
        let _ = fs::remove_dir_all(&dir);
        st
    } else {
        Command::new("pkexec")
            .args(["/bin/sh", "-c", r#"rm -f "$1" "$2"; rmdir "$(dirname "$1")" 2>/dev/null; true"#, "sh"])
            .args([HELPER_PATH, POLICY_PATH])
            .status()
    }
    .context("pkexec")?;
    if !status.success() {
        bail!("pkexec exited with {status}");
    }
    Ok(())
}

// Pipe relay -----------------------------------------------------------------

/// Where the installed relay lives.
pub fn relay_path() -> PathBuf {
    data_dir().join("rvrelay.exe")
}

/// Installs the embedded relay to `relay_path()` if missing or a different size,
/// and returns its path. Cheap to call repeatedly (size check only).
pub fn ensure_relay() -> Result<PathBuf> {
    let path = relay_path();
    let up_to_date = fs::metadata(&path).map(|m| m.len() == RELAY.len() as u64).unwrap_or(false);
    if !up_to_date {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, RELAY).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path)
}

// AppImage -------------------------------------------------------------------

fn downloaded_appimage() -> PathBuf {
    data_dir().join(APPIMAGE_NAME)
}

/// The AppImage to run: the one the user picked, or ours once downloaded.
pub fn appimage(c: &Config) -> Option<PathBuf> {
    c.appimage
        .clone()
        .filter(|p| p.is_file())
        .or_else(|| Some(downloaded_appimage()).filter(|p| p.is_file()))
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut f = fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// A copy of the pinned AppImage the user already downloaded, so the 180 MB
/// download can be skipped.
pub fn local_copy() -> Option<PathBuf> {
    let home = crate::home();
    [home.join("Downloads"), home.join("Applications"), home.join("Transferências")]
        .into_iter()
        .map(|d| d.join(APPIMAGE_NAME))
        .find(|p| p.is_file() && sha256_file(p).is_ok_and(|h| h == APPIMAGE_SHA256))
}

/// Copies a local AppImage or downloads the pinned one, checking its hash.
/// `progress` gets (bytes done, total bytes).
pub fn fetch_appimage(progress: impl Fn(u64, u64)) -> Result<PathBuf> {
    let dest = downloaded_appimage();
    fs::create_dir_all(dest.parent().unwrap())?;
    let part = dest.with_extension("AppImage.part");
    if let Some(src) = local_copy() {
        fs::copy(&src, &part)?;
    } else {
        let resp = ureq::get(APPIMAGE_URL).call()?;
        let total = resp.header("Content-Length").and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut reader = resp.into_reader();
        let mut out = fs::File::create(&part)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        out.flush()?;
    }
    if sha256_file(&part)? != APPIMAGE_SHA256 {
        let _ = fs::remove_file(&part);
        bail!("checksum mismatch");
    }
    make_executable(&part)?;
    fs::rename(&part, &dest)?;
    Ok(dest)
}

// Autostart ------------------------------------------------------------------

fn autostart_path() -> PathBuf {
    config_path().parent().unwrap().with_file_name("autostart").join("radmin-wired.desktop")
}

pub fn set_autostart(on: bool) -> Result<()> {
    let p = autostart_path();
    if !on {
        let _ = fs::remove_file(p);
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    fs::create_dir_all(p.parent().unwrap())?;
    fs::write(
        p,
        format!(
            "[Desktop Entry]\nType=Application\nName=Radmin VPN\nExec=\"{}\" --tray\nIcon=network-vpn\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        ),
    )?;
    Ok(())
}
