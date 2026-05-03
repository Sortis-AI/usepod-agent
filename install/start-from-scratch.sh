#!/bin/sh
# Use Pod — start-from-scratch operator bootstrap.
#
# Installs the bare minimum to serve inference: llama.cpp's `llama-server`
# binary plus a known-good small model (Llama-3.2-3B Q4_K_M, ~2GB, runs on
# almost any consumer GPU and even CPU). After this script, `usepod-agent
# setup` will detect llama.cpp on :8080 and the operator can pair.
#
# Usage:
#   bash <(curl -fsSL https://usepod.ai/start-from-scratch.sh)
#
# Optional environment overrides:
#   USEPOD_PREFIX   install prefix (default: /usr/local)
#   USEPOD_MODEL    model URL to download (default: Llama-3.2-3B Q4_K_M)

set -eu

PREFIX="${USEPOD_PREFIX:-/usr/local}"
INSTALL_DIR="${PREFIX}/bin"
MODELS_DIR="${HOME}/.usepod-agent/models"
MODEL_URL="${USEPOD_MODEL:-https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf}"
MODEL_FILE="${MODELS_DIR}/Llama-3.2-3B-Instruct-Q4_K_M.gguf"

info() { printf '\033[36m▶\033[0m %s\n' "$1"; }
err()  { printf '\033[31m✗\033[0m %s\n' "$1" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }

# --- Sanity checks -----------------------------------------------------------

need uname
need curl

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}-${ARCH}" in
    Linux-x86_64|Linux-aarch64|Darwin-arm64) ;;
    *) err "unsupported platform: ${OS}-${ARCH}" ;;
esac

info "Use Pod — start-from-scratch installer"
info "platform: ${OS}-${ARCH}"

# --- llama.cpp ---------------------------------------------------------------

if command -v llama-server >/dev/null 2>&1; then
    info "llama-server already installed at $(command -v llama-server) — skipping"
else
    info "installing llama.cpp via system package manager"
    if [ "$OS" = "Darwin" ]; then
        if ! command -v brew >/dev/null 2>&1; then
            err "Homebrew is required on macOS. Install it from https://brew.sh"
        fi
        brew install llama.cpp
    elif [ -f /etc/debian_version ]; then
        info "(building from source — Ubuntu/Debian package may lag upstream)"
        need git
        need cmake
        need make
        WORKDIR="$(mktemp -d)"
        ( cd "$WORKDIR" && \
            git clone --depth 1 https://github.com/ggerganov/llama.cpp && \
            cd llama.cpp && \
            cmake -B build -DLLAMA_CURL=ON && \
            cmake --build build --config Release -j "$(nproc)" )
        sudo install -m 755 "$WORKDIR/llama.cpp/build/bin/llama-server" "$INSTALL_DIR/llama-server"
        rm -rf "$WORKDIR"
    else
        err "unsupported Linux distribution; install llama.cpp manually then re-run"
    fi
fi

# --- Model download ----------------------------------------------------------

mkdir -p "$MODELS_DIR"
if [ -f "$MODEL_FILE" ]; then
    info "model already present at $MODEL_FILE — skipping"
else
    info "downloading model (~2GB) — this is a one-time cost"
    info "  source: $MODEL_URL"
    info "  target: $MODEL_FILE"
    curl --fail --location --progress-bar -o "$MODEL_FILE" "$MODEL_URL"
fi

# --- Provider agent ----------------------------------------------------------

if command -v usepod-agent >/dev/null 2>&1; then
    info "usepod-agent already installed at $(command -v usepod-agent) — skipping"
else
    info "installing usepod-agent"
    curl -fsSL https://usepod.ai/install.sh | sh
fi

# --- Done --------------------------------------------------------------------

cat <<EOF

──────────────────────────────────────────────────────────────────────
  Setup complete. Next steps:

  1. Start llama-server in a long-running terminal or systemd unit:

       llama-server -m "$MODEL_FILE" --host 0.0.0.0 --port 8080

  2. In another terminal, pair this agent with your Use Pod account:

       usepod-agent setup

  The setup command prints a short pair code. Type it into
  https://usepod.ai/host/pair to finish onboarding.
──────────────────────────────────────────────────────────────────────
EOF
