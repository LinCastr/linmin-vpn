# radmin-wired

A small native Linux front end for [Radmin VPN](https://www.radmin-vpn.com/).

Radmin VPN is Windows-only. [radmin-vpn-linux](https://github.com/baptisterajaut/radmin-vpn-linux)
already runs it on Linux through Wine, but you get the bundled Wine GUI and a
terminal window, and every launch asks for your sudo password. radmin-wired
replaces that with:

- a **native window** and a **system-tray icon** (StatusNotifierItem), no terminal;
- **no sudo prompt** after a one-time setup — a tiny root helper, allowed through
  polkit, configures only the `radminvpn0` interface and nothing else;
- optional **wallust** theming, so it matches a themed desktop;
- English and Brazilian Portuguese.

It drives the official Radmin VPN service underneath (via radmin-vpn-linux), so
memory use is the service's, not a second heavy app.

> Status: early. The window, tray, theming and the password-free setup work.
> Talking to the Radmin service directly over its pipe (to join/leave networks
> without the Wine window at all) is in progress — see `docs/PROTOCOL.md`.

## Build

```bash
cargo build --release
```

The binary is `target/release/radmin-wired`. `radmin-wired --tray` starts hidden
in the tray; `radmin-wired up|down|status` is a small command-line interface.

## License

GPL-3.0-or-later.
