#!/usr/bin/env bash
# DialF installer.
#
#   curl -fsSL https://dl.agora.build/dialf/install.sh | bash
#
# Downloads the prebuilt `dialf` binary (+ bundled ten-vad lib) for this OS/arch,
# installs it under /opt/dialf with a symlink in /usr/local/bin, and (unless
# DIALF_NO_SERVICE=1) installs dialfd as a boot service via `dialf service install`.
#
# Env overrides:
#   DIALF_REPO       GitHub owner/repo            (default Agora-Build/DialF)
#   DIALF_VERSION    release tag or "latest"      (default latest)
#   DIALF_PREFIX     install dir                  (default /opt/dialf)
#   DIALF_BINDIR     symlink dir                  (default /usr/local/bin)
#   DIALF_NO_SERVICE 1 = skip `dialf service install`
#   DIALF_USER_SERVICE 1 = install as a per-user (login) service instead of system
set -euo pipefail

REPO="${DIALF_REPO:-Agora-Build/DialF}"
VERSION="${DIALF_VERSION:-latest}"
PREFIX="${DIALF_PREFIX:-/opt/dialf}"
BINDIR="${DIALF_BINDIR:-/usr/local/bin}"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Run a command as root when not already root.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 && SUDO="sudo" || die "need root (or sudo) to install into $PREFIX / $BINDIR"
fi

# --- detect platform -> release target ---
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Darwin) os_tag="darwin" ;;
  Linux)  os_tag="linux" ;;
  *) die "unsupported OS: $os" ;;
esac
case "$arch" in
  x86_64|amd64)  arch_tag="x86_64" ;;
  arm64|aarch64) arch_tag="aarch64" ;;
  *) die "unsupported arch: $arch" ;;
esac
target="${os_tag}-${arch_tag}"
say "platform: $target"

# --- resolve version ---
if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -m1 '"tag_name"' | cut -d'"' -f4)"
  [ -n "$VERSION" ] || die "could not resolve latest version from GitHub"
fi
say "version: $VERSION"

# Release tag is vX.Y.Z; asset file uses the bare X.Y.Z.
ver_no_v="${VERSION#v}"
asset="dialf-${ver_no_v}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"

# --- download + extract ---
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
say "downloading $url"
curl -fSL -o "$tmp/$asset" "$url" || die "download failed: $url"
tar -xzf "$tmp/$asset" -C "$tmp"

# Tarball layout: dialf-<ver>-<target>/{dialf, lib/...}
srcdir="$(find "$tmp" -maxdepth 1 -type d -name 'dialf-*' | head -1)"
[ -n "$srcdir" ] || die "unexpected tarball layout"

# --- install ---
say "installing to $PREFIX"
$SUDO rm -rf "$PREFIX"
$SUDO mkdir -p "$PREFIX" "$BINDIR"
$SUDO cp -R "$srcdir"/. "$PREFIX"/
$SUDO chmod +x "$PREFIX/dialf"
$SUDO ln -sf "$PREFIX/dialf" "$BINDIR/dialf"
say "installed: $($BINDIR/dialf --version 2>/dev/null || echo dialf)"

# --- service ---
if [ "${DIALF_NO_SERVICE:-0}" = "1" ]; then
  say "skipping service install (DIALF_NO_SERVICE=1). Start later with: dialf service install"
else
  if [ "${DIALF_USER_SERVICE:-0}" = "1" ]; then
    say "installing dialfd as a per-user (login) service"
    "$BINDIR/dialf" service install --user
  else
    say "installing dialfd as a system (boot) service"
    $SUDO "$BINDIR/dialf" service install
  fi
fi

say "done. dialfd is running. Try: dialf devices"
