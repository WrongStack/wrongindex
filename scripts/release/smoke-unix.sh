#!/usr/bin/env bash
# Installed-artifact smoke of the Linux/macOS archive (WI-M8-06), the
# counterpart of smoke-installed-core.ps1: unpack the archive into a new
# temporary directory and use it as a user would, with HOME, the data root
# and WrongStack's home all inside that directory (no real profile, service
# or autostart entry is touched). Start the service, register a project,
# wait until its index is current, search it, check the commands that must
# refuse, turn autostart on and off, stop the service, and check that no
# process from the archive is left. Writes a JSON report.
#
#   scripts/release/smoke-unix.sh <archive.tar.gz> <report.json>
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: smoke-unix.sh <archive.tar.gz> <report.json>" >&2; exit 2; }
archive=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
report=$2
work=$(mktemp -d "${TMPDIR:-/tmp}/wi-unix-smoke-XXXXXX")
work=$(cd "$work" && pwd -P)
step=start
results=()
note() { results+=("\"$1\": \"$2\""); }

finish() {
  local code=$?
  local cli="${cli:-}"
  if [ -n "$cli" ] && [ -x "$cli" ]; then "$cli" daemon stop --wait-seconds 30 >/dev/null 2>&1 || true; fi
  # Whatever still runs from the unpacked archive is a leak; end it.
  local left=""
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    left=$(pgrep -f "$work/app/" || true)
    [ -z "$left" ] && break
    sleep 0.5
  done
  if [ -n "$left" ]; then
    note leftProcesses "$(echo "$left" | tr '\n' ' ')"
    pkill -KILL -f "$work/app/" || true
    [ "$code" -eq 0 ] && code=1
  fi
  local status=PASS
  [ "$code" -eq 0 ] || status="FAIL at $step"
  {
    printf '{\n  "status": "%s",\n  "archive": "%s",\n  "platform": "%s",\n' "$status" "$(basename "$archive")" "$(uname -sm)"
    printf '  "boundary": "temporary unpack/core/refusals/autostart-entry/stop proof; sign-in start, the compatibility worker and a clean machine NOT RUN"'
    for r in "${results[@]}"; do printf ',\n  %s' "$r"; done
    printf '\n}\n'
  } > "$report"
  cat "$report"
  rm -rf "$work"
  exit "$code"
}
trap finish EXIT

export HOME="$work/home"
export XDG_CONFIG_HOME="$HOME/.config"
export WRONGINDEX_HOME="$work/service"
export WRONGSTACK_HOME="$work/wrongstack"
unset WRONGINDEX_TEST_DAEMON
mkdir -p "$HOME" "$work/app" "$work/project"
printf 'export function installedNativeProof() { return 1; }\n' > "$work/project/entry.ts"

step=unpack
tar -xzf "$archive" -C "$work/app"
app=$(find "$work/app" -mindepth 1 -maxdepth 1 -type d)
[ "$(printf '%s\n' "$app" | wc -l | tr -d ' ')" = 1 ] || { echo "expected one directory in the archive" >&2; exit 1; }
cli="$app/wrongindex"
for f in wrongindexd wrongindex LICENSE README.txt THIRD-PARTY-LICENSES.txt; do
  [ -f "$app/$f" ] || { echo "missing from the archive: $f" >&2; exit 1; }
done
note version "$("$cli" --version)"

step=build-info
"$app/wrongindexd" --build-info | grep -q '"fts5Trigram": *true' || { echo "the service's SQLite lacks the FTS5 trigram tokenizer" >&2; exit 1; }
note sqlite PASS

step=start
"$cli" daemon start --wait-seconds 20 >/dev/null
note start PASS

step=index
registration=$("$cli" --json project add "$work/project" --index-dir "$work/index" --watch pinned)
id=$(printf '%s' "$registration" | grep -o '"projectId": *"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')
[ -n "$id" ] || { echo "no project id in: $registration" >&2; exit 1; }
deadline=$((SECONDS + 60))
until "$cli" --json project status "$id" | grep -q '"freshness": *"current"'; do
  [ $SECONDS -lt $deadline ] || { echo "the project did not become current" >&2; "$cli" --json project status "$id" >&2; exit 1; }
  sleep 0.3
done
[ -f "$work/index/index.db" ] || { echo "no index.db in the index directory" >&2; exit 1; }
note indexing PASS

step=search
"$cli" --json search "$id" installedNativeProof | grep -q installedNativeProof || { echo "search did not find the symbol" >&2; exit 1; }
note search PASS

step=refusals
set +e
"$cli" self-update --check >/dev/null 2>&1
su=$?
"$cli" setup add-to-path "$work/app" >/dev/null 2>&1
st=$?
set -e
[ "$su" = 2 ] && [ "$st" = 2 ] || { echo "self-update exited $su, setup $st; both must refuse with 2" >&2; exit 1; }
[ ! -e "$WRONGINDEX_HOME/updates" ] || { echo "self-update wrote an updates directory" >&2; exit 1; }
note refusals PASS

step=autostart
if [ "$(uname -s)" = Darwin ]; then entry="$HOME/Library/LaunchAgents/dev.wrongstack.wrongindex.plist"; else entry="$XDG_CONFIG_HOME/autostart/wrongindex.desktop"; fi
"$cli" daemon autostart on >/dev/null
[ -f "$entry" ] && grep -q -F "$cli" "$entry" || { echo "autostart on did not write $entry naming $cli" >&2; exit 1; }
"$cli" daemon autostart status >/dev/null
"$cli" daemon autostart off >/dev/null
[ ! -e "$entry" ] || { echo "autostart off left $entry" >&2; exit 1; }
note autostartEntry PASS

step=stop
"$cli" daemon stop --wait-seconds 30 >/dev/null
note stop PASS
step=done
