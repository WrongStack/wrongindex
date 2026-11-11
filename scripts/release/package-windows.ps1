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

# Reproducible binaries: the same commit and toolchain give the same bytes on
# any machine. Without this a binary carried the checkout's and the Cargo
# home's absolute paths (panic locations; the build account's name among
# them), whose lengths shifted everything after them, and the link time with
# a random PDB id. Source paths are mapped to fixed prefixes, and the linker
# writes content hashes for the time stamp and the PDB id (/Brepro). Set for
# the target triple, these flags are added to the configured ones
# (`+crt-static`); RUSTFLAGS would replace them. The variable is a
# space-separated list, so no mapped path may contain a space.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$reproFlags = @(
  "--remap-path-prefix=$($root.Path)=/wrongindex",
  "--remap-path-prefix=$(Join-Path $cargoHome 'registry\src')=/cargo/registry/src",
  "--remap-path-prefix=$(Join-Path $cargoHome 'git\checkouts')=/cargo/git/checkouts",
  '-Clink-arg=/Brepro'
)
if ($reproFlags -match ' ') { throw "a path to map contains a space: $($reproFlags -join ' | ')" }
# Invariant upper case: under a Turkish locale ToUpper() turns the `i` of
# `windows` into `İ`, a variable Cargo never reads (the flags were silently
# dropped on such a machine).
$flagsVar = "CARGO_TARGET_$($triple.ToUpperInvariant().Replace('-', '_'))_RUSTFLAGS"
if ($flagsVar -cnotmatch '^CARGO_TARGET_[A-Z0-9_]+_RUSTFLAGS$') { throw "unexpected variable name $flagsVar" }
if ([Environment]::GetEnvironmentVariable($flagsVar)) { throw "$flagsVar is already set; it would be replaced" }
[Environment]::SetEnvironmentVariable($flagsVar, ($reproFlags -join ' '))
try {

  # The shipped binaries must all be this version. Cargo keeps an earlier
  # build "fresh" when a version is switched back and then leaves the binary of
  # the other version in target/release, so check, and rebuild clean once.
  function Get-BuiltVersions {
    [ordered]@{
      wrongindexd = (& (Join-Path $target 'release\wrongindexd.exe') --build-info | ConvertFrom-Json).version
      wrongindex  = ((& (Join-Path $target 'release\wrongindex.exe') --version) -split ' ')[-1]
      # PowerShell does not capture a Windows-subsystem application's
      # output reliably. A piped native child also waits for its exit.
      'wrongindex-gpui' = ((node -e 'process.stdout.write(require("node:child_process").execFileSync(process.argv[1], ["--version"], {windowsHide:true}));' (Join-Path $target 'release\wrongindex-gpui.exe')) -split ' ')[-1]
    }
  }
  cargo build --release --locked -p wrongindexd -p wrongindex-cli -p wrongindex-gpui
  if ($LASTEXITCODE) { throw 'cargo build failed' }
  $built = Get-BuiltVersions
  if (@($built.Values | Where-Object { $_ -ne $version }).Count) {
    Write-Host "stale binaries ($(($built.GetEnumerator() | ForEach-Object { "$($_.Key) $($_.Value)" }) -join ', ')) for version $version; rebuilding them"
    cargo clean --release -p wrongindexd -p wrongindex-cli -p wrongindex-desktop -p wrongindex-gpui
    cargo build --release --locked -p wrongindexd -p wrongindex-cli -p wrongindex-gpui
    if ($LASTEXITCODE) { throw 'cargo build failed' }
    $built = Get-BuiltVersions
    if (@($built.Values | Where-Object { $_ -ne $version }).Count) { throw "built binaries are not version ${version}: $($built | ConvertTo-Json -Compress)" }
  }

  # Tauri sidecars are named <name>-<target triple>.exe at build time and
  # installed as <name>.exe next to the desktop executable, which is where the
  # desktop looks for the service and the CLI.
  $bin = Join-Path $root 'crates\wrongindex-desktop\binaries'
  New-Item -ItemType Directory -Force $bin | Out-Null
  foreach ($name in 'wrongindexd', 'wrongindex', 'wrongindex-gpui') {
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
  # The mapping took effect: no shipped binary names this machine's paths.
  foreach ($name in 'wrongindexd.exe', 'wrongindex.exe', 'wrongindex-desktop.exe', 'wrongindex-gpui.exe') {
    $text = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes((Join-Path $target "release\$name")))
    foreach ($local in $root.Path, $cargoHome) {
      if ($text.IndexOf($local, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "$name contains the build path $local; the path mapping did not apply" }
    }
  }
  $desktopVersion = (Get-Item (Join-Path $target 'release\wrongindex-desktop.exe')).VersionInfo.ProductVersion
  if ($desktopVersion -ne $version) { throw "wrongindex-desktop.exe is version $desktopVersion, expected $version" }

  $out = Join-Path $target 'release\bundle\nsis'
  # Old local installers may coexist in this directory. Certify only the
  # artifact just built, never an older version or a mixture of releases.
  $installers = @(Get-ChildItem $out -Filter "WrongIndex_${version}_*-setup.exe")
  if ($installers.Count -ne 1) { throw "expected one installer for $version, found $($installers.Count)" }
  $manifest = $installers | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name
  }
  # LF line endings: `sha256sum -c SHA256SUMS.txt` must work on any system.
  [IO.File]::WriteAllText((Join-Path $out 'SHA256SUMS.txt'), (($manifest -join "`n") + "`n"))
  Copy-Item (Join-Path $bin 'THIRD-PARTY-LICENSES.txt') $out -Force
  node (Join-Path $root 'scripts\release\manifest.mjs') manifest --bin (Join-Path $target 'release') --bundle $out --installer $installers[0].Name --out (Join-Path $out 'RELEASE-MANIFEST.json') --shipped-out (Join-Path $target 'release\bundle\shipped')
  if ($LASTEXITCODE) { throw 'release manifest failed' }
  $manifest
} finally {
  [Environment]::SetEnvironmentVariable($flagsVar, $null)
}
