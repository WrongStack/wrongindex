# Rebuild a published release from its tag and compare the binaries with the
# ones it ships (the hashes in its RELEASE-MANIFEST.json).
#
#   pwsh scripts/release/reproduce.ps1 -Tag v0.1.7
#
# Builds in a new temporary directory (a git worktree of the tag and its own
# target directory) with scripts/release/package-windows.ps1, the script the
# release workflow runs, then removes it unless -Keep is given. Needs what
# that script needs (the pinned Rust toolchain, the Tauri CLI at the version
# the manifest records, Node, 7-Zip) and Visual C++ build tools: SQLite is C
# code, so a different compiler version gives different bytes. Both are
# compared and named when they differ.
#
# Releases built before the path mapping in package-windows.ps1 (0.1.6 and
# earlier) carry the build machine's paths and link time; they cannot match.
#
# Exit code 0 when every binary matches, 1 when one differs, 2 on a failure.
param(
  [Parameter(Mandatory)][string]$Tag,
  [string]$Repo = 'WrongStack/wrongindex',
  [switch]$Keep
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ('wrongindex-reproduce-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$src = Join-Path $work 'src'
$code = 2
$targetBefore = $env:CARGO_TARGET_DIR
try {
  $headers = @{ 'User-Agent' = 'wrongindex-reproduce'; 'Accept' = 'application/vnd.github+json' }
  $release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/tags/$Tag" -Headers $headers
  $asset = $release.assets | Where-Object name -eq 'RELEASE-MANIFEST.json' | Select-Object -First 1
  if (-not $asset) { throw "release $Tag has no RELEASE-MANIFEST.json" }
  $published = Invoke-RestMethod $asset.browser_download_url -Headers @{ 'User-Agent' = 'wrongindex-reproduce' }

  New-Item -ItemType Directory -Force $work | Out-Null
  git -C $root fetch --quiet origin "refs/tags/${Tag}:refs/tags/$Tag" 2>$null
  git -C $root worktree add --detach --quiet $src $Tag
  if ($LASTEXITCODE) { throw "git worktree add $Tag failed" }
  $commit = (git -C $src rev-parse HEAD).Trim()
  if ($commit -ne $published.source.commit) { throw "$Tag is $commit here, but the release was built from $($published.source.commit)" }

  Write-Host "building $Tag ($commit) in $work"
  $env:CARGO_TARGET_DIR = Join-Path $work 'target'
  pwsh -NoProfile -File (Join-Path $src 'scripts\release\package-windows.ps1')
  if ($LASTEXITCODE) { throw 'package-windows.ps1 failed' }
  $local = Get-Content (Join-Path $env:CARGO_TARGET_DIR 'release\bundle\nsis\RELEASE-MANIFEST.json') -Raw | ConvertFrom-Json

  $compiler = { param($m) @($m.sqlite.compileOptions) | Where-Object { $_ -like 'COMPILER=*' } }
  foreach ($k in 'rustc', 'cargo', 'tauriCli') {
    if ($local.toolchain.$k -ne $published.toolchain.$k) { Write-Host "toolchain differs: $k here '$($local.toolchain.$k)', release '$($published.toolchain.$k)'" }
  }
  if ((& $compiler $local) -ne (& $compiler $published)) { Write-Host "C compiler differs: here $(& $compiler $local), release $(& $compiler $published)" }

  $differ = 0
  foreach ($p in $published.binaries.PSObject.Properties) {
    $mine = $local.binaries.($p.Name)
    if ($mine -and $mine.sha256 -eq $p.Value.sha256) {
      Write-Host ("MATCH   {0}  {1}" -f $p.Name, $p.Value.sha256)
    } else {
      $differ++
      Write-Host ("DIFFERS {0}  release {1}, here {2}" -f $p.Name, $p.Value.sha256, $(if ($mine) { $mine.sha256 } else { '(not built)' }))
    }
  }
  $code = if ($differ) { 1 } else { 0 }
} catch {
  Write-Host "reproduce: $_"
  $code = 2
} finally {
  $env:CARGO_TARGET_DIR = $targetBefore
  if (-not $Keep) {
    git -C $root worktree remove --force $src 2>$null
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
  } else {
    Write-Host "kept $work"
  }
}
exit $code
