#!/usr/bin/env bash
# Build the Linux/macOS headless archive (WI-M8-06): the service
# (wrongindexd) and the CLI (wrongindex), without the desktop window. An
# unsupported preview: neither platform is supported yet (WI-ADR-024), and
# nothing here is signed.
#
#   scripts/release/package-unix.sh
#
# Needs bun (BUN=<path> names one) for the license inventory, the manifest
# and the archive. Output in <target dir>/release/unix (honours
# CARGO_TARGET_DIR): wrongindex-<version>-<triple>.tar.gz, SHA256SUMS.txt,
# RELEASE-MANIFEST.json and THIRD-PARTY-LICENSES.txt.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
bun=${BUN:-bun}
target=${CARGO_TARGET_DIR:-$root/target}
triple=$(rustc -vV | sed -n 's/^host: //p')
version=$(cargo pkgid --locked -p wrongindexd | sed 's/.*[#@]//')
case "$triple" in
  *-linux-*|*-apple-darwin) ;;
  *) echo "package-unix.sh builds the Linux or macOS archive; this host is $triple" >&2; exit 2 ;;
esac

# Reproducible binaries: source paths are mapped to fixed prefixes, so a
# binary names neither the checkout nor the Cargo home (the build account's
# name among them). Set for the target triple, the flags add to the
# configured ones; RUSTFLAGS would replace them. A space-separated list, so
# no mapped path may contain a space.
cargo_home=${CARGO_HOME:-$HOME/.cargo}
flags="--remap-path-prefix=$root=/wrongindex --remap-path-prefix=$cargo_home/registry/src=/cargo/registry/src --remap-path-prefix=$cargo_home/git/checkouts=/cargo/git/checkouts"
for p in "$root" "$cargo_home"; do
  case "$p" in *' '*) echo "a path to map contains a space: $p" >&2; exit 1 ;; esac
done
flags_var="CARGO_TARGET_$(printf '%s' "$triple" | tr '[:lower:]-' '[:upper:]_')_RUSTFLAGS"
if [ -n "${!flags_var:-}" ]; then
  echo "$flags_var is already set; it would be replaced" >&2
  exit 1
fi
export "$flags_var=$flags"

# The shipped binaries must both be this version: Cargo can keep an earlier
# build "fresh" after a version is switched back, so check, and rebuild
# clean once.
built_versions() {
  local d c
  d=$("$target/release/wrongindexd" --build-info | "$bun" -e 'console.log(JSON.parse(await Bun.stdin.text()).version)')
  c=$("$target/release/wrongindex" --version | awk '{print $NF}')
  echo "$d $c"
}
build() { cargo build --release --locked -p wrongindexd -p wrongindex-cli; }
build
if [ "$(built_versions)" != "$version $version" ]; then
  echo "stale binaries ($(built_versions)) for version $version; rebuilding them"
  cargo clean --release -p wrongindexd -p wrongindex-cli
  build
  [ "$(built_versions)" = "$version $version" ] || { echo "built binaries are not version $version: $(built_versions)" >&2; exit 1; }
fi
# The mapping took effect: no shipped binary names this machine's paths.
for b in wrongindexd wrongindex; do
  for p in "$root" "$cargo_home"; do
    if grep -q -a -F "$p" "$target/release/$b"; then
      echo "$b contains the build path $p; the path mapping did not apply" >&2
      exit 1
    fi
  done
done

out="$target/release/unix"
name="wrongindex-$version-$triple"
rm -rf "$out"
mkdir -p "$out/stage/$name"
stage="$out/stage/$name"
cp "$target/release/wrongindexd" "$target/release/wrongindex" "$stage/"
chmod 755 "$stage/wrongindexd" "$stage/wrongindex"
cp LICENSE "$stage/LICENSE"
"$bun" scripts/release/manifest.mjs licenses --target "$triple" --headless --out "$stage/THIRD-PARTY-LICENSES.txt"
cat > "$stage/README.txt" <<EOF
WrongIndex $version for $triple: the service (wrongindexd) and the CLI
(wrongindex). Source: https://github.com/WrongStack/wrongindex

UNSUPPORTED PREVIEW. Linux and macOS are not supported platforms yet, there
is no desktop window, and these files are not signed. On macOS, a copy
downloaded with a browser is quarantined and Gatekeeper refuses to run it.

Keep both programs in one directory: the CLI starts the service next to it.

  ./wrongindex daemon start
  ./wrongindex project add <checkout>
  ./wrongindex search <project id> <text>
  ./wrongindex daemon stop

Start at sign-in: ./wrongindex daemon autostart on (off, status). The entry
names this directory; after moving it, run the command again.

Updates: wrongindex self-update installs the Windows installer only. Replace
this directory with a newer archive by hand (stop the service first).

Check a download: sha256sum -c SHA256SUMS.txt (macOS: shasum -a 256 -c), and
gh attestation verify <file> --repo WrongStack/wrongindex where the file was
attested.
EOF

# The archive, twice: the same inputs must give the same bytes.
epoch=$(git log -1 --format=%ct)
"$bun" scripts/release/tar.mjs --dir "$out/stage" --name "$name" --mtime "$epoch" --out "$out/$name.tar.gz" >/dev/null
"$bun" scripts/release/tar.mjs --dir "$out/stage" --name "$name" --mtime "$epoch" --out "$out/again.tar.gz" >/dev/null
cmp "$out/$name.tar.gz" "$out/again.tar.gz"
rm "$out/again.tar.gz"
cp "$stage/THIRD-PARTY-LICENSES.txt" "$out/"
rm -rf "$out/stage"

sum() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
(cd "$out" && sum "$name.tar.gz" > SHA256SUMS.txt)
"$bun" scripts/release/manifest.mjs archive --bin "$target/release" --archive "$out/$name.tar.gz" --target "$triple" --out "$out/RELEASE-MANIFEST.json"
cat "$out/SHA256SUMS.txt"
