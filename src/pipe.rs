//! Native client for the Radmin service pipe, over the `rvrelay.exe` bridge.
//!
//! Wine owns the named pipe, so a Linux process cannot open it directly. The
//! app spawns `rvrelay.exe` inside the service's own Wine prefix with its
//! stdin/stdout wired here; the relay bridges raw bytes to the pipe. This client
//! frames commands out, reassembles event frames in, and folds them into a
//! shared [`State`].
//!
//! NOTE: the transport and state parsing have unit tests, but the end-to-end
//! path (relay + live service) has not yet been exercised against a running
//! Radmin service — see `docs/PROTOCOL.md` "Next steps". The real GUI window
//! must not run at the same time: the service allows one GUI client on the pipe.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::msg;
use crate::proto::{decode_body, encode_frame, Attr, FrameReader};
use crate::setup;
use crate::state::State;
use crate::vpn;

/// A live connection to the service through the relay. Dropping it stops the
/// relay and the reader/heartbeat threads.
pub struct Client {
    child: Child,
    stdin: Mutex<ChildStdin>,
    state: Arc<Mutex<State>>,
    next_id: AtomicU32,
    alive: Arc<AtomicBool>,
}

impl Client {
    /// Spawns the relay in the service's Wine prefix and starts pumping frames.
    /// `relay_exe` is the Linux path to `rvrelay.exe`.
    pub fn connect(relay_exe: &Path) -> Result<Client> {
        let env = vpn::wine_env()?;
        let mut child = Command::new(&env.wine)
            .arg(relay_exe)
            .current_dir(&env.radmin_dir)
            .env_clear()
            .envs(env.vars.iter().map(|(k, v)| (k, v)))
            .env("WINEDEBUG", "-all")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("wine rvrelay.exe")?;

        let stdin = child.stdin.take().context("relay stdin")?;
        let stdout = child.stdout.take().context("relay stdout")?;
        let state = Arc::new(Mutex::new(State::default()));
        let alive = Arc::new(AtomicBool::new(true));

        // Reader: relay stdout -> frames -> state.
        {
            let state = Arc::clone(&state);
            let alive = Arc::clone(&alive);
            thread::spawn(move || read_loop(stdout, state, alive));
        }

        let client = Client {
            child,
            stdin: Mutex::new(stdin),
            state,
            next_id: AtomicU32::new(1),
            alive,
        };
        Ok(client)
    }

    /// Installs the relay if needed, then connects. The convenience entry point
    /// for the app.
    pub fn connect_installed() -> Result<Client> {
        let relay = setup::ensure_relay()?;
        Client::connect(&relay)
    }

    /// Snapshot of the current state for the UI to render.
    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// True while the reader thread has not seen the relay close.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn next_id(&self) -> u32 {
        // Wrap before the NO_REPLY sentinel so a real request never collides.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id >= msg::NO_REPLY { 1 } else { id }
    }

    fn send(&self, attrs: &[Attr]) -> Result<()> {
        let frame = encode_frame(attrs);
        let mut w = self.stdin.lock().unwrap();
        w.write_all(&frame).context("write to relay")?;
        w.flush().context("flush relay")?;
        Ok(())
    }

    // -- mapped commands -----------------------------------------------------

    pub fn create_network(&self, name: &str, password: &str) -> Result<()> {
        self.send(&msg::create_network(self.next_id(), name, password))
    }

    pub fn join_network(&self, name: &str, password: &str) -> Result<()> {
        self.send(&msg::join_network(self.next_id(), name, password))
    }

    pub fn switch_power(&self, on: bool) -> Result<()> {
        self.send(&msg::switch_power(self.next_id(), on))
    }

    pub fn leave_network(&self, net: [u8; 16]) -> Result<()> {
        self.send(&msg::leave_network(self.next_id(), net))
    }

    pub fn delete_network(&self, net: [u8; 16]) -> Result<()> {
        self.send(&msg::delete_network(self.next_id(), net))
    }

    pub fn change_alias(&self, alias: &str) -> Result<()> {
        self.send(&msg::change_alias(self.next_id(), alias))
    }

    pub fn update_network(
        &self,
        net: [u8; 16],
        name: &str,
        prop_a: bool,
        prop_b: bool,
        prop_c: bool,
    ) -> Result<()> {
        self.send(&msg::update_network(self.next_id(), net, name, prop_a, prop_b, prop_c))
    }

    /// Sends the visible-nodes heartbeat once, for every node currently known.
    pub fn ping_visible_nodes(&self) -> Result<()> {
        let ids: Vec<u64> = {
            let st = self.state.lock().unwrap();
            st.networks.iter().flat_map(|n| n.nodes.iter().map(|x| x.id)).collect()
        };
        self.send(&msg::update_visible_nodes(&ids))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        // Closing stdin makes the relay's input pump exit; then kill the child
        // so Wine does not leave a lingering process.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads relay stdout until it closes, folding each frame into the state.
fn read_loop(mut stdout: impl Read, state: Arc<Mutex<State>>, alive: Arc<AtomicBool>) {
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        reader.feed(&buf[..n]);
        while let Some(body) = reader.next_frame() {
            if let Ok(attrs) = decode_body(&body) {
                if std::env::var_os("RWIRED_PIPE_RAW").is_some() {
                    eprintln!("--- frame ({} bytes) ---\n{attrs:#x?}", body.len());
                }
                state.lock().unwrap().apply(&attrs);
            }
        }
    }
    alive.store(false, Ordering::Relaxed);
}

/// Spawns a background heartbeat that pings visible nodes every ~3 s, matching
/// the original GUI, until the client dies. Call once after `connect`.
pub fn spawn_heartbeat(client: Arc<Client>) {
    thread::spawn(move || {
        while client.is_alive() {
            let _ = client.ping_visible_nodes();
            thread::sleep(Duration::from_secs(3));
        }
    });
}
