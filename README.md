<div align="center">

<img src="assets/linmin-vpn.svg" width="104" alt="Linmin VPN logo">

# Linmin VPN

**A lightweight, native Linux front end for [Radmin VPN](https://www.radmin-vpn.com/).**

![Linux](https://img.shields.io/badge/platform-Linux-1d2026?style=flat-square&logo=linux&logoColor=white)
![Rust](https://img.shields.io/badge/made%20with-Rust-1d2026?style=flat-square&logo=rust&logoColor=white)
![Slint](https://img.shields.io/badge/UI-Slint-1d2026?style=flat-square)
![License](https://img.shields.io/badge/license-GPL--3.0-4d8eff?style=flat-square)

</div>


Radmin VPN is officially Windows-only. While [radmin-vpn-linux](https://github.com/baptisterajaut/radmin-vpn-linux) enables running it on Linux through Wine, it relies on the bundled legacy Wine GUI, requires a terminal window, and prompts for your administrator (`sudo`) password every single launch.

**Linmin VPN** replaces the Wine GUI with a clean, native desktop application in Rust:

- **Native Interface & Tray:** Small, responsive desktop window powered by Slint and a System Tray (StatusNotifierItem) icon — zero terminal windows.
- **Direct Service IPC:** Communicates directly with the background Radmin control service via an internal named-pipe relay. Create, join, approve members, and manage networks without ever opening the Wine window.
- **Passwordless After Setup:** A tiny, restricted root helper allowed via Polkit configures only the `radminvpn0` interface. No repeated sudo prompts every time you turn on the VPN.
- **Automated Runtime Setup:** Automatically downloads and verifies the runtime AppImage if not already installed.
- **Dynamic Theming:** Seamless support for desktop color schemes (including optional [wallust](https://github.com/huacnlee/wallust) integration).
- **Multilingual:** Full support for English and Brazilian Portuguese (pt-BR).

---

## Prerequisites

- **Linux x86_64** (Wayland or X11)
- **FUSE 2:** AppImages require `fuse2` (`libfuse2` on Ubuntu/Debian, `fuse2` on Arch/Fedora) to mount properly.
- **Polkit Agent:** A graphical Polkit agent (standard on KDE, GNOME, XFCE, or daemons like `polkit-kde-authentication-agent-1` on window managers like Hyprland/Sway).

---

## Installation & Usage

### Quick install

```bash
curl -fsSL https://raw.githubusercontent.com/LinCastr/linmin-vpn/main/install.sh | bash
```

The script installs to `~/.local` (no root needed), verifies the SHA-256 of the
release binary, and adds the icon and desktop entry. It only reports missing
dependencies; it never installs packages for you. Read it first if you like:
[`install.sh`](install.sh). Pin a version with `LINMIN_VERSION=v0.1.0`.

### Manual install

Download `linmin-vpn-x86_64-linux` from the [Releases](https://github.com/LinCastr/linmin-vpn/releases) page, make it executable, and place it in your PATH:

```bash
chmod +x linmin-vpn-x86_64-linux
mv linmin-vpn-x86_64-linux ~/.local/bin/linmin-vpn
```

Run `linmin-vpn` to launch the application.

### One-Time Polkit Setup

To configure the network interface without asking for your password every time:

```bash
linmin-vpn install-helper
```

*(You can also simply click the power button or **Allow** under Settings inside the app, which will prompt for authorization once).*

### Command-Line Interface

Linmin VPN can also be controlled headlessly or through scripts:

```bash
linmin-vpn up             # Connect / turn on the VPN
linmin-vpn down           # Disconnect / turn off the VPN
linmin-vpn status         # Output JSON status with IP, phase, and peers
linmin-vpn --tray         # Start directly minimized in the tray
```

---

## Building from Source

To compile Linmin VPN from source, you need **Rust** (stable) and **[Zig](https://ziglang.org/)** (used by `build.rs` to cross-compile the embedded Windows named-pipe relay `rvrelay.exe`):

```bash
git clone https://github.com/LinCastr/linmin-vpn.git
cd linmin-vpn
cargo build --release
```

The resulting binary will be located at `target/release/linmin-vpn`.

---

## Acknowledgments & Credits

- **[Baptiste Rajaut (baptisterajaut/radmin-vpn-linux)](https://github.com/baptisterajaut/radmin-vpn-linux):** For creating the pioneering Wine runtime wrapper, TAP network bridge, and standalone AppImage that powers the underlying VPN service.
- **[Slint](https://slint.dev/):** The lightweight native GUI toolkit powering the user interface.

---

## Disclaimer & Trademarks

Linmin VPN is an independent, community-driven open-source project by the [LinCastr](https://github.com/LinCastr) organization and is **not** affiliated with, endorsed by, or sponsored by **Famatech Corp.** 

*Radmin* and *Radmin VPN* are registered trademarks of Famatech Corp. Linmin VPN does not distribute or alter any proprietary Famatech binaries; all underlying services are downloaded directly by the upstream runtime environment.

---

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) for details.
