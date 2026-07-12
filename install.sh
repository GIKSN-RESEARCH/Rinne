#!/bin/sh
# Rinne installer.
#
#   curl -fsSL https://raw.githubusercontent.com/GIKSN-RESEARCH/Rinne/main/install.sh | sh
#
# Downloads the prebuilt `rinne` binary for your platform from the latest
# GitHub Release, verifies its SHA-256 checksum, and installs it to
# ~/.local/bin (override with RINNE_INSTALL_DIR). Re-run to upgrade.
#
# Environment overrides:
#   RINNE_VERSION       tag to install, e.g. v0.1.7 (default: latest release)
#   RINNE_INSTALL_DIR   install directory (default: $HOME/.local/bin)
#   RINNE_REPO          owner/repo to download from (default: GIKSN-RESEARCH/Rinne)

set -eu

REPO="${RINNE_REPO:-GIKSN-RESEARCH/Rinne}"
INSTALL_DIR="${RINNE_INSTALL_DIR:-$HOME/.local/bin}"
BIN="rinne"

info() { printf '\033[1;34m=>\033[0m %s\n' "$1"; }
err()  { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }
need uname
need tar
need mktemp

# Prefer curl, fall back to wget.
if command -v curl >/dev/null 2>&1; then
  DL="curl -fsSL"
  DL_O="curl -fsSL -o"
elif command -v wget >/dev/null 2>&1; then
  DL="wget -qO-"
  DL_O="wget -qO"
else
  err "need either curl or wget installed"
fi

# Map uname output to a Rust target triple matching the release asset names.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64)        echo "x86_64-apple-darwin" ;;
        *) err "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) err "no prebuilt Linux arm64 binary yet; install with: cargo install rinne" ;;
        *) err "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    *)
      err "unsupported OS: $os (Windows users: download the .zip from the Releases page)"
      ;;
  esac
}

resolve_version() {
  if [ -n "${RINNE_VERSION:-}" ]; then
    echo "$RINNE_VERSION"
    return
  fi
  # Follow the /releases/latest redirect to discover the newest tag without jq.
  tag="$($DL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep -m1 '"tag_name"' \
    | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
  [ -n "$tag" ] || err "could not determine latest release tag for $REPO"
  echo "$tag"
}

verify_checksum() {
  archive="$1"
  sumfile="$2"
  expected="$(awk '{print $1}' "$sumfile")"
  if command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
  elif command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive" | awk '{print $1}')"
  else
    info "no shasum/sha256sum available; skipping checksum verification"
    return
  fi
  [ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"
  info "checksum verified"
}

main() {
  target="$(detect_target)"
  version="$(resolve_version)"
  asset="rinne-${target}.tar.gz"
  base="https://github.com/$REPO/releases/download/$version"

  info "installing rinne $version for $target"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  info "downloading $asset"
  $DL_O "$tmp/$asset" "$base/$asset" || err "download failed: $base/$asset"
  if $DL_O "$tmp/$asset.sha256" "$base/$asset.sha256" 2>/dev/null; then
    verify_checksum "$tmp/$asset" "$tmp/$asset.sha256"
  else
    info "no checksum file published; skipping verification"
  fi

  tar -C "$tmp" -xzf "$tmp/$asset" || err "failed to extract $asset"
  # Archive layout is rinne-<target>/rinne (see .github/workflows/release.yml).
  src="$tmp/rinne-${target}/$BIN"
  [ -f "$src" ] || src="$(find "$tmp" -type f -name "$BIN" -print | head -n1)"
  [ -f "$src" ] || err "binary '$BIN' not found in archive"

  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$src" "$INSTALL_DIR/$BIN" 2>/dev/null \
    || { cp "$src" "$INSTALL_DIR/$BIN" && chmod 0755 "$INSTALL_DIR/$BIN"; }

  info "installed to $INSTALL_DIR/$BIN"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) info "note: $INSTALL_DIR is not on your PATH. Add this to your shell profile:"
       printf '\n    export PATH="%s:$PATH"\n\n' "$INSTALL_DIR" ;;
  esac

  "$INSTALL_DIR/$BIN" --version >/dev/null 2>&1 \
    && info "run 'rinne --help' to get started" \
    || info "installed; ensure $INSTALL_DIR is on PATH, then run 'rinne --help'"
}

main "$@"
