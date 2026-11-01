# Build the per-user Windows installer (WI-700): WrongIndex desktop with the
# service (wrongindexd) and the CLI (wrongindex) installed next to it.
#
#   pwsh scripts/release/package-windows.ps1
#
# Needs the Tauri CLI (`cargo install tauri-cli --version "=2.12.4" --locked`);
# Tauri fetches or reuses NSIS. Output: <target dir>/release/bundle/nsis/*.exe
# (honours CARGO_TARGET_DIR)
# plus a SHA-256 manifest next to it.
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $root

$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$triple = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
cargo build --release --locked -p wrongindexd -p wrongindex-cli
if ($LASTEXITCODE) { throw 'cargo build failed' }

# Tauri sidecars are named <name>-<target triple>.exe at build time and
# installed as <name>.exe next to the desktop executable, which is where the
# desktop looks for the service and the CLI.
$bin = Join-Path $root 'crates\wrongindex-desktop\binaries'
New-Item -ItemType Directory -Force $bin | Out-Null
foreach ($name in 'wrongindexd', 'wrongindex') {
  Copy-Item (Join-Path $target "release\$name.exe") (Join-Path $bin "$name-$triple.exe") -Force
}

Push-Location (Join-Path $root 'crates\wrongindex-desktop')
try {
  cargo tauri build --config tauri.bundle.json -- --locked
  if ($LASTEXITCODE) { throw 'cargo tauri build failed' }
} finally {
  Pop-Location
}

$out = Join-Path $target 'release\bundle\nsis'
$manifest = Get-ChildItem $out -Filter *.exe | ForEach-Object {
  "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
$manifest | Set-Content (Join-Path $out 'SHA256SUMS.txt')
$manifest
