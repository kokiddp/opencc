#!/usr/bin/env bash
set -euo pipefail

# install.sh — installs opencc (wrapper + proxy) on Linux and macOS.
# Windows: use install.ps1.
#
# Binary source, in order of preference:
#   1. a freshly built cross binary:  <script-dir>/target/<triple>/release/
#   2. a freshly built host binary:   <script-dir>/target/release/
#   3. a prebuilt binary copied next to this script
#   4. the latest release from GitHub (sha256-verified)
#
# The local binaries are only used if they actually run on this machine
# (compatibility check via `opencc --version`).
#
# Usage:
#   ./install.sh
#
# Environment overrides:
#   OPENCC_REPO            GitHub repo (default: this repo's origin, or kokiddp/opencc)
#   OPENCC_DOWNLOAD_BASE   base URL for the downloads (tests/mirrors)
#   OPENCC_INSTALL_DIR     install directory (default: ~/.opencc)
#   OPENCC_BIN_DIR         bin directory (default: ~/.local/bin)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${OPENCC_INSTALL_DIR:-$HOME/.opencc}"
BIN_DIR="${OPENCC_BIN_DIR:-$HOME/.local/bin}"

log()  { printf '%s\n' "$*"; }
die()  { printf 'Error: %s\n' "$*" >&2; exit 1; }

# --- 0) Detect OS and architecture → release asset name ------------------------
OS=""
case "$(uname -s)" in
  Linux)  OS="linux" ;;
  Darwin) OS="darwin" ;;
  *) die "unsupported OS '$(uname -s)': use install.ps1 on Windows, or build from source (cargo build --release)." ;;
esac

ARCH=""
case "$(uname -m)" in
  x86_64|amd64)   ARCH="x86_64" ;;
  i686|i386|x86)  ARCH="i686" ;;
  aarch64|arm64)  ARCH="aarch64" ;;
  armv7l|armv7)   ARCH="armv7" ;;
  armv6l|armv6|arm) ARCH="armv6" ;;
  riscv64)        ARCH="riscv64" ;;
  *) die "unsupported architecture '$(uname -m)' — build from source (cargo build --release)." ;;
esac

# A Raspberry Pi 0/1 (ARMv6 CPU) often runs an ARMv7 kernel and reports
# armv7l: refine with /proc/cpuinfo.
if [[ "$ARCH" == "armv7" && -r /proc/cpuinfo ]] && grep -qi 'ARMv6' /proc/cpuinfo; then
  ARCH="armv6"
fi

# Release asset name for this platform. Linux x86_64/aarch64 use the fully
# static musl builds (they run on any distro, including Alpine).
TRIPLE=""
case "$OS-$ARCH" in
  linux-x86_64)  TRIPLE="x86_64-unknown-linux-musl" ;;
  linux-aarch64) TRIPLE="aarch64-unknown-linux-musl" ;;
  linux-i686)    TRIPLE="i686-unknown-linux-gnu" ;;
  linux-armv7)   TRIPLE="armv7-unknown-linux-gnueabihf" ;;
  linux-armv6)   TRIPLE="arm-unknown-linux-gnueabihf" ;;
  linux-riscv64) TRIPLE="riscv64gc-unknown-linux-gnu" ;;
  darwin-x86_64) TRIPLE="x86_64-apple-darwin" ;;
  darwin-aarch64) TRIPLE="aarch64-apple-darwin" ;;
  *) die "no release binary for $OS/$ARCH — build from source (cargo build --release)." ;;
esac

log "Detected: $OS/$ARCH (release asset: $TRIPLE)"

# On musl systems (Alpine) only the static binaries work; those exist only
# for x86_64/aarch64.
is_musl() {
  ldd --version 2>&1 | head -1 | grep -qi musl
}

# --- 1) Claude Code ------------------------------------------------------------
if command -v claude >/dev/null 2>&1; then
  log "Claude Code found: $(command -v claude)"
else
  log "Claude Code is not installed (required by opencc)." >&2
  printf "Install it now? [y/N] "
  read -r ANS || ANS=""
  if [[ ! "$ANS" =~ ^[yY]$ ]]; then
    log "Install Claude Code (https://claude.com/download) and re-run install.sh."
    exit 1
  fi
  if command -v curl >/dev/null 2>&1; then
    log "Installing with the official installer (https://claude.ai/install.sh)..."
    curl -fsSL https://claude.ai/install.sh | bash
  elif command -v npm >/dev/null 2>&1; then
    log "curl not found: installing via npm..."
    npm install -g @anthropic-ai/claude-code
  else
    log "Neither curl nor npm is available: install Claude Code manually and re-run install.sh." >&2
    exit 1
  fi
  hash -r 2>/dev/null || true
  # The native installer writes to ~/.local/bin: make sure it is on the PATH
  # of this shell even if the user has not added it permanently yet.
  export PATH="$BIN_DIR:$PATH"
  command -v claude >/dev/null 2>&1 || { log "Claude Code not found after installation." >&2; exit 1; }
  log "Claude Code installed: $(command -v claude)"
fi

# --- 2) Pick the source of the binaries ----------------------------------------
# (No associative arrays: macOS ships bash 3.2.)
SRC_DIR=""

# 2a) Local build directories, newest first. Compatibility check: the binary
# must actually run on this machine (`opencc --version`).
best_dir=""
best_mtime=-1
for d in "$SCRIPT_DIR/target/$TRIPLE/release" "$SCRIPT_DIR/target/release" "$SCRIPT_DIR"; do
  if [[ -f "$d/opencc" && -f "$d/opencc-proxy" ]]; then
    m="$(stat -c %Y "$d/opencc" 2>/dev/null || stat -f %m "$d/opencc" 2>/dev/null || echo -1)"
    if [[ "$m" -gt "$best_mtime" ]]; then
      best_dir="$d"
      best_mtime="$m"
    fi
  fi
done
if [[ -n "$best_dir" ]] && "$best_dir/opencc" --version >/dev/null 2>&1; then
  SRC_DIR="$best_dir"
  log "Using locally built binaries: $SRC_DIR"
fi

# 2b) GitHub latest release.
if [[ -z "$SRC_DIR" ]]; then
  if [[ "$OS" == "linux" ]] && is_musl && [[ "$TRIPLE" != *musl ]]; then
    die "musl system and no static release for $ARCH — build from source (cargo build --release)."
  fi
  REPO="${OPENCC_REPO:-}"
  if [[ -z "$REPO" ]]; then
    REMOTE_URL="$(git -C "$SCRIPT_DIR" remote get-url origin 2>/dev/null || true)"
    REPO="$(printf '%s' "$REMOTE_URL" | sed -nE 's#(git@github\.com:|https://github\.com/)([^/]+/[^/]+)(\.git)?$#\2#p')"
    REPO="${REPO:-kokiddp/opencc}"
  fi
  BASE="${OPENCC_DOWNLOAD_BASE:-https://github.com/$REPO/releases/latest/download}"
  log "No usable local binaries: downloading from $BASE ..."

  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  # Download with the release asset names (that is what sha256sums.txt
  # references), verify, then rename to the plain names the install step
  # expects.
  for asset in "opencc-$TRIPLE" "opencc-proxy-$TRIPLE" "sha256sums.txt"; do
    curl -fsSL -o "$TMP/$asset" "$BASE/$asset" \
      || die "cannot download $BASE/$asset — is the release published? (drafts are not downloadable)"
  done
  # Verify the checksums (sha256sum on Linux, shasum on macOS).
  if command -v sha256sum >/dev/null 2>&1; then
    CHECKSUM="sha256sum"
  else
    CHECKSUM="shasum -a 256"
  fi
  ( cd "$TMP" && grep -E "opencc(-proxy)?-$TRIPLE" sha256sums.txt | $CHECKSUM -c - >/dev/null ) \
    || die "checksum verification failed for the downloaded binaries."
  mv -f "$TMP/opencc-$TRIPLE" "$TMP/opencc"
  mv -f "$TMP/opencc-proxy-$TRIPLE" "$TMP/opencc-proxy"
  SRC_DIR="$TMP"
fi

# --- 3) Install ----------------------------------------------------------------
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
cp -f "$SRC_DIR/opencc" "$SRC_DIR/opencc-proxy" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/opencc" "$INSTALL_DIR/opencc-proxy"
ln -sf "$INSTALL_DIR/opencc" "$BIN_DIR/opencc"
ln -sf "$INSTALL_DIR/opencc-proxy" "$BIN_DIR/opencc-proxy"

log "opencc installed:"
log "  $INSTALL_DIR/opencc        (+ opencc-proxy)"
log "  $BIN_DIR/opencc        -> $INSTALL_DIR/opencc"
log "  $BIN_DIR/opencc-proxy -> $INSTALL_DIR/opencc-proxy"
log "Version: $("$BIN_DIR/opencc" --version)"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
  log "Note: $BIN_DIR is not on your PATH. Add it, e.g. in ~/.bashrc:" >&2
  log "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc" >&2
fi
