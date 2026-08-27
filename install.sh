#!/usr/bin/env bash
set -euo pipefail

# install.sh — installs opencc (wrapper + proxy) under ~/.opencc and links
# both into ~/.local/bin via symlinks, so refreshing ~/.opencc (or re-running
# this script) never leaves orphaned files on the PATH.
#
# Prerequisite: Claude Code (`claude`). If it is missing, the script offers to
# install it (official installer via curl, npm as a fallback) and only then
# proceeds with opencc.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="$HOME/.opencc"
BIN_DIR="$HOME/.local/bin"

# --- 1) Claude Code -----------------------------------------------------------
if command -v claude >/dev/null 2>&1; then
  echo "Claude Code found: $(command -v claude)"
else
  echo "Claude Code is not installed (required by opencc)." >&2
  printf "Install it now? [y/N] "
  read -r ANS || ANS=""
  if [[ ! "$ANS" =~ ^[yY]$ ]]; then
    echo "Install Claude Code (https://claude.com/download) and re-run install.sh." >&2
    exit 1
  fi
  if command -v curl >/dev/null 2>&1; then
    echo "Installing with the official installer (https://claude.ai/install.sh)..."
    curl -fsSL https://claude.ai/install.sh | bash
  elif command -v npm >/dev/null 2>&1; then
    echo "curl not found: installing via npm..."
    npm install -g @anthropic-ai/claude-code
  else
    echo "Neither curl nor npm is available: install Claude Code manually" >&2
    echo "and re-run install.sh." >&2
    exit 1
  fi
  hash -r 2>/dev/null || true
  # The native installer writes to ~/.local/bin: make sure it is on the PATH
  # of this shell even if the user has not added it permanently yet.
  export PATH="$BIN_DIR:$PATH"
  command -v claude >/dev/null 2>&1 || { echo "Claude Code not found after installation." >&2; exit 1; }
  echo "Claude Code installed: $(command -v claude)"
fi

# --- 2) Files under ~/.opencc ---------------------------------------------------
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
cp -f "$SCRIPT_DIR/opencc" "$SCRIPT_DIR/opencc-proxy.mjs" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/opencc"

# --- 3) Symlinks in ~/.local/bin ------------------------------------------------
ln -sf "$INSTALL_DIR/opencc" "$BIN_DIR/opencc"
ln -sf "$INSTALL_DIR/opencc-proxy.mjs" "$BIN_DIR/opencc-proxy.mjs"

echo "opencc installed:"
echo "  $INSTALL_DIR/opencc          (+ opencc-proxy.mjs)"
echo "  $BIN_DIR/opencc           -> $INSTALL_DIR/opencc"
echo "  $BIN_DIR/opencc-proxy.mjs -> $INSTALL_DIR/opencc-proxy.mjs"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
  echo "Note: $BIN_DIR is not on your PATH. Add it, e.g. in ~/.bashrc:" >&2
  echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc" >&2
fi
