# Build the per-user Windows installer (WI-700): WrongIndex desktop with the
# service (wrongindexd) and the CLI (wrongindex) installed next to it.
#
#   pwsh scripts/release/package-windows.ps1
#
# Needs the Tauri CLI (`cargo install tauri-cli --version "=2.12.4" --locked`);
# Tauri fetches or reuses NSIS. Output: <target dir>/release/bundle/nsis/*.exe
# (honours CARGO_TARGET_DIR) plus, next to it, SHA256SUMS.txt,
# RELEASE-MANIFEST.json and THIRD-PARTY-LICENSES.txt (WI-703; needs Node).
# The license inventory is also installed next to the binaries.
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $root

$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$triple = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
$version = ((cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).packages |
  Where-Object name -eq 'wrongindexd').version

# The shipped binaries must all be this version. Cargo keeps an earlier
# build "fresh" when a version is switched back and then leaves the binary of
# the other version in target/release, so check, and rebuild clean once.
function Get-BuiltVersions {
  [ordered]@{
    wrongindexd = (& (Join-Path $target 'release\wrongindexd.exe') --build-info | ConvertFrom-Json).version
    wrongindex  = ((& (Join-Path $target 'release\wrongindex.exe') --version) -split ' ')[-1]
  }
}
cargo build --release --locked -p wrongindexd -p wrongindex-cli
if ($LASTEXITCODE) { throw 'cargo build failed' }
$built = Get-BuiltVersions
if (@($built.Values | Where-Object { $_ -ne $version }).Count) {
  Write-Host "stale binaries ($(($built.GetEnumerator() | ForEach-Object { "$($_.Key) $($_.Value)" }) -join ', ')) for version $version; rebuilding them"
  cargo clean --release -p wrongindexd -p wrongindex-cli -p wrongindex-desktop
  cargo build --release --locked -p wrongindexd -p wrongindex-cli
  if ($LASTEXITCODE) { throw 'cargo build failed' }
  $built = Get-BuiltVersions
  if (@($built.Values | Where-Object { $_ -ne $version }).Count) { throw "built binaries are not version ${version}: $($built | ConvertTo-Json -Compress)" }
}

# Tauri sidecars are named <name>-<target triple>.exe at build time and
# installed as <name>.exe next to the desktop executable, which is where the
# desktop looks for the service and the CLI.
$bin = Join-Path $root 'crates\wrongindex-desktop\binaries'
New-Item -ItemType Directory -Force $bin | Out-Null
foreach ($name in 'wrongindexd', 'wrongindex') {
  Copy-Item (Join-Path $target "release\$name.exe") (Join-Path $bin "$name-$triple.exe") -Force
}

# Third-party licenses for what the binaries link; bundled as a resource
# (tauri.bundle.json) and published next to the installer.
node (Join-Path $root 'scripts\release\manifest.mjs') licenses --out (Join-Path $bin 'THIRD-PARTY-LICENSES.txt')
if ($LASTEXITCODE) { throw 'license inventory failed' }

Push-Location (Join-Path $root 'crates\wrongindex-desktop')
try {
  cargo tauri build --config tauri.bundle.json -- --locked
  if ($LASTEXITCODE) { throw 'cargo tauri build failed' }
} finally {
  Pop-Location
}
$desktopVersion = (Get-Item (Join-Path $target 'release\wrongindex-desktop.exe')).VersionInfo.ProductVersion
if ($desktopVersion -ne $version) { throw "wrongindex-desktop.exe is version $desktopVersion, expected $version" }

$out = Join-Path $target 'release\bundle\nsis'
$manifest = Get-ChildItem $out -Filter *.exe | ForEach-Object {
  "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
# LF line endings: `sha256sum -c SHA256SUMS.txt` must work on any system.
[IO.File]::WriteAllText((Join-Path $out 'SHA256SUMS.txt'), (($manifest -join "`n") + "`n"))
Copy-Item (Join-Path $bin 'THIRD-PARTY-LICENSES.txt') $out -Force
node (Join-Path $root 'scripts\release\manifest.mjs') manifest --bin (Join-Path $target 'release') --bundle $out --out (Join-Path $out 'RELEASE-MANIFEST.json')
if ($LASTEXITCODE) { throw 'release manifest failed' }
$manifest
