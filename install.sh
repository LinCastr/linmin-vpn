#!/usr/bin/env bash
# Linmin VPN installer: downloads the latest release binary, verifies its
# checksum and installs it for the current user (no root needed).
#
#   curl -fsSL https://raw.githubusercontent.com/LinCastr/linmin-vpn/main/install.sh | bash
#
# Environment:
#   LINMIN_VERSION  release tag to install (default: latest), e.g. v0.1.0
#   PREFIX          install prefix for the binary (default: ~/.local)
set -euo pipefail

REPO="LinCastr/linmin-vpn"
RAW="https://raw.githubusercontent.com/${REPO}/main"
PREFIX="${PREFIX:-$HOME/.local}"
VERSION="${LINMIN_VERSION:-latest}"
ASSET="linmin-vpn-x86_64-linux"

say()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ]   || die "Linux only."
[ "$(uname -m)" = "x86_64" ]  || die "only x86_64 builds are published."
[ "$(id -u)" -ne 0 ]          || die "run as your normal user, not root."
command -v curl >/dev/null    || die "curl is required."
command -v sha256sum >/dev/null || die "sha256sum is required."

if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/${REPO}/releases/latest/download"
else
  BASE="https://github.com/${REPO}/releases/download/${VERSION}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading Linmin VPN (${VERSION})"
curl -fsSL "${BASE}/${ASSET}" -o "${tmp}/${ASSET}"
curl -fsSL "${BASE}/${ASSET}.sha256" -o "${tmp}/${ASSET}.sha256"

say "Verifying checksum"
(cd "$tmp" && sha256sum -c "${ASSET}.sha256" >/dev/null) || die "checksum mismatch, aborting."

bin_dir="${PREFIX}/bin"
app_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icon_dir="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/256x256/apps"
mkdir -p "$bin_dir" "$app_dir" "$icon_dir"

say "Installing to ${bin_dir}/linmin-vpn"
install -m 0755 "${tmp}/${ASSET}" "${bin_dir}/linmin-vpn"

say "Installing icon and desktop entry"
curl -fsSL "${RAW}/data/linmin-vpn.png" -o "${icon_dir}/linmin-vpn.png"
curl -fsSL "${RAW}/data/linmin-vpn.desktop" -o "${app_dir}/linmin-vpn.desktop"
sed -i "s|^Exec=.*|Exec=${bin_dir}/linmin-vpn|" "${app_dir}/linmin-vpn.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$app_dir" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "${icon_dir%/256x256/apps}" 2>/dev/null || true

# Runtime dependencies are only reported, never installed behind your back.
missing=()
command -v wine >/dev/null   || missing+=("wine")
command -v pkexec >/dev/null || missing+=("polkit")
command -v ip >/dev/null     || missing+=("iproute2")
if ! ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; then missing+=("fuse2 (libfuse2)"); fi
if [ "${#missing[@]}" -gt 0 ]; then
  warn "missing runtime dependencies: ${missing[*]}"
fi

case ":$PATH:" in
  *":${bin_dir}:"*) ;;
  *) warn "${bin_dir} is not in your PATH; add it to run 'linmin-vpn' from a terminal." ;;
esac

say "Done. Run 'linmin-vpn' (or open it from your app menu)."
say "First launch downloads the Radmin runtime and asks once to authorize the network helper."
