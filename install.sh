#!/usr/bin/env bash
set -euo pipefail

# install.sh — installa opencc (wrapper + proxy) in ~/.opencc e li collega a
# ~/.local/bin tramite symlink, così aggiornare ~/.opencc (o rifare questo
# script) non lascia file orfani nel PATH.
#
# Prerequisito: Claude Code (`claude`). Se manca, lo script propone
# l'installazione (installer ufficiale via curl, fallback npm) e solo dopo
# procede con opencc.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="$HOME/.opencc"
BIN_DIR="$HOME/.local/bin"

# --- 1) Claude Code -----------------------------------------------------------
if command -v claude >/dev/null 2>&1; then
  echo "Claude Code trovato: $(command -v claude)"
else
  echo "Claude Code non è installato (richiesto per usare opencc)." >&2
  printf "Vuoi installarlo adesso? [s/N] "
  read -r ANS || ANS=""
  if [[ ! "$ANS" =~ ^[sSyY]$ ]]; then
    echo "Installa Claude Code (https://claude.com/download) e rilancia install.sh." >&2
    exit 1
  fi
  if command -v curl >/dev/null 2>&1; then
    echo "Installazione con l'installer ufficiale (https://claude.ai/install.sh)..."
    curl -fsSL https://claude.ai/install.sh | bash
  elif command -v npm >/dev/null 2>&1; then
    echo "curl non trovato: installazione via npm..."
    npm install -g @anthropic-ai/claude-code
  else
    echo "Né curl né npm sono disponibili: installa Claude Code manualmente" >&2
    echo "e rilancia install.sh." >&2
    exit 1
  fi
  hash -r 2>/dev/null || true
  # L'installer nativo scrive in ~/.local/bin: assicuriamoci che sia nel PATH
  # di questa shell anche se l'utente non l'ha ancora aggiunto in modo stabile.
  export PATH="$BIN_DIR:$PATH"
  command -v claude >/dev/null 2>&1 || { echo "Claude Code non trovato dopo l'installazione." >&2; exit 1; }
  echo "Claude Code installato: $(command -v claude)"
fi

# --- 2) File in ~/.opencc ------------------------------------------------------
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
cp -f "$SCRIPT_DIR/opencc" "$SCRIPT_DIR/opencc-proxy.mjs" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/opencc"

# --- 3) Symlink in ~/.local/bin ------------------------------------------------
ln -sf "$INSTALL_DIR/opencc" "$BIN_DIR/opencc"
ln -sf "$INSTALL_DIR/opencc-proxy.mjs" "$BIN_DIR/opencc-proxy.mjs"

echo "opencc installato:"
echo "  $INSTALL_DIR/opencc          (+ opencc-proxy.mjs)"
echo "  $BIN_DIR/opencc           -> $INSTALL_DIR/opencc"
echo "  $BIN_DIR/opencc-proxy.mjs -> $INSTALL_DIR/opencc-proxy.mjs"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
  echo "Nota: $BIN_DIR non è nel PATH. Aggiungilo, es. in ~/.bashrc:" >&2
  echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc" >&2
fi
