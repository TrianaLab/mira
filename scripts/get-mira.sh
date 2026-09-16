#!/usr/bin/env bash
# One-line installer for Mira.
#
#   curl -fsSL https://miradb.dev/install.sh | bash
#
# `docs/install.sh` is a symlink to this file, so the docs build publishes the
# same bytes at that URL — a symlink rather than a copy, because two installers
# is one installer that is wrong by the next release. The raw.githubusercontent
# URL still works and is the fallback if Pages is down; it is not advertised,
# because a URL with a branch name in it is a URL that pins nothing.
#
# The shape of it:
#
#   * Assets are named by Rust target triple (`mira-0.1.0-aarch64-apple-darwin`)
#     and each is a tarball with the binary one directory down, because that is
#     what release.yml publishes.
#   * There is no Windows build. Mira mmaps its blocks and the whole storage
#     layer is written against POSIX; a Windows port is not a build flag.
#   * There are no plugins to install alongside. Mira is one binary; that is the
#     product.
#   * Checksums live in SHA256SUMS, and every release also carries one SLSA
#     provenance attestation over that file. If the GitHub CLI is on PATH this
#     verifies it — that is a stronger statement than the checksum, because it
#     says *this workflow in this repository built these bytes* rather than just
#     "these bytes are the ones the server is serving".
set -euo pipefail

: "${BINARY_NAME:=mira}"
: "${USE_SUDO:=true}"
: "${MIRA_INSTALL_DIR:=/usr/local/bin}"
: "${REPO:=TrianaLab/mira}"
: "${API_URL:=https://api.github.com/repos/$REPO/releases}"

DOWNLOAD_DIR=""
DESIRED_VERSION="${DESIRED_VERSION:-}"
NO_VERIFY="false"

HAS_CURL="$(type curl >/dev/null 2>&1 && echo true || echo false)"
HAS_WGET="$(type wget >/dev/null 2>&1 && echo true || echo false)"

# Authenticate api.github.com calls when a token is available. Anonymous API
# access is limited to 60 requests/hour per IP and rate-limits on shared CI
# runners, which surfaces as a spurious "version not found" during tag lookup.
# Download URLs (the release CDN) are not rate-limited and are left
# unauthenticated on purpose.
#
# These two are expanded below through the `[@]+` form rather than plainly
# quoted, and that is not a style choice. Under `set -u`, bash before 4.4 treats
# an empty array expanded with `[@]` as an unbound variable and exits; macOS
# still ships 3.2.57 as /bin/bash, frozen there by bash 4.0's move to GPLv3, and
# `curl ... | bash` runs exactly that. So the failing case was the common one —
# a Mac, no token, empty array — and it failed before printing anything.
# The `+` form expands to nothing when the array is empty and to the quoted
# elements otherwise, which is what the plain expansion was meant to do.
GH_API_TOKEN="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
CURL_AUTH=()
WGET_AUTH=()
if [ -n "$GH_API_TOKEN" ]; then
  CURL_AUTH=(-H "Authorization: Bearer $GH_API_TOKEN")
  WGET_AUTH=(--header="Authorization: Bearer $GH_API_TOKEN")
fi

initArch() {
  ARCH=$(uname -m)
  case $ARCH in
    x86_64|amd64) ARCH="x86_64";;
    aarch64|arm64) ARCH="aarch64";;
    *) echo "Unsupported architecture: $ARCH" >&2; exit 1;;
  esac
}

initOS() {
  OS=$(uname | tr '[:upper:]' '[:lower:]')
  case "$OS" in
    linux)  VENDOR="unknown-linux-gnu";;
    darwin) VENDOR="apple-darwin";;
    *) echo "Unsupported OS: $OS. Mira builds for linux and macOS only." >&2; exit 1;;
  esac
  TARGET="${ARCH}-${VENDOR}"
}

runAsRoot() {
  if [ "$USE_SUDO" = "true" ] && [ "$(id -u)" -ne 0 ]; then
    sudo "$@"
  else
    "$@"
  fi
}

verifySupported() {
  supported="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin"
  if ! echo "$supported" | grep -qw "$TARGET"; then
    echo "No prebuilt binary for $TARGET" >&2
    exit 1
  fi
  if [ "$HAS_CURL" != "true" ] && [ "$HAS_WGET" != "true" ]; then
    echo "curl or wget is required" >&2
    exit 1
  fi
  # The linux tarballs are built on ubuntu-22.04, so the floor is glibc 2.34:
  # RHEL 9, Amazon Linux 2023, Debian 12, Ubuntu 22.04+. Warn rather than
  # refuse — `ldd --version` is not portable and being wrong about it is worse
  # than letting the dynamic loader say so itself, clearly, a second later.
  if [ "$OS" = "linux" ] && command -v ldd >/dev/null 2>&1; then
    glibc=$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)
    if [ -n "$glibc" ] && [ "$(printf '%s\n2.34\n' "$glibc" | sort -V | head -1)" != "2.34" ]; then
      echo "Warning: glibc $glibc detected; the published binaries need 2.34 or newer." >&2
      echo "  On Alpine or an older distro, build from source: see docs/install.md" >&2
    fi
  fi
}

fetch() {
  # $1 url, $2 output path ("-" for stdout)
  if [ "$HAS_CURL" = "true" ]; then
    if [ "$2" = "-" ]; then curl -fsSL ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} "$1"; else curl -fsSL "$1" -o "$2"; fi
  else
    if [ "$2" = "-" ]; then wget ${WGET_AUTH[@]+"${WGET_AUTH[@]}"} -qO- "$1"; else wget -qO "$2" "$1"; fi
  fi
}

checkDesiredVersion() {
  if [ -z "$DESIRED_VERSION" ]; then
    # `|| true`, because every part of this pipeline fails for an ordinary
    # reason: `curl -f` on a 404 when the repository has no release yet, and
    # `grep` with no match when the API answered with a rate-limit document
    # instead. Without it `set -o pipefail` aborts the substitution and the
    # message below — the one that says which of those it was — never prints.
    TAG=$(fetch "$API_URL/latest" - 2>/dev/null | grep -E '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || true)
    if [ -z "$TAG" ]; then
      echo "No release found for $REPO." >&2
      echo "  Either the repository has not published one yet, or the anonymous" >&2
      echo "  API limit (60/hour per IP) was hit — set GH_TOKEN and retry to rule" >&2
      echo "  that out. To build from source instead: https://miradb.dev/install/" >&2
      exit 1
    fi
  else
    TAG="$DESIRED_VERSION"
    status_code=0
    if [ "$HAS_CURL" = "true" ]; then
      status_code=$(curl -sSL ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -o /dev/null -w "%{http_code}" "$API_URL/tags/$TAG")
    else
      status_code=$(wget ${WGET_AUTH[@]+"${WGET_AUTH[@]}"} --server-response --spider -q "$API_URL/tags/$TAG" 2>&1 | awk '/HTTP\//{print $2}')
    fi
    if [ "$status_code" != "200" ]; then
      echo "Version $TAG not found in $REPO releases" >&2
      exit 1
    fi
  fi
  VERSION="${TAG#v}"
}

checkInstalledVersion() {
  if [ -f "$MIRA_INSTALL_DIR/$BINARY_NAME" ]; then
    INSTALLED=$("$MIRA_INSTALL_DIR/$BINARY_NAME" --version 2>/dev/null || true)
    if echo "$INSTALLED" | grep -qw "$VERSION"; then
      echo "$BINARY_NAME $TAG is already installed"
      exit 0
    fi
  fi
}

# The checksum says the bytes are intact. The attestation says who built them.
# Both are best-effort on the client: a user without `gh`, or behind a proxy that
# eats the API, still gets a working install and a clear line saying which of the
# two checks did not run. Silence would be the wrong default in both directions —
# refusing to install is user-hostile, and pretending it verified is worse.
verifyChecksum() {
  local file="$1" filename="$2"
  local url="https://github.com/$REPO/releases/download/$TAG/SHA256SUMS"
  tmp_checksums="$(mktemp)"
  fetch "$url" "$tmp_checksums" 2>/dev/null || true

  if [ ! -s "$tmp_checksums" ]; then
    rm -f "$tmp_checksums"
    echo "Warning: SHA256SUMS not available, skipping verification" >&2
    return 0
  fi
  expected=$(grep -F "$filename" "$tmp_checksums" | awk '{print $1}')
  rm -f "$tmp_checksums"
  if [ -z "$expected" ]; then
    echo "Warning: no checksum found for $filename, skipping verification" >&2
    return 0
  fi

  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$file" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$file" | awk '{print $1}')
  else
    echo "Warning: sha256sum/shasum not found, skipping verification" >&2
    return 0
  fi

  if [ "$expected" != "$actual" ]; then
    echo "Checksum verification failed for $filename" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    return 1
  fi
  echo "  sha256 ok"
}

verifyProvenance() {
  local file="$1"
  if [ "$NO_VERIFY" = "true" ]; then
    return 0
  fi
  if ! command -v gh >/dev/null 2>&1; then
    echo "  provenance not checked (no gh CLI); verify later with:" >&2
    echo "    gh attestation verify $MIRA_INSTALL_DIR/$BINARY_NAME --repo $REPO" >&2
    return 0
  fi
  if gh attestation verify "$file" --repo "$REPO" >/dev/null 2>&1; then
    echo "  slsa provenance ok"
  else
    echo "Warning: provenance verification failed or unavailable for this release." >&2
    echo "  re-run manually: gh attestation verify <file> --repo $REPO" >&2
  fi
}

downloadFile() {
  name="${BINARY_NAME}-${VERSION}-${TARGET}"
  filename="${name}.tar.gz"
  url="https://github.com/$REPO/releases/download/$TAG/$filename"
  tmp="$(mktemp -d)"
  DOWNLOAD_DIR="$tmp"
  fetch "$url" "$tmp/$filename"
  verifyChecksum "$tmp/$filename" "$filename"
  verifyProvenance "$tmp/$filename"
  tar -xzf "$tmp/$filename" --strip-components=1 -C "$tmp" "$name/$BINARY_NAME"
  chmod +x "$tmp/$BINARY_NAME"
}

installFile() {
  if [ ! -d "$MIRA_INSTALL_DIR" ]; then
    echo "Install directory $MIRA_INSTALL_DIR does not exist" >&2
    exit 1
  fi
  runAsRoot mv "$DOWNLOAD_DIR/$BINARY_NAME" "$MIRA_INSTALL_DIR/"
  echo "$BINARY_NAME installed to $MIRA_INSTALL_DIR/$BINARY_NAME"
  echo
  echo "  mira --data-dir ./data          # OTLP/gRPC 4317, OTLP/HTTP + UI + MCP 4318"
  echo "  mira mira --data-dir ./data     # the same views in the terminal"
}

help() {
  echo "Usage: get-mira.sh [--version <version>] [--no-sudo] [--no-verify] [--help]"
  echo "  --version, -v  specify version (e.g. v0.1.0); default: latest release"
  echo "  --no-sudo      disable sudo for installation"
  echo "  --no-verify    skip SLSA provenance verification (the checksum is still checked)"
  echo "  --help, -h     show help"
  echo ""
  echo "Environment:"
  echo "  MIRA_INSTALL_DIR  install directory (default: /usr/local/bin); must exist"
  echo "  GH_TOKEN          GitHub token for version lookup, to avoid the"
  echo "                    anonymous API rate limit (60 requests/hour per IP)"
}

# Must end on a successful command: bash takes the exit status of the last
# command in an EXIT trap as the script's status, so a bare failing test here
# turns `--help` and "already installed" (both `exit 0`) into exit 1.
cleanup() {
  if [ -n "$DOWNLOAD_DIR" ]; then
    rm -rf "$DOWNLOAD_DIR"
  fi
  return 0
}

trap cleanup EXIT

while [ $# -gt 0 ]; do
  case $1 in
    --version|-v)
      shift
      if [ -n "${1:-}" ]; then
        DESIRED_VERSION="$1"
      else
        echo "Expected version after --version" >&2
        exit 1
      fi
      ;;
    --no-sudo)   USE_SUDO="false";;
    --no-verify) NO_VERIFY="true";;
    --help|-h)   help; exit 0;;
    *)           echo "Unknown option: $1" >&2; help; exit 1;;
  esac
  shift
done

initArch
initOS
verifySupported
checkDesiredVersion

echo "Installing $BINARY_NAME $TAG ($TARGET)..."
checkInstalledVersion
downloadFile
installFile
