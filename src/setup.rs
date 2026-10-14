//! One-time setup: the polkit helper (no more password prompts), the
//! radmin-vpn-linux AppImage, the app config and the login autostart entry.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::vpn::data_dir;

pub const HELPER_PATH: &str = "/usr/local/libexec/linmin-vpn/radmin-net";
pub const POLICY_PATH: &str = "/usr/share/polkit-1/actions/io.github.lincastr.linmin-vpn.policy";
const HELPER: &str = include_str!("../data/radmin-net");
const POLICY: &str = include_str!("../data/io.github.lincastr.linmin-vpn.policy");
const SHIM: &str = include_str!("../data/sudo-shim");
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
        .join("linmin-vpn/config.json")
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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SavedNetProps {
    pub allow_without_password: bool,
    pub block_new_members: bool,
}

fn networks_cache_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".config"))
        .join("linmin-vpn/networks.json")
}

pub fn load_networks_cache() -> std::collections::HashMap<String, SavedNetProps> {
    fs::read_to_string(networks_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_network_cache(id: &str, allow_without_password: bool, block_new_members: bool) {
    let mut cache = load_networks_cache();
    cache.insert(
        id.to_string(),
        SavedNetProps {
            allow_without_password,
            block_new_members,
        },
    );
    let p = networks_cache_path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if let Ok(data) = serde_json::to_string_pretty(&cache) {
        if fs::write(&tmp, data).is_ok() {
            let _ = fs::rename(tmp, p);
        }
    }
}

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

pub fn shim_path() -> PathBuf {
    data_dir().join("shim/sudo")
}

pub fn ensure_shim() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let p = shim_path();
    let up_to_date = fs::read_to_string(&p).map(|s| s == SHIM).unwrap_or(false);
    if !up_to_date {
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&p, SHIM)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755))?;
    }
    Ok(p)
}

fn is_polkit_agent_running() -> bool {
    // Verifica se o agente do Quickshell (rice) ou polkit-kde está reportado ativo
    if let Ok(output) = Command::new("rice-polkit-daemon").arg("status").output() {
        if let Ok(s) = String::from_utf8(output.stdout) {
            if s.contains(r#""quickshell_polkit_enabled": true"#)
                || s.contains(r#""polkit_kde_agent_running": true"#)
            {
                return true;
            }
        }
    }

    let Ok(dir) = fs::read_dir("/proc") else { return false };
    let me = std::process::id() as i32;
    for entry in dir.flatten() {
        let Ok(pid) = entry.file_name().to_str().unwrap_or("").parse::<i32>() else { continue };
        if pid == me {
            continue;
        }
        let cmd = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
            .unwrap_or_default();
        if cmd.is_empty() {
            continue;
        }
        let is_agent = (cmd.contains("polkit") && cmd.contains("agent"))
            || cmd.contains("polkit-kde")
            || cmd.contains("polkit-gnome")
            || cmd.contains("hyprpolkitagent")
            || cmd.contains("lxpolkit");
        if is_agent && !cmd.contains("polkitd") {
            return true;
        }
    }
    false
}

fn ensure_polkit_agent() {
    if is_polkit_agent_running() {
        return;
    }

    // Tenta rice-polkit-daemon se existir
    let _ = Command::new("rice-polkit-daemon").arg("ensure").status();
    std::thread::sleep(std::time::Duration::from_millis(150));
    if is_polkit_agent_running() {
        return;
    }

    // Procura agentes gráficos comuns caso nenhum esteja rodando
    let candidates = [
        "/usr/lib/polkit-kde-authentication-agent-1",
        "/usr/lib/polkit-gnome/polkit-gnome-authentication-agent-1",
        "/usr/libexec/polkit-gnome-authentication-agent-1",
        "/usr/lib/hyprpolkitagent",
        "/usr/lib/polkit-mate-authentication-agent-1",
        "/usr/bin/lxpolkit",
    ];
    for agent in candidates {
        if Path::new(agent).exists() {
            let _ = Command::new(agent)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            std::thread::sleep(std::time::Duration::from_millis(250));
            break;
        }
    }
}

/// Installs (or removes) the helper and its polkit action with one
/// administrator prompt.
pub fn set_helper(install: bool) -> Result<()> {
    // Se já estiver rodando como root (ex: script de instalação ou sudo), instala diretamente
    if unsafe { libc::geteuid() == 0 } {
        use std::os::unix::fs::PermissionsExt;
        if install {
            if let Some(parent) = Path::new(HELPER_PATH).parent() {
                fs::create_dir_all(parent)?;
            }
            if let Some(parent) = Path::new(POLICY_PATH).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(HELPER_PATH, HELPER)?;
            fs::set_permissions(HELPER_PATH, fs::Permissions::from_mode(0o755))?;
            fs::write(POLICY_PATH, POLICY)?;
            fs::set_permissions(POLICY_PATH, fs::Permissions::from_mode(0o644))?;
            let _ = ensure_shim();
        } else {
            let _ = fs::remove_file(HELPER_PATH);
            let _ = fs::remove_file(POLICY_PATH);
            if let Some(parent) = Path::new(HELPER_PATH).parent() {
                let _ = fs::remove_dir(parent);
            }
        }
        return Ok(());
    }

    // Só inicia agente polkit se nenhum estiver rodando na sessão
    ensure_polkit_agent();

    let status = if install {
        let dir = data_dir().join("install");
        fs::create_dir_all(&dir)?;
        let (h, p) = (dir.join("radmin-net"), dir.join("linmin-vpn.policy"));
        fs::write(&h, HELPER)?;
        fs::write(&p, POLICY)?;
        let st = Command::new("pkexec")
            .stdin(Stdio::null())
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
        let _ = ensure_shim();
        st
    } else {
        Command::new("pkexec")
            .stdin(Stdio::null())
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
    config_path().parent().unwrap().with_file_name("autostart").join("linmin-vpn.desktop")
}

fn user_binds_path() -> PathBuf {
    crate::home().join(".config/hypr/user-binds.lua")
}

fn hyprland_conf_path() -> PathBuf {
    crate::home().join(".config/hypr/hyprland.conf")
}

fn is_hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.to_ascii_lowercase().contains("hyprland"))
            .unwrap_or(false)
        || crate::home().join(".config/hypr").is_dir()
}

fn set_hyprland_autostart(on: bool) -> Result<()> {
    if !is_hyprland() {
        return Ok(());
    }

    // 1. Hollow-wired rice / Lua Hyprland provider (usa user-binds.lua)
    let user_binds = user_binds_path();
    let hypr_dir = crate::home().join(".config/hypr");
    if user_binds.is_file() || hypr_dir.join("hyprland.lua").is_file() {
        let content = fs::read_to_string(&user_binds).unwrap_or_default();
        let marker_start = "-- >>> linmin-vpn autostart >>>";
        let marker_end = "-- <<< linmin-vpn autostart <<<";
        let mut lines: Vec<&str> = Vec::new();
        let mut in_block = false;
        for line in content.lines() {
            if line.trim() == marker_start {
                in_block = true;
                continue;
            }
            if line.trim() == marker_end {
                in_block = false;
                continue;
            }
            if !in_block {
                lines.push(line);
            }
        }
        let mut new_content = lines.join("\n");
        if on {
            if !new_content.ends_with('\n') && !new_content.is_empty() {
                new_content.push('\n');
            }
            new_content.push_str(&format!(
                "{marker_start}\nhl.exec_cmd(\"linmin-vpn --tray\")\n{marker_end}\n"
            ));
        }
        if new_content != content {
            fs::write(&user_binds, new_content)?;
        }
        return Ok(());
    }

    // 2. Hyprland padrão com hyprland.conf (fora do rice)
    let conf = hyprland_conf_path();
    if conf.is_file() {
        let content = fs::read_to_string(&conf).unwrap_or_default();
        let marker_start = "# >>> linmin-vpn autostart >>>";
        let marker_end = "# <<< linmin-vpn autostart <<<";
        let mut lines: Vec<&str> = Vec::new();
        let mut in_block = false;
        for line in content.lines() {
            if line.trim() == marker_start {
                in_block = true;
                continue;
            }
            if line.trim() == marker_end {
                in_block = false;
                continue;
            }
            if !in_block {
                lines.push(line);
            }
        }
        let mut new_content = lines.join("\n");
        if on {
            if !new_content.ends_with('\n') && !new_content.is_empty() {
                new_content.push('\n');
            }
            new_content.push_str(&format!(
                "{marker_start}\nexec-once = linmin-vpn --tray\n{marker_end}\n"
            ));
        }
        if new_content != content {
            fs::write(&conf, new_content)?;
        }
    }

    Ok(())
}

pub fn set_autostart(on: bool) -> Result<()> {
    let p = autostart_path();
    if !on {
        let _ = fs::remove_file(p);
    } else {
        let exe = std::env::current_exe()?;
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(
            p,
            format!(
                "[Desktop Entry]\nType=Application\nName=Linmin VPN\nExec=\"{}\" --tray\nIcon=network-vpn\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            ),
        )?;
    }
    let _ = set_hyprland_autostart(on);
    Ok(())
}
