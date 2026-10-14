//! linmin-vpn: a small native front end for Radmin VPN on Linux.
//!
//! It runs the radmin-vpn-linux AppImage without a terminal, keeps an icon in
//! the system tray (StatusNotifierItem) and, after a one-time setup, never asks
//! for the sudo password. Radmin's own window is still used for managing
//! networks, since its service protocol is not public.

mod i18n;
mod msg;
mod pipe;
mod proto;
mod setup;
mod state;
mod theme;
mod update;
mod vpn;

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("linmin-vpn.sock")
}

static QUITTING: AtomicBool = AtomicBool::new(false);
/// Set while the user has opened Radmin's own Wine window. The service accepts
/// one GUI client on the pipe, so the native pipe client stands down until the
/// VPN is toggled off again.
static WINE_GUI: AtomicBool = AtomicBool::new(false);

// Tray -------------------------------------------------------------------------

struct Tray {
    ui: slint::Weak<AppWindow>,
    phase: vpn::Phase,
    ip: String,
}

impl Tray {
    fn on_ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                f(ui);
            }
        });
    }
}

fn load_tray_icon() -> Option<ksni::Icon> {
    let bytes = include_bytes!("../data/linmin-vpn.png");
    let decoder = png::Decoder::new(&bytes[..]);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    // ksni expects ARGB32 in network byte order (A, R, G, B)
    let mut argb = Vec::with_capacity((info.width * info.height * 4) as usize);
    for chunk in buf[..info.buffer_size()].chunks_exact(4) {
        argb.push(chunk[3]); // A
        argb.push(chunk[0]); // R
        argb.push(chunk[1]); // G
        argb.push(chunk[2]); // B
    }
    Some(ksni::Icon {
        width: info.width as i32,
        height: info.height as i32,
        data: argb,
    })
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "linmin-vpn".into()
    }
    fn title(&self) -> String {
        "Linmin VPN".into()
    }
    fn icon_name(&self) -> String {
        "linmin-vpn".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static ICON: std::sync::OnceLock<Option<ksni::Icon>> = std::sync::OnceLock::new();
        let icon = ICON.get_or_init(load_tray_icon);
        icon.clone().into_iter().collect()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Linmin VPN".into(),
            description: phase_text(&self.phase, &self.ip),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.on_ui(|ui| {
            let w = ui.window();
            if w.is_visible() {
                let _ = w.hide();
            } else {
                let _ = w.show();
            }
        });
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let active = matches!(self.phase, vpn::Phase::On | vpn::Phase::Starting(_));
        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: phase_text(&self.phase, &self.ip),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: if active { tr!("Turn off", "Desligar") } else { tr!("Turn on", "Ligar") },
                enabled: self.phase != vpn::Phase::Stopping,
                activate: Box::new(|t: &mut Self| t.on_ui(|ui| ui.invoke_toggle_vpn())),
                ..Default::default()
            }
            .into(),
        ];
        if !self.ip.is_empty() {
            let ip = self.ip.clone();
            items.push(
                StandardItem {
                    label: tr!("Copy address", "Copiar endereço"),
                    activate: Box::new(move |_: &mut Self| copy_text(&ip)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
            StandardItem {
                label: tr!("Manage networks", "Gerenciar redes"),
                enabled: self.phase == vpn::Phase::On,
                activate: Box::new(|t: &mut Self| t.on_ui(|ui| ui.invoke_open_radmin())),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tr!("Open window", "Abrir janela"),
                activate: Box::new(|t: &mut Self| {
                    t.on_ui(|ui| {
                        let _ = ui.window().show();
                    })
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: tr!("Quit", "Sair"),
                activate: Box::new(|_: &mut Self| {
                    QUITTING.store(true, Ordering::Relaxed);
                    vpn::stop();
                }),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

fn phase_text(p: &vpn::Phase, ip: &str) -> String {
    match p {
        vpn::Phase::Off => tr!("Off", "Desligada"),
        vpn::Phase::Starting(_) => tr!("Connecting…", "Conectando…"),
        vpn::Phase::On => tr!("Connected — {ip}", "Conectada — {ip}"),
        vpn::Phase::Stopping => tr!("Turning off…", "Desligando…"),
        vpn::Phase::Failed(_) => tr!("Stopped unexpectedly", "Parou sozinha"),
    }
}

// Pipe client lifecycle -------------------------------------------------------

/// Keeps the native pipe client in step with the current phase and returns a
/// live snapshot, or `None` when there is no live client this tick.
///
/// `code` is the phase code (2 = On). The client is connected lazily when the
/// VPN is up and the Wine window is not in use, dropped when the VPN goes down,
/// and reconnected if the relay dies.
fn live_state(
    cell: &Rc<RefCell<Option<Arc<pipe::Client>>>>,
    code: i32,
) -> Option<state::State> {
    let on = code == 2;
    // The Wine window was closed: take the pipe back.
    if WINE_GUI.load(Ordering::Relaxed) && vpn::find_procs("RvRvpnGui.exe").is_empty() {
        WINE_GUI.store(false, Ordering::Relaxed);
    }
    let wine_gui = WINE_GUI.load(Ordering::Relaxed);

    // Drop the client when it should not be running or has died.
    {
        let mut slot = cell.borrow_mut();
        let dead = slot.as_ref().map(|c| !c.is_alive()).unwrap_or(false);
        if !on || wine_gui || dead {
            slot.take();
        }
    }
    if !on || wine_gui {
        LAST_LIVE.with(|last| last.borrow_mut().take());
        return None;
    }
    // Connect if we have no client yet.
    if cell.borrow().is_none() {
        match pipe::Client::connect_installed() {
            Ok(c) => {
                let c = Arc::new(c);
                pipe::spawn_heartbeat(Arc::clone(&c));
                let _ = c.switch_power(true);
                *cell.borrow_mut() = Some(c);
            }
            // Leave the slot empty; the ARP fallback covers this tick and we
            // retry on the next one.
            Err(_) => return None,
        }
    }
    let client = cell.borrow().as_ref().map(Arc::clone)?;
    let snap = client.snapshot();
    if snap.power == Some(false) {
        let _ = client.switch_power(true);
    }
    // The service only sends the full state (event 34) right after a client
    // connects, and not every later change arrives as an event we parse (a
    // join request, or networks loaded after the client connected while the
    // service was still starting). Reconnecting gets a fresh full state: soon
    // when it came empty, and periodically otherwise.
    let resync_after = if snap.networks.is_empty() {
        PIPE_RESYNC_EMPTY
    } else {
        PIPE_RESYNC_PERIOD
    };
    if client.age() >= resync_after {
        cell.borrow_mut().take();
    }
    LAST_LIVE.with(|last| {
        let mut last = last.borrow_mut();
        if !snap.networks.is_empty() {
            *last = Some(snap.clone());
            return Some(snap);
        }
        // A fresh client has not received its full state yet: keep showing
        // the previous one instead of flashing the ARP fallback.
        match last.as_ref() {
            Some(prev) if client.age() < PIPE_RESYNC_EMPTY => Some(prev.clone()),
            _ => Some(snap),
        }
    })
}

/// How long a pipe client may report no networks before it is reconnected.
const PIPE_RESYNC_EMPTY: Duration = Duration::from_secs(4);
/// How often a healthy pipe client is reconnected to refresh the full state.
const PIPE_RESYNC_PERIOD: Duration = Duration::from_secs(20);

thread_local! {
    /// Last live snapshot that had networks, shown while a new client syncs.
    static LAST_LIVE: RefCell<Option<state::State>> = const { RefCell::new(None) };
}

/// Converts a state::Node into the Slint PeerInfo struct.
fn peer_info(node: &state::Node) -> PeerInfo {
    let ip = node.ipv4.map(|a| a.to_string()).unwrap_or_default();
    let alias = if node.alias.is_empty() {
        ip.clone()
    } else {
        node.alias.clone()
    };
    let latency = if node.online && node.rtt_ms.map_or(false, |ms| ms > 0) {
        node.rtt_ms.map(|ms| format!("{ms} ms")).unwrap_or_default()
    } else {
        String::new()
    };
    PeerInfo {
        alias: alias.into(),
        ip: ip.into(),
        online: node.online,
        latency: latency.into(),
        id: node.id.to_string().into(),
        applicant: node.is_applicant(),
    }
}

/// Builds the network rows for the UI from a live snapshot, or a single
/// fallback "network" from the ARP addresses when there is no live data yet.
fn network_rows(live: Option<&state::State>, arp: &[String]) -> Vec<NetInfo> {
    if let Some(st) = live.filter(|s| !s.networks.is_empty()) {
        let cache = setup::load_networks_cache();
        return st
            .networks
            .iter()
            .map(|net| {
                let peers: Vec<PeerInfo> = net.nodes.iter().map(peer_info).collect();
                let id_str = hex_guid(&net.guid);
                let (allow_pw, block_new) = if net.has_prop_a || net.has_prop_c {
                    (net.allow_without_password, net.block_new_members)
                } else if let Some(saved) = cache.get(&id_str) {
                    (saved.allow_without_password, saved.block_new_members)
                } else {
                    (net.allow_without_password, net.block_new_members)
                };
                NetInfo {
                    name: net.name.clone().into(),
                    id: id_str.into(),
                    online: peers.iter().filter(|p| p.online).count() as i32,
                    total: peers.len() as i32,
                    collapsed: false,
                    allow_without_password: allow_pw,
                    block_new_members: block_new,
                    can_delete: net.can_delete(),
                    last_admin: net.is_last_admin(),
                    peers: ModelRc::from(Rc::new(VecModel::from(peers))),
                }
            })
            .collect();
    }
    if arp.is_empty() {
        return Vec::new();
    }
    // No live grouping: show the kernel's ARP addresses under one headless row,
    // with an empty id so no Leave button appears.
    let peers: Vec<PeerInfo> = arp
        .iter()
        .map(|ip| PeerInfo {
            alias: ip.clone().into(),
            ip: ip.clone().into(),
            online: true,
            latency: SharedString::new(),
            id: SharedString::new(),
            applicant: false,
        })
        .collect();
    vec![NetInfo {
        name: tr!("Seen on the network", "Vistos na rede").into(),
        id: SharedString::new(),
        online: arp.len() as i32,
        total: arp.len() as i32,
        collapsed: false,
        allow_without_password: false,
        block_new_members: false,
        can_delete: false,
        last_admin: false,
        peers: ModelRc::from(Rc::new(VecModel::from(peers))),
    }]
}

/// A cheap content signature of the network rows, to skip model rebuilds (which
/// would reset hover/scroll) when nothing changed.
fn rows_signature(rows: &[NetInfo]) -> String {
    let mut s = String::new();
    for r in rows {
        s.push_str(r.id.as_str());
        s.push('|');
        s.push_str(r.name.as_str());
        s.push('|');
        s.push_str(if r.allow_without_password { "1|" } else { "0|" });
        s.push_str(if r.block_new_members { "1|" } else { "0|" });
        s.push_str(if r.can_delete { "1|" } else { "0|" });
        s.push_str(if r.last_admin { "1|" } else { "0|" });
        for p in r.peers.iter() {
            s.push_str(p.alias.as_str());
            s.push(':');
            s.push_str(p.ip.as_str());
            s.push(':');
            s.push(if p.online { '1' } else { '0' });
            s.push(':');
            s.push_str(p.latency.as_str());
            s.push(':');
            s.push(if p.applicant { 'a' } else { 'm' });
            s.push(',');
        }
        s.push(';');
    }
    s
}

/// Lower-case hex of a 16-byte network GUID, as the UI id.
fn hex_guid(g: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in g {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parses a 32-char hex id back into a GUID. Returns None on a malformed id.
fn parse_guid(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut g = [0u8; 16];
    for (i, b) in g.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(g)
}

// Helpers ----------------------------------------------------------------------

fn copy_text(text: &str) {
    for (cmd, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
        if let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return;
        }
    }
}

fn color(c: theme::Rgb) -> slint::Color {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    slint::Color::from_rgb_u8(b(c.0), b(c.1), b(c.2))
}

fn apply_theme(ui: &AppWindow) {
    let palette = theme::find_palette();
    ui.set_wallust_available(palette.is_some());
    let wants_wallust = setup::load().theme == "wallust";
    ui.set_theme_mode(if wants_wallust { 1 } else { 0 });
    let c = palette
        .filter(|_| wants_wallust)
        .and_then(|p| theme::load_palette(&p))
        .map(|p| theme::wallust_colors(&p))
        .unwrap_or_else(theme::default_colors);
    let t = ui.global::<Theme>();
    t.set_bg(color(c.bg));
    t.set_sidebar(color(c.sidebar));
    t.set_surface(color(c.surface));
    t.set_raised(color(c.raised));
    t.set_hover(color(c.hover));
    t.set_line(color(c.line));
    t.set_text(color(c.text));
    t.set_dim(color(c.dim));
    t.set_faint(color(c.faint));
    t.set_accent(color(c.accent));
    t.set_on_accent(color(c.on_accent));
}

fn palette_mtime() -> Option<SystemTime> {
    theme::find_palette().and_then(|p| theme::mtime(&p))
}

/// Development: LINMIN_SNAPSHOT=file.png saves the window as an image and quits.
fn snapshot_mode(ui: &AppWindow) -> bool {
    let Ok(out) = std::env::var("LINMIN_SNAPSHOT") else { return false };
    if std::env::var("LINMIN_SETTINGS").is_ok() {
        ui.set_show_settings(true);
    }
    let weak = ui.as_weak();
    // The pipe client needs a moment to spawn Wine and receive the first state;
    // LINMIN_SNAPSHOT_DELAY (ms) lets a test wait for it.
    let delay = std::env::var("LINMIN_SNAPSHOT_DELAY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1500);
    slint::Timer::single_shot(Duration::from_millis(delay), move || {
        if let Some(ui) = weak.upgrade() {
            // LINMIN_SNAPSHOT_SIZE=WxH checks the layout at another window size.
            if let Some((w, h)) = std::env::var("LINMIN_SNAPSHOT_SIZE").ok().and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
            }) {
                ui.window().dispatch_event(slint::platform::WindowEvent::Resized {
                    size: slint::LogicalSize::new(w, h),
                });
            }
            // LINMIN_SNAPSHOT_CLICK="x,y[,right] ..." clicks before the shot, to
            // check menus and dialogs without touching the user's screen.
            for click in std::env::var("LINMIN_SNAPSHOT_CLICK").unwrap_or_default().split_whitespace() {
                // "type:abc" types text, "key:esc" / "key:enter" presses a key.
                let keys: Option<String> = if let Some(t) = click.strip_prefix("type:") {
                    Some(t.replace("%20", " "))
                } else {
                    match click {
                        "key:esc" => Some(SharedString::from(slint::platform::Key::Escape).to_string()),
                        "key:enter" => Some(SharedString::from(slint::platform::Key::Return).to_string()),
                        _ => None,
                    }
                };
                if let Some(keys) = keys {
                    for ch in keys.chars() {
                        let text: SharedString = ch.to_string().into();
                        ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
                        ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                    }
                    continue;
                }
                let parts: Vec<&str> = click.split(',').collect();
                let (Some(Ok(x)), Some(Ok(y))) = (parts.first().map(|s| s.parse::<f32>()), parts.get(1).map(|s| s.parse::<f32>())) else {
                    continue;
                };
                let button = if parts.get(2) == Some(&"right") {
                    slint::platform::PointerEventButton::Right
                } else {
                    slint::platform::PointerEventButton::Left
                };
                let position = slint::LogicalPosition::new(x, y);
                let w = ui.window();
                w.dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
                w.dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
                w.dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
            }
        }
        // LINMIN_SNAPSHOT_AFTER (ms) waits after the clicks, e.g. for a model
        // rebuild by the status timer.
        let after = std::env::var("LINMIN_SNAPSHOT_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        slint::Timer::single_shot(Duration::from_millis(after), move || {
            if let Some(ui) = weak.upgrade() {
                if let Ok(img) = ui.window().take_snapshot() {
                    let file = std::fs::File::create(&out).expect("snapshot file");
                    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width(), img.height());
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    let _ = enc.write_header().and_then(|mut w| w.write_image_data(img.as_bytes()));
                }
            }
            let _ = slint::quit_event_loop();
        });
    });
    true
}

fn refresh_setup(ui: &AppWindow) {
    let cfg = setup::load();
    ui.set_has_appimage(setup::appimage(&cfg).is_some());
    ui.set_has_local_copy(setup::appimage(&cfg).is_none() && setup::local_copy().is_some());
    ui.set_helper_state(match setup::helper_state() {
        setup::HelperState::Missing => 0,
        setup::HelperState::Outdated => 1,
        setup::HelperState::Ok => 2,
    });
    ui.set_autostart(cfg.autostart);
    ui.set_connect_on_login(cfg.connect_on_login);
    ui.set_language(match cfg.language.as_str() {
        "en" => 1,
        "pt" => 2,
        _ => 0,
    });
}

fn toast(ui: &AppWindow, text: String) {
    show_toast(ui, text, false);
}

fn toast_error(ui: &AppWindow, text: String) {
    show_toast(ui, text, true);
}

fn show_toast(ui: &AppWindow, text: String, error: bool) {
    let text: SharedString = text.into();
    ui.set_toast(text.clone());
    ui.set_toast_error(error);
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(3), move || {
        // Only clear our own message; a newer toast keeps its full time.
        if let Some(ui) = weak.upgrade().filter(|ui| ui.get_toast() == text) {
            ui.set_toast(SharedString::new());
        }
    });
}

fn start_vpn(ui: &AppWindow) {
    vpn::clear_failure();
    let cfg = setup::load();
    let Some(app) = setup::appimage(&cfg) else { return };
    if setup::helper_state() != setup::HelperState::Ok {
        ui.set_busy(true);
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let ok = setup::set_helper(true).is_ok();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_busy(false);
                    refresh_setup(&ui);
                    if ok {
                        let helper = setup::helper_state() == setup::HelperState::Ok;
                        if let Err(e) = vpn::start(&app, helper) {
                            toast_error(&ui, tr!("Could not start: {e}", "Não foi possível ligar: {e}"));
                        }
                    } else {
                        toast(&ui, tr!("Cancelled", "Cancelado"));
                    }
                }
            });
        });
        return;
    }
    let helper = true;
    if let Err(e) = vpn::start(&app, helper) {
        toast_error(ui, tr!("Could not start: {e}", "Não foi possível ligar: {e}"));
    }
}

/// `linmin-vpn up|down|status|install-helper` for scripts and testing.
fn cli(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    if !["up", "down", "status", "pipe-dump", "install-helper", "setup-helper", "remove-helper"].contains(&cmd) {
        return None;
    }
    i18n::apply_lang(&setup::load().language);
    vpn::adopt();
    match cmd {
        // Live test harness: bring up the native pipe client through the relay,
        // ping visible nodes, then print the parsed state. Not a user command.
        "pipe-dump" => {
            let relay = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| vpn::data_dir().join("rvrelay.exe"));
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(6);
            let client = match pipe::Client::connect(&relay) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("linmin-vpn: pipe connect: {e:#}");
                    return Some(1);
                }
            };
            std::thread::sleep(Duration::from_millis(500));
            let _ = client.ping_visible_nodes();
            // Optional write probe: `rename:<name>` sends ChangeAlias so the
            // reply (result 0x310) can be observed under LINMIN_PIPE_RAW.
            if args.iter().any(|a| a == "power:on" || a == "power:1") {
                if let Err(e) = client.switch_power(true) {
                    eprintln!("linmin-vpn: switch_power: {e:#}");
                }
            }
            if let Some(rest) = args.get(3).and_then(|s| s.strip_prefix("rename:")) {
                if let Err(e) = client.change_alias(rest) {
                    eprintln!("linmin-vpn: rename: {e:#}");
                }
            }
            let deadline = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < deadline && client.is_alive() {
                std::thread::sleep(Duration::from_millis(250));
            }
            let st = client.snapshot();
            let nets: Vec<_> = st
                .networks
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "name": n.name,
                        "members": n.members,
                        "nodes": n.nodes.iter().map(|x| serde_json::json!({
                            "id": x.id, "alias": x.alias,
                            "ip": x.ipv4.map(|a| a.to_string()),
                            "online": x.online, "rtt_ms": x.rtt_ms,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "alive": client.is_alive(),
                    "connect_state": st.connect_state,
                    "networks": nets,
                })
            );
            Some(0)
        }
        "up" => {
            let cfg = setup::load();
            let Some(app) = setup::appimage(&cfg) else {
                eprintln!("linmin-vpn: AppImage not installed (open the app to install it)");
                return Some(1);
            };
            if setup::helper_state() != setup::HelperState::Ok {
                eprintln!("linmin-vpn: helper is missing or outdated. Run 'linmin-vpn install-helper' to avoid sudo password prompts.");
            }
            if let Err(e) = vpn::start(&app, setup::helper_state() == setup::HelperState::Ok) {
                eprintln!("linmin-vpn: {e:#}");
                return Some(1);
            }
            Some(0)
        }
        "install-helper" | "setup-helper" => {
            match setup::set_helper(true) {
                Ok(_) => {
                    println!("linmin-vpn: helper and polkit policy installed successfully");
                    Some(0)
                }
                Err(e) => {
                    eprintln!("linmin-vpn: failed to install helper: {e:#}");
                    Some(1)
                }
            }
        }
        "remove-helper" => {
            match setup::set_helper(false) {
                Ok(_) => {
                    println!("linmin-vpn: helper and polkit policy removed");
                    Some(0)
                }
                Err(e) => {
                    eprintln!("linmin-vpn: failed to remove helper: {e:#}");
                    Some(1)
                }
            }
        }
        "down" => {
            vpn::stop();
            for _ in 0..30 {
                if !vpn::running() {
                    return Some(0);
                }
                vpn::status();
                std::thread::sleep(Duration::from_secs(1));
            }
            Some(1)
        }
        _ => {
            let st = vpn::status();
            let (phase, detail) = match &st.phase {
                vpn::Phase::Off => ("off", String::new()),
                vpn::Phase::Starting(s) => ("starting", s.clone()),
                vpn::Phase::On => ("on", String::new()),
                vpn::Phase::Stopping => ("stopping", String::new()),
                vpn::Phase::Failed(s) => ("failed", s.clone()),
            };
            println!(
                "{}",
                serde_json::json!({ "phase": phase, "detail": detail, "ip": st.ip, "peers": st.peers })
            );
            Some(0)
        }
    }
}

fn init_window() -> AppWindow {
    let start = std::time::Instant::now();
    loop {
        // No boot inicial, o processo pode subir milissegundos antes das variáveis
        // WAYLAND_DISPLAY/DISPLAY serem propagadas. Procuramos o socket ativo.
        if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
            if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
                if let Ok(entries) = std::fs::read_dir(&runtime) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let s = name.to_string_lossy();
                        if s.starts_with("wayland-") && !s.ends_with(".lock") {
                            unsafe {
                                std::env::set_var("WAYLAND_DISPLAY", s.as_ref());
                            }
                            break;
                        }
                    }
                }
            }
        }
        match AppWindow::new() {
            Ok(ui) => return ui,
            Err(_e) if start.elapsed() < std::time::Duration::from_secs(6) => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => panic!("window: Could not initialize backend: {e}"),
        }
    }
}

// Main -------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tray_only = args.iter().any(|a| a == "--tray");
    if let Some(code) = cli(&args) {
        std::process::exit(code);
    }
    let snapshot = std::env::var("LINMIN_SNAPSHOT").is_ok();

    // One instance: a second launch just shows the first one's window.
    let sock = socket_path();
    if !snapshot && let Ok(mut s) = UnixStream::connect(&sock) {
        let _ = s.write_all(if tray_only { b"noop" } else { b"show" });
        return;
    }
    let listener = if snapshot { None } else { let _ = std::fs::remove_file(&sock); UnixListener::bind(&sock).ok() };

    let ui = init_window();
    // Only works once the platform exists (AppWindow::new creates it).
    let _ = slint::set_xdg_app_id(std::env::var("LINMIN_APP_ID").unwrap_or_else(|_| "linmin-vpn".into()));
    i18n::apply(&setup::load().language);
    apply_theme(&ui);
    refresh_setup(&ui);
    if setup::load().autostart {
        let _ = setup::set_autostart(true);
    }
    ui.set_current_version(update::CURRENT_VERSION.into());
    if !snapshot {
        let upd_weak = ui.as_weak();
        std::thread::spawn(move || {
            if let Ok(Some(tag)) = update::check_for_update() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = upd_weak.upgrade() {
                        ui.set_update_available(true);
                        ui.set_available_version(tag.into());
                    }
                });
            }
        });
    }
    if std::env::var("LINMIN_TEST_UPDATE").is_ok() {
        ui.set_update_available(true);
        ui.set_available_version("v0.1.2".into());
    }
    if std::env::var("LINMIN_TEST_UPDATE_DIALOG").is_ok() {
        ui.set_update_available(true);
        ui.set_available_version("v0.1.2".into());
        ui.set_show_update_dialog(true);
    }
    vpn::adopt();

    let tray = (!snapshot).then(|| ksni::blocking::TrayMethods::spawn(Tray {
        ui: ui.as_weak(),
        phase: vpn::Phase::Off,
        ip: String::new(),
    })
    .ok()).flatten();
    // Without a tray to come back from, closing the window has to quit.
    let has_tray = tray.is_some();
    ui.window().on_close_requested(move || {
        if !has_tray {
            QUITTING.store(true, Ordering::Relaxed);
            vpn::stop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    if let Some(listener) = listener {
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut msg = String::new();
                let _ = s.read_to_string(&mut msg);
                if msg == "show" {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let _ = ui.window().show();
                        }
                    });
                }
            }
        });
    }

    // The native pipe client, alive only while the VPN is up and the Wine
    // window is not in use. Shared between the open-radmin callback and the
    // status timer, both of which run on the event-loop thread.
    let client_cell: Rc<RefCell<Option<Arc<pipe::Client>>>> = Rc::new(RefCell::new(None));

    // Callbacks ----------------------------------------------------------------
    {
        let weak = ui.as_weak();
        ui.on_toggle_vpn(move || {
            let ui = weak.unwrap();
            match vpn::status().phase {
                vpn::Phase::On | vpn::Phase::Starting(_) => vpn::stop(),
                vpn::Phase::Stopping => {}
                _ => {
                    // A fresh connection drives the native pipe client, not the
                    // Wine window from a previous session.
                    WINE_GUI.store(false, Ordering::Relaxed);
                    start_vpn(&ui);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_open_radmin(move || {
            // Free the pipe for the Wine GUI: stand the native client down first.
            WINE_GUI.store(true, Ordering::Relaxed);
            client_cell.borrow_mut().take();
            if let Err(e) = vpn::open_radmin_window() {
                toast_error(&weak.unwrap(), tr!("Could not open: {e}", "Não foi possível abrir: {e}"));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_create_network(move |name, password, allow_without_password| {
            let ui = weak.unwrap();
            let fail = |msg: String| {
                ui.set_dialog_error(msg.into());
                false
            };
            let name = name.trim();
            if name.is_empty() {
                return fail(tr!("Network name is required", "Nome da rede é obrigatório"));
            }
            if !allow_without_password && password.chars().count() < 6 {
                return fail(tr!(
                    "Password must have at least 6 characters",
                    "A senha precisa ter pelo menos 6 caracteres"
                ));
            }
            if allow_without_password && !password.is_empty() && password.chars().count() < 6 {
                return fail(tr!(
                    "Password must have at least 6 characters",
                    "A senha precisa ter pelo menos 6 caracteres"
                ));
            }
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                return fail(tr!("Not connected", "Sem conexão"));
            };
            match client.create_network(name, &password, allow_without_password) {
                Ok(()) => true,
                Err(e) => fail(tr!(
                    "Could not create network: {e}",
                    "Não foi possível criar rede: {e}"
                )),
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_join_network(move |name, password| {
            let ui = weak.unwrap();
            let fail = |msg: String| {
                ui.set_dialog_error(msg.into());
                false
            };
            let name = name.trim();
            if name.is_empty() {
                return fail(tr!("Network name is required", "Nome da rede é obrigatório"));
            }
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                return fail(tr!("Not connected", "Sem conexão"));
            };
            match client.join_network(name, &password) {
                Ok(()) => true,
                Err(e) => fail(tr!(
                    "Could not join network: {e}",
                    "Não foi possível entrar na rede: {e}"
                )),
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_update_network(
            move |id, name, allow_without_password, block_new_members, new_password| {
                let ui = weak.unwrap();
                let fail = |msg: String| {
                    ui.set_dialog_error(msg.into());
                    false
                };
                let name = name.trim();
                if name.is_empty() {
                    return fail(tr!("Network name is required", "Nome da rede é obrigatório"));
                }
                let Some(guid) = parse_guid(&id) else {
                    return fail(tr!("Invalid network id", "ID de rede inválido"));
                };
                let new_pw = if new_password.is_empty() {
                    None
                } else {
                    if new_password.chars().count() < 6 {
                        return fail(tr!(
                            "Password must have at least 6 characters",
                            "A senha precisa ter pelo menos 6 caracteres"
                        ));
                    }
                    Some(new_password.as_str())
                };
                let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                    return fail(tr!("Not connected", "Sem conexão"));
                };
                match client.update_network(
                    guid,
                    name,
                    allow_without_password,
                    block_new_members,
                    new_pw,
                ) {
                    Ok(()) => {
                        setup::save_network_cache(&id, allow_without_password, block_new_members);
                        true
                    }
                    Err(e) => fail(tr!(
                        "Could not update network: {e}",
                        "Não foi possível atualizar a rede: {e}"
                    )),
                }
            },
        );
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_approve_join(move |net_id, node_id| {
            let ui = weak.unwrap();
            let (Some(guid), Ok(node)) = (parse_guid(&net_id), node_id.parse::<u64>()) else {
                return;
            };
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                toast_error(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            match client.approve_join(guid, node) {
                Ok(()) => toast(&ui, tr!("Approval sent", "Aprovação enviada")),
                Err(e) => toast_error(
                    &ui,
                    tr!("Could not approve: {e}", "Não foi possível aprovar: {e}"),
                ),
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_leave_network(move |id| {
            let ui = weak.unwrap();
            let Some(guid) = parse_guid(&id) else { return };
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                toast_error(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            if let Err(e) = client.leave_network(guid) {
                toast_error(&ui, tr!("Could not leave: {e}", "Não foi possível sair: {e}"));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_delete_network(move |id| {
            let ui = weak.unwrap();
            let Some(guid) = parse_guid(&id) else { return };
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                toast_error(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            if let Err(e) = client.delete_network(guid) {
                toast_error(
                    &ui,
                    tr!(
                        "Could not delete network: {e}",
                        "Não foi possível excluir a rede: {e}"
                    ),
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_ping_peer(move |ip| {
            let ui = weak.unwrap();
            let ip_str = ip.to_string();
            toast(&ui, format!("Ping {ip_str}…"));
            let weak_thread = ui.as_weak();
            std::thread::spawn(move || {
                let out = Command::new("ping")
                    .args(["-c", "1", "-W", "2", &ip_str])
                    .output();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_thread.upgrade() {
                        if let Ok(res) = out {
                            if res.status.success() {
                                let stdout = String::from_utf8_lossy(&res.stdout);
                                if let Some(t) = stdout
                                    .lines()
                                    .find(|l| l.contains("time="))
                                    .and_then(|l| l.split("time=").nth(1))
                                {
                                    toast(&ui, format!("{ip_str} · {t}"));
                                    return;
                                }
                                toast(&ui, tr!("{} is online", "{} está online", ip_str));
                            } else {
                                toast_error(&ui, tr!("{} did not respond to ping", "{} não respondeu ao ping", ip_str));
                            }
                        }
                    }
                });
            });
        });
    }
    {
        ui.on_quit_app(|| {
            QUITTING.store(true, Ordering::Relaxed);
            let _ = slint::quit_event_loop();
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_rename_self(move |name| {
            let ui = weak.unwrap();
            let fail = |msg: String| {
                ui.set_dialog_error(msg.into());
                false
            };
            let name = name.trim();
            if name.is_empty() {
                return fail(tr!("Name is required", "O nome é obrigatório"));
            }
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                return fail(tr!("Not connected", "Sem conexão"));
            };
            match client.change_alias(name) {
                Ok(()) => {
                    ui.set_self_alias(name.into());
                    true
                }
                Err(e) => fail(tr!("Could not rename: {e}", "Não foi possível renomear: {e}")),
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_copy(move |text| {
            copy_text(&text);
            toast(&weak.unwrap(), tr!("Copied {text}", "{text} copiado"));
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_download(move || {
            let ui = weak.unwrap();
            ui.set_downloading(true);
            ui.set_download_progress(0.0);
            ui.set_download_text(tr!("Preparing…", "Preparando…").into());
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let w = weak.clone();
                let result = setup::fetch_appimage(move |done, total| {
                    let w = w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let mb = |b: u64| b as f64 / 1_048_576.0;
                            if total > 0 {
                                ui.set_download_progress(done as f32 / total as f32);
                            }
                            ui.set_download_text(
                                tr!(
                                    "Downloading… {:.0} of {:.0} MB",
                                    "Baixando… {:.0} de {:.0} MB",
                                    mb(done),
                                    mb(total)
                                )
                                .into(),
                            );
                        }
                    });
                });
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_downloading(false);
                        if let Err(e) = result {
                            toast_error(&ui, tr!("Download failed: {e}", "O download falhou: {e}"));
                        }
                        refresh_setup(&ui);
                    }
                });
            });
        });
    }
    for install in [true, false] {
        let weak = ui.as_weak();
        let cb = move || {
            let ui = weak.unwrap();
            ui.set_busy(true);
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let result = setup::set_helper(install);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_busy(false);
                        if result.is_err() {
                            toast(&ui, tr!("Cancelled", "Cancelado"));
                        }
                        refresh_setup(&ui);
                    }
                });
            });
        };
        if install {
            ui.on_install_helper(cb);
        } else {
            ui.on_remove_helper(cb);
        }
    }
    {
        let weak = ui.as_weak();
        ui.on_set_theme(move |i| {
            let mut c = setup::load();
            c.theme = if i == 1 { "wallust" } else { "default" }.into();
            let _ = setup::save(&c);
            apply_theme(&weak.unwrap());
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_set_language(move |i| {
            let mut c = setup::load();
            c.language = ["auto", "en", "pt"][i.clamp(0, 2) as usize].into();
            let _ = setup::save(&c);
            i18n::apply(&c.language);
            refresh_setup(&weak.unwrap());
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_set_autostart(move |on| {
            let mut c = setup::load();
            c.autostart = on;
            let _ = setup::save(&c);
            if let Err(e) = setup::set_autostart(on) {
                toast_error(&weak.unwrap(), e.to_string());
            }
            refresh_setup(&weak.unwrap());
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_set_connect_on_login(move |on| {
            let mut c = setup::load();
            c.connect_on_login = on;
            let _ = setup::save(&c);
            refresh_setup(&weak.unwrap());
        });
    }
    ui.on_open_log(|| {
        let _ = Command::new("xdg-open")
            .arg(vpn::log_path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    });
    {
        let weak = ui.as_weak();
        ui.on_check_for_updates(move || {
            let ui = weak.unwrap();
            toast(&ui, tr!("Checking for updates…", "Verificando atualizações…"));
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                match update::check_for_update() {
                    Ok(Some(tag)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_update_available(true);
                                ui.set_available_version(tag.into());
                                ui.set_show_update_dialog(true);
                            }
                        });
                    }
                    Ok(None) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                toast(
                                    &ui,
                                    tr!(
                                        "You are on the latest version ({})",
                                        "Você está na versão mais recente ({})",
                                        update::CURRENT_VERSION
                                    ),
                                );
                            }
                        });
                    }
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                toast_error(
                                    &ui,
                                    tr!(
                                        "Could not check for updates: {e}",
                                        "Não foi possível verificar atualizações: {e}"
                                    ),
                                );
                            }
                        });
                    }
                }
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_perform_update(move || {
            let ui = weak.unwrap();
            ui.set_updating(true);
            ui.set_update_progress(0.0);
            let target_ver = ui.get_available_version().to_string();
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let w = weak.clone();
                let result = update::apply_update(&target_ver, move |progress| {
                    let w = w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_update_progress(progress);
                        }
                    });
                });
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_updating(false);
                        ui.set_show_update_dialog(false);
                        match result {
                            Ok(new_ver) => {
                                ui.set_update_available(false);
                                ui.set_current_version(new_ver.into());
                                toast(
                                    &ui,
                                    tr!(
                                        "Updated successfully! Restart the app to apply.",
                                        "Atualizado com sucesso! Reinicie o aplicativo para aplicar."
                                    ),
                                );
                            }
                            Err(e) => {
                                toast_error(
                                    &ui,
                                    tr!("Update failed: {e}", "Falha ao atualizar: {e}"),
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    // Status polling -------------------------------------------------------------
    let networks_model: Rc<VecModel<NetInfo>> = Rc::new(VecModel::default());
    ui.set_networks(ModelRc::from(networks_model.clone()));
    // Networks the user collapsed, by id; kept across model rebuilds.
    let collapsed: Rc<RefCell<HashSet<SharedString>>> = Rc::default();
    {
        let networks_model = networks_model.clone();
        let collapsed = collapsed.clone();
        ui.on_toggle_network(move |id| {
            let mut set = collapsed.borrow_mut();
            let now = if set.remove(&id) { false } else { set.insert(id.clone()) };
            for i in 0..networks_model.row_count() {
                if let Some(mut row) = networks_model.row_data(i).filter(|r| r.id == id) {
                    row.collapsed = now;
                    networks_model.set_row_data(i, row);
                }
            }
        });
    }
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        let mut last: Option<vpn::Status> = None;
        let mut last_palette = palette_mtime();
        let mut last_alias = String::new();
        let mut last_sig = String::new();
        let mut tick = 0u32;
        timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(ui) = weak.upgrade() else { return };
            if QUITTING.load(Ordering::Relaxed) && !vpn::running() {
                let _ = slint::quit_event_loop();
                return;
            }
            let st = vpn::status();
            tick += 1;
            if tick % 3 == 0 {
                let m = palette_mtime();
                if m != last_palette {
                    last_palette = m;
                    apply_theme(&ui);
                }
            }
            let code = match &st.phase {
                vpn::Phase::Off => 0,
                vpn::Phase::Starting(_) => 1,
                vpn::Phase::On => 2,
                vpn::Phase::Stopping => 3,
                vpn::Phase::Failed(_) => 4,
            };
            // Networks come from the live pipe (named peers, per network) when
            // connected, otherwise from the kernel's ARP table (IP only, no
            // grouping). Refreshed every tick so latency and membership changes
            // show even when the phase is steady.
            let live = live_state(&client_cell, code);
            if let Some(st) = &live {
                if !st.self_alias.is_empty() && st.self_alias != last_alias {
                    last_alias = st.self_alias.clone();
                    ui.set_self_alias(st.self_alias.clone().into());
                }
            }
            let mut rows = network_rows(live.as_ref(), &st.peers);
            for row in &mut rows {
                row.collapsed = collapsed.borrow().contains(&row.id);
            }
            let sig = rows_signature(&rows);
            if sig != last_sig {
                last_sig = sig;
                networks_model.set_vec(rows);
            }
            if last.as_ref() == Some(&st) {
                return;
            }
            let detail = match &st.phase {
                vpn::Phase::Starting(s) | vpn::Phase::Failed(s) => s.clone(),
                _ => String::new(),
            };
            ui.set_phase(code);
            ui.set_phase_detail(detail.into());
            ui.set_ip(st.ip.clone().unwrap_or_default().into());
            if let Some(tray) = &tray {
                let (phase, ip) = (st.phase.clone(), st.ip.clone().unwrap_or_default());
                tray.update(move |t| {
                    t.phase = phase;
                    t.ip = ip;
                });
            }
            last = Some(st);
        });
    }

    let cfg = setup::load();
    if cfg.connect_on_login && !vpn::running() {
        start_vpn(&ui);
    }
    if !tray_only || !has_tray {
        let _ = ui.window().show();
    }
    snapshot_mode(&ui);
    let _ = slint::run_event_loop_until_quit();
    if !snapshot {
        let _ = std::fs::remove_file(&sock);
    }
}
