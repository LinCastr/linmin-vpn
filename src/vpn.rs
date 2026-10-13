//! Runs the radmin-vpn-linux AppImage in the background and reports its state.
//!
//! The AppImage is started with `--no-ui` (service only), detached from any
//! terminal, with a `sudo` shim first in its PATH so its root commands go to the
//! polkit helper instead of a password prompt. Radmin's own window is opened on
//! demand with the same environment the launcher uses, and closing it never
//! takes the tunnel down.

use std::ffi::CStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub const DEV: &str = "radminvpn0";

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Off,
    /// Launcher running, not connected yet; the text is its latest step.
    Starting(String),
    On,
    Stopping,
    /// The launcher stopped on its own; the text is its last useful line.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub phase: Phase,
    pub ip: Option<String>,
    pub peers: Vec<String>,
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".local/share"))
        .join("linmin-vpn")
}

/// Where the upstream launcher keeps its Wine prefix and log.
fn upstream_dir() -> PathBuf {
    data_dir().with_file_name("radmin-vpn-linux")
}

pub fn log_path() -> PathBuf {
    upstream_dir().join("run.log")
}

fn pid_file() -> PathBuf {
    data_dir().join("launcher.pid")
}

struct Runner {
    child: Option<Child>,
    pid: Option<i32>,
    stop_requested: Option<Instant>,
    failed: Option<String>,
}

static RUNNER: Mutex<Runner> =
    Mutex::new(Runner { child: None, pid: None, stop_requested: None, failed: None });

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn cmdline(pid: i32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Processes whose command line contains `needle`.
pub fn find_procs(needle: &str) -> Vec<i32> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    let me = std::process::id() as i32;
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid != me && cmdline(pid).contains(needle))
        .collect()
}

/// Picks up a launcher started by an earlier run of this app.
pub fn adopt() {
    let mut r = RUNNER.lock().unwrap();
    if r.pid.is_some() {
        return;
    }
    if let Some(pid) = fs::read_to_string(pid_file()).ok().and_then(|s| s.trim().parse().ok()) {
        if alive(pid) && cmdline(pid).contains("AppImage") {
            r.pid = Some(pid);
        }
    }
}

pub fn running() -> bool {
    let mut r = RUNNER.lock().unwrap();
    reap(&mut r);
    r.pid.is_some()
}

/// Notices a launcher that exited and records why.
fn reap(r: &mut Runner) {
    let Some(pid) = r.pid else { return };
    let exited = match r.child.as_mut() {
        Some(c) => !matches!(c.try_wait(), Ok(None)),
        None => !alive(pid),
    };
    if !exited {
        return;
    }
    r.child = None;
    r.pid = None;
    let _ = fs::remove_file(pid_file());
    if r.stop_requested.take().is_none() {
        r.failed = Some(last_log_line(true).unwrap_or_default());
    }
}

pub fn start(appimage: &Path, use_helper: bool) -> Result<()> {
    let mut r = RUNNER.lock().unwrap();
    reap(&mut r);
    if r.pid.is_some() {
        return Ok(());
    }
    let dir = data_dir();
    fs::create_dir_all(&dir)?;
    let mut path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    if use_helper {
        let _ = crate::setup::ensure_shim();
        let shim_dir = crate::setup::shim_path().parent().unwrap().to_path_buf();
        path = format!("{}:{path}", shim_dir.display());
    }
    let child = Command::new(appimage)
        .arg("--no-ui")
        // Without this the AppRun reopens itself inside a terminal emulator.
        .env("RADMIN_IN_TERM", "1")
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("{}", appimage.display()))?;
    let pid = child.id() as i32;
    let _ = fs::write(pid_file(), pid.to_string());
    r.child = Some(child);
    r.pid = Some(pid);
    r.failed = None;
    r.stop_requested = None;
    Ok(())
}

/// Asks the launcher to shut down the way Ctrl+C would: its cleanup kills Wine
/// and removes the interface. Escalates if it does not finish in time.
pub fn stop() {
    let mut r = RUNNER.lock().unwrap();
    reap(&mut r);
    let Some(pid) = r.pid else { return };
    if r.stop_requested.is_none() {
        r.stop_requested = Some(Instant::now());
    }
    // Only run.sh gets the signal: interrupting `tee` too would break its pipe
    // in the middle of the cleanup.
    let scripts: Vec<i32> = find_procs("usr/bin/run.sh")
        .into_iter()
        .filter(|&p| unsafe { libc::getpgid(p) } == pid)
        .collect();
    unsafe {
        if scripts.is_empty() {
            libc::kill(-pid, libc::SIGINT);
        } else {
            for p in scripts {
                libc::kill(p, libc::SIGINT);
            }
        }
    }
}

/// Called periodically while stopping: forces the end after a grace period.
fn enforce_stop(r: &mut Runner) {
    let (Some(pid), Some(t)) = (r.pid, r.stop_requested) else { return };
    if t.elapsed() > Duration::from_secs(20) {
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        for p in find_procs("wineserver") {
            if fs::read_to_string(format!("/proc/{p}/environ"))
                .is_ok_and(|e| e.contains("radmin-vpn-linux"))
            {
                unsafe { libc::kill(p, libc::SIGKILL) };
            }
        }
    }
}

pub fn status() -> Status {
    let (pid, stopping, failed) = {
        let mut r = RUNNER.lock().unwrap();
        reap(&mut r);
        enforce_stop(&mut r);
        (r.pid, r.stop_requested.is_some(), r.failed.clone())
    };
    // A leftover interface from an interrupted run means nothing while off.
    let ip = pid.and_then(|_| interface_ip());
    let phase = match pid {
        None => match failed {
            Some(msg) => Phase::Failed(msg),
            None => Phase::Off,
        },
        Some(_) if stopping => Phase::Stopping,
        Some(_) => {
            if ip.is_some() && log_says_running() {
                Phase::On
            } else {
                Phase::Starting(last_log_line(false).unwrap_or_default())
            }
        }
    };
    let peers = if phase == Phase::On { peers() } else { vec![] };
    Status { phase, ip, peers }
}

pub fn clear_failure() {
    RUNNER.lock().unwrap().failed = None;
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn log_lines() -> Vec<String> {
    let Ok(f) = fs::File::open(log_path()) else { return vec![] };
    BufReader::new(f).lines().map_while(Result::ok).map(|l| strip_ansi(&l)).collect()
}

fn log_says_running() -> bool {
    log_lines().iter().any(|l| l.contains("Radmin VPN running"))
}

/// Latest launcher step ("[*] ..." / "[+] ..."), or with `errors` the last
/// error-looking line, without the marker.
fn last_log_line(errors: bool) -> Option<String> {
    let lines = log_lines();
    let pick = |l: &&String| {
        let t = l.trim_start();
        if errors {
            t.starts_with("[-]") || t.starts_with("[!]") || t.contains("rror")
        } else {
            t.starts_with("[*]") || t.starts_with("[+]")
        }
    };
    lines
        .iter()
        .rev()
        .find(pick)
        .or_else(|| lines.iter().rev().find(|l| !l.trim().is_empty()))
        .map(|l| l.trim().trim_start_matches(|c| "[*+-!] ".contains(c)).trim().to_string())
}

/// IPv4 address of the VPN interface.
pub fn interface_ip() -> Option<String> {
    let mut found = None;
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET
                && CStr::from_ptr(ifa.ifa_name).to_bytes() == DEV.as_bytes()
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                found = Some(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).to_string());
                break;
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

/// Radmin addresses the kernel has resolved on the VPN interface: the peers
/// this machine has talked to. Radmin keeps names to itself, so only IPs.
pub fn peers() -> Vec<String> {
    let Ok(arp) = fs::read_to_string("/proc/net/arp") else { return vec![] };
    let mut out: Vec<String> = arp
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 6 && f[5] == DEV && f[2] == "0x2" && f[0].starts_with("26."))
                .then(|| f[0].to_string())
        })
        .collect();
    out.sort_by_key(|ip| ip.parse::<std::net::Ipv4Addr>().map(u32::from).unwrap_or(0));
    out.dedup();
    out
}

/// Opens Radmin's own window (networks, rooms, chat). Needs the service running.
/// A window that is already open, or hidden because Radmin "minimised to tray"
/// (there is no Wine tray here), is closed and opened again.
/// The launcher's Wine environment, recovered from the running service so other
/// Wine processes (the GUI window, the pipe relay) share its prefix exactly.
pub struct WineEnv {
    /// All of the launcher's env vars except `LD_PRELOAD` (service-only DNS shim).
    pub vars: Vec<(String, String)>,
    #[allow(dead_code)]
    pub prefix: PathBuf,
    /// The bundled `wine` binary, resolved through the launcher's PATH.
    pub wine: PathBuf,
    /// The Radmin VPN install dir inside the prefix.
    pub radmin_dir: PathBuf,
}

/// Reads the running service's environment. Errors if the service is not up.
pub fn wine_env() -> Result<WineEnv> {
    let Some(script) = find_procs("usr/bin/run.sh").into_iter().next() else {
        bail!("service not running");
    };
    let env = fs::read(format!("/proc/{script}/environ"))?;
    let vars: Vec<(String, String)> = env
        .split(|&b| b == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            // The service-only DNS shim must not reach other Wine processes.
            (k != "LD_PRELOAD").then(|| (k.to_string(), v.to_string()))
        })
        .collect();
    let get = |key: &str| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let prefix = PathBuf::from(get("WINEPREFIX").context("WINEPREFIX")?);
    let wine = get("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|d| Path::new(d).join("wine"))
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("wine"));
    let radmin_dir = prefix.join("drive_c/Program Files (x86)/Radmin VPN");
    Ok(WineEnv { vars, prefix, wine, radmin_dir })
}

pub fn open_radmin_window() -> Result<()> {
    let env = wine_env()?;
    for p in find_procs("RvRvpnGui.exe") {
        unsafe { libc::kill(p, libc::SIGTERM) };
    }
    std::thread::sleep(Duration::from_millis(300));
    let mut cmd = Command::new(&env.wine);
    cmd.env_clear().envs(env.vars.iter().map(|(k, v)| (k, v)));
    cmd.arg("RvRvpnGui.exe")
        .current_dir(&env.radmin_dir)
        .env("WINEDEBUG", "-all")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("GALLIUM_DRIVER", "llvmpipe")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = cmd.spawn().context("wine RvRvpnGui.exe")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
