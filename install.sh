#!/bin/sh
# WrongIndex installer for POSIX shells on Windows (Git Bash, MSYS2, Cygwin):
#
#   curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.sh | sh
#
# It runs install.ps1 (the one installer implementation): download the
# release from GitHub, verify its SHA-256 (and its build-provenance
# attestation when `gh` is available), install for the current user and put
# `wrongindex` on the user's PATH. Options, as environment variables:
#
#   WRONGINDEX_VERSION=v0.1.0      release tag (default: latest)
#   WRONGINDEX_INSTALL_DIR=...     install directory (Windows path)
#   WRONGINDEX_REPO=owner/name     GitHub repository
#
# WrongIndex runs on Windows only for now; on Linux and macOS this exits
# without changing anything.
set -eu

repo="${WRONGINDEX_REPO:-WrongStack/wrongindex}"
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *)
    echo "wrongindex-install: WrongIndex currently runs on Windows only (nothing was changed)." >&2
    exit 1
    ;;
esac

if ! command -v powershell.exe >/dev/null 2>&1; then
  echo "wrongindex-install: powershell.exe not found on PATH" >&2
  exit 1
fi

work="$(mktemp -d 2>/dev/null || mktemp -d -t wrongindex)"
trap 'rm -rf "$work"' EXIT INT TERM

# Run as a file from a checkout (`sh install.sh`): use the install.ps1 next to
# it. Piped into sh, $0 is the shell, and an install.ps1 in the current
# directory is not ours: fetch it from the repository instead.
local_ps1=""
case "$0" in
  *install.sh)
    if [ -f "$0" ]; then
      local_ps1="$(cd "$(dirname "$0")" 2>/dev/null && pwd || true)/install.ps1"
    fi
    ;;
esac
if [ -n "$local_ps1" ] && [ -f "$local_ps1" ]; then
  cp "$local_ps1" "$work/install.ps1"
else
  ref="${WRONGINDEX_REF:-main}"
  curl -fsSL "https://raw.githubusercontent.com/$repo/$ref/install.ps1" -o "$work/install.ps1"
fi

script="$work/install.ps1"
if command -v cygpath >/dev/null 2>&1; then
  script="$(cygpath -w "$script")"
fi
WRONGINDEX_REPO="$repo" powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$script"
