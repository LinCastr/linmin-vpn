//! radmin-wired: a small native front end for Radmin VPN on Linux.
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
mod vpn;

use std::cell::RefCell;
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
        .join("radmin-wired.sock")
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

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "radmin-wired".into()
    }
    fn title(&self) -> String {
        "Radmin VPN".into()
    }
    fn icon_name(&self) -> String {
        match self.phase {
            vpn::Phase::On => "network-vpn",
            vpn::Phase::Starting(_) | vpn::Phase::Stopping => "network-vpn-acquiring",
            vpn::Phase::Failed(_) => "network-vpn-error-symbolic",
            vpn::Phase::Off => "network-vpn-disconnected-symbolic",
        }
        .into()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Radmin VPN".into(),
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
        return None;
    }
    // Connect if we have no client yet.
    if cell.borrow().is_none() {
        match pipe::Client::connect_installed() {
            Ok(c) => {
                let c = Arc::new(c);
                pipe::spawn_heartbeat(Arc::clone(&c));
                *cell.borrow_mut() = Some(c);
            }
            // Leave the slot empty; the ARP fallback covers this tick and we
            // retry on the next one.
            Err(_) => return None,
        }
    }
    let client = cell.borrow().as_ref().map(Arc::clone)?;
    Some(client.snapshot())
}

/// Builds the network rows for the UI from a live snapshot, or a single
/// fallback "network" from the ARP addresses when there is no live data yet.
fn network_rows(live: Option<&state::State>, arp: &[String]) -> Vec<NetInfo> {
    if let Some(st) = live.filter(|s| !s.networks.is_empty()) {
        return st
            .networks
            .iter()
            .map(|net| {
                let peers: Vec<SharedString> =
                    net.nodes.iter().map(|n| SharedString::from(peer_line(n))).collect();
                NetInfo {
                    name: net.name.clone().into(),
                    id: hex_guid(&net.guid).into(),
                    members: net.members as i32,
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
    let peers: Vec<SharedString> = arp.iter().map(SharedString::from).collect();
    vec![NetInfo {
        name: tr!("Seen on the network", "Vistos na rede").into(),
        id: SharedString::new(),
        members: arp.len() as i32,
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
        for p in r.peers.iter() {
            s.push_str(p.as_str());
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

/// One peer row: name when known, then IP, then latency. Falls back to the IP
/// alone when the service has not sent a name yet.
fn peer_line(node: &state::Node) -> String {
    let ip = node.ipv4.map(|a| a.to_string()).unwrap_or_default();
    let mut s = if node.alias.is_empty() {
        ip.clone()
    } else if ip.is_empty() {
        node.alias.clone()
    } else {
        format!("{} — {}", node.alias, ip)
    };
    if let Some(ms) = node.rtt_ms {
        s.push_str(&format!("  ·  {ms} ms"));
    }
    s
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

/// Development: RWIRED_SNAPSHOT=file.png saves the window as an image and quits.
fn snapshot_mode(ui: &AppWindow) -> bool {
    let Ok(out) = std::env::var("RWIRED_SNAPSHOT") else { return false };
    if std::env::var("RWIRED_SETTINGS").is_ok() {
        ui.set_show_settings(true);
    }
    let weak = ui.as_weak();
    // The pipe client needs a moment to spawn Wine and receive the first state;
    // RWIRED_SNAPSHOT_DELAY (ms) lets a test wait for it.
    let delay = std::env::var("RWIRED_SNAPSHOT_DELAY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1500);
    slint::Timer::single_shot(Duration::from_millis(delay), move || {
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
    ui.set_toast(text.into());
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(3), move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_toast(SharedString::new());
        }
    });
}

fn start_vpn(ui: &AppWindow) {
    vpn::clear_failure();
    let cfg = setup::load();
    let Some(app) = setup::appimage(&cfg) else { return };
    let helper = setup::helper_state() == setup::HelperState::Ok;
    if let Err(e) = vpn::start(&app, helper) {
        toast(ui, tr!("Could not start: {e}", "Não foi possível ligar: {e}"));
    }
}

/// `radmin-wired up|down|status` for scripts and testing.
fn cli(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    if !["up", "down", "status", "pipe-dump"].contains(&cmd) {
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
                    eprintln!("radmin-wired: pipe connect: {e:#}");
                    return Some(1);
                }
            };
            std::thread::sleep(Duration::from_millis(500));
            let _ = client.ping_visible_nodes();
            // Optional write probe: `rename:<name>` sends ChangeAlias so the
            // reply (result 0x310) can be observed under RWIRED_PIPE_RAW.
            if let Some(rest) = args.get(3).and_then(|s| s.strip_prefix("rename:")) {
                if let Err(e) = client.change_alias(rest) {
                    eprintln!("radmin-wired: rename: {e:#}");
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
                eprintln!("radmin-wired: AppImage not installed (open the app to install it)");
                return Some(1);
            };
            if let Err(e) = vpn::start(&app, setup::helper_state() == setup::HelperState::Ok) {
                eprintln!("radmin-wired: {e:#}");
                return Some(1);
            }
            Some(0)
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

// Main -------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tray_only = args.iter().any(|a| a == "--tray");
    if let Some(code) = cli(&args) {
        std::process::exit(code);
    }
    let snapshot = std::env::var("RWIRED_SNAPSHOT").is_ok();

    // One instance: a second launch just shows the first one's window.
    let sock = socket_path();
    if !snapshot && let Ok(mut s) = UnixStream::connect(&sock) {
        let _ = s.write_all(if tray_only { b"noop" } else { b"show" });
        return;
    }
    let listener = if snapshot { None } else { let _ = std::fs::remove_file(&sock); UnixListener::bind(&sock).ok() };

    let ui = AppWindow::new().expect("window");
    // Only works once the platform exists (AppWindow::new creates it).
    let _ = slint::set_xdg_app_id(std::env::var("RWIRED_APP_ID").unwrap_or_else(|_| "radmin-wired".into()));
    i18n::apply(&setup::load().language);
    apply_theme(&ui);
    refresh_setup(&ui);
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
                toast(&weak.unwrap(), tr!("Could not open: {e}", "Não foi possível abrir: {e}"));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_create_network(move |name, password| {
            let ui = weak.unwrap();
            let name = name.trim();
            if name.is_empty() {
                toast(&ui, tr!("Network name is required", "Nome da rede é obrigatório"));
                return;
            }
            if password.len() < 6 {
                toast(
                    &ui,
                    tr!(
                        "Password must have at least 6 characters",
                        "A senha precisa ter pelo menos 6 caracteres"
                    ),
                );
                return;
            }
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                toast(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            if let Err(e) = client.create_network(name, &password) {
                toast(
                    &ui,
                    tr!(
                        "Could not create network: {e}",
                        "Não foi possível criar rede: {e}"
                    ),
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_join_network(move |name, password| {
            let ui = weak.unwrap();
            let name = name.trim();
            if name.is_empty() {
                toast(&ui, tr!("Network name is required", "Nome da rede é obrigatório"));
                return;
            }
            if password.is_empty() {
                toast(&ui, tr!("Password is required", "Senha é obrigatória"));
                return;
            }
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                toast(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            if let Err(e) = client.join_network(name, &password) {
                toast(
                    &ui,
                    tr!(
                        "Could not join network: {e}",
                        "Não foi possível entrar na rede: {e}"
                    ),
                );
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
                toast(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            if let Err(e) = client.leave_network(guid) {
                toast(&ui, tr!("Could not leave: {e}", "Não foi possível sair: {e}"));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let client_cell = client_cell.clone();
        ui.on_rename_self(move |name| {
            let ui = weak.unwrap();
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let Some(client) = client_cell.borrow().as_ref().map(Arc::clone) else {
                toast(&ui, tr!("Not connected", "Sem conexão"));
                return;
            };
            if let Err(e) = client.change_alias(name) {
                toast(&ui, tr!("Could not rename: {e}", "Não foi possível renomear: {e}"));
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
                            toast(&ui, tr!("Download failed: {e}", "O download falhou: {e}"));
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
                toast(&weak.unwrap(), e.to_string());
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

    // Status polling -------------------------------------------------------------
    let networks_model: Rc<VecModel<NetInfo>> = Rc::new(VecModel::default());
    ui.set_networks(ModelRc::from(networks_model.clone()));
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
            let rows = network_rows(live.as_ref(), &st.peers);
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
    if tray_only && cfg.connect_on_login && !vpn::running() {
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
