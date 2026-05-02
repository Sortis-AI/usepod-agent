#!/bin/sh
# Use Pod provider-agent installer (POSIX sh).
#
# Usage:
#   curl -fsSL https://usepod.ai/install.sh | sh
#
# Optional environment overrides:
#   USEPOD_VERSION   pin a specific release tag (default: latest)
#   USEPOD_PREFIX    install prefix (default: /usr/local)
#   USEPOD_BASE_URL  base URL for the version pointer (default: https://usepod.ai)
#   USEPOD_REPO      GitHub releases repo (default: Sortis-AI/usepod-agent)

set -eu

# --- Configuration --------------------------------------------------------

FALLBACK_VERSION="v0.1.2"
BASE_URL="${USEPOD_BASE_URL:-https://usepod.ai}"
REPO="${USEPOD_REPO:-Sortis-AI/usepod-agent}"
PREFIX="${USEPOD_PREFIX:-/usr/local}"
INSTALL_DIR="${PREFIX}/bin"
TMPDIR_BASE="${TMPDIR:-/tmp}"

# --- Helpers --------------------------------------------------------------

err() {
    printf 'usepod-agent installer: error: %s\n' "$1" >&2
    exit 1
}

info() {
    printf 'usepod-agent installer: %s\n' "$1"
}

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "required command not found: $1"
    fi
}

# Pick a sha256 verification command. POSIX-portable across Linux + macOS.
sha256_verify() {
    # $1 = sums file; cwd must contain referenced file
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$1"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$1"
    else
        err "neither sha256sum nor shasum is available; cannot verify download"
    fi
}

# --- Platform detection ---------------------------------------------------

need_cmd uname
need_cmd curl

OS=$(uname -s)
ARCH=$(uname -m)
PLATFORM="${OS}-${ARCH}"

case "$PLATFORM" in
    Linux-x86_64)
        ASSET="usepod-agent-linux-x64"
        ;;
    Linux-aarch64)
        ASSET="usepod-agent-linux-arm64"
        ;;
    Darwin-arm64)
        ASSET="usepod-agent-darwin-arm64"
        ;;
    Darwin-x86_64)
        err "Intel Mac is not supported. Use an Apple Silicon Mac, or run the Linux build under Docker."
        ;;
    *)
        err "unsupported platform: ${PLATFORM}. Supported: Linux-x86_64, Linux-aarch64, Darwin-arm64."
        ;;
esac

# --- Resolve version ------------------------------------------------------

if [ -n "${USEPOD_VERSION:-}" ]; then
    VERSION="$USEPOD_VERSION"
    info "using pinned version: ${VERSION}"
else
    info "fetching latest version from ${BASE_URL}/agent-latest"
    if VERSION=$(curl -fsSL "${BASE_URL}/agent-latest" 2>/dev/null); then
        VERSION=$(printf '%s' "$VERSION" | tr -d '\r\n[:space:]')
    fi
    if [ -z "${VERSION:-}" ]; then
        info "could not resolve latest version; falling back to ${FALLBACK_VERSION}"
        VERSION="$FALLBACK_VERSION"
    fi
fi

case "$VERSION" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
esac

RELEASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

# --- Download + verify ----------------------------------------------------

WORKDIR=$(mktemp -d "${TMPDIR_BASE}/usepod-agent-install.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT INT HUP TERM

info "downloading ${ASSET} (${VERSION})"
if ! curl --fail --location --silent --show-error \
        --output "${WORKDIR}/${ASSET}" \
        "${RELEASE_URL}/${ASSET}"; then
    err "failed to download ${RELEASE_URL}/${ASSET}"
fi

info "downloading checksum"
if ! curl --fail --location --silent --show-error \
        --output "${WORKDIR}/${ASSET}.sha256" \
        "${RELEASE_URL}/${ASSET}.sha256"; then
    err "failed to download ${RELEASE_URL}/${ASSET}.sha256"
fi

info "verifying SHA-256"
( cd "$WORKDIR" && sha256_verify "${ASSET}.sha256" >/dev/null ) \
    || err "checksum verification failed for ${ASSET}"

# --- Install --------------------------------------------------------------

if [ ! -d "$INSTALL_DIR" ]; then
    err "install directory does not exist: ${INSTALL_DIR}"
fi

# Choose privilege escalation only if we cannot write directly.
SUDO=""
if [ ! -w "$INSTALL_DIR" ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
        info "installing to ${INSTALL_DIR} requires sudo"
    else
        err "${INSTALL_DIR} is not writable and sudo is unavailable. Set USEPOD_PREFIX=\$HOME/.local"
    fi
fi

TARGET="${INSTALL_DIR}/usepod-agent"
if command -v install >/dev/null 2>&1; then
    $SUDO install -m 755 "${WORKDIR}/${ASSET}" "$TARGET"
else
    $SUDO cp "${WORKDIR}/${ASSET}" "$TARGET"
    $SUDO chmod 755 "$TARGET"
fi

info "Installed usepod-agent ${VERSION}. Run: usepod-agent --help"
