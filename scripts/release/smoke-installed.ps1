# Packaged smoke (WI-408): install the built installer silently into a
# throw-away directory, run the INSTALLED service, CLI and desktop against a
# throw-away data root, drive the installed desktop's WebView2 over CDP, then
# stop everything and uninstall.
#
#   pwsh scripts/release/smoke-installed.ps1 -PlaywrightCore <dir with playwright-core index.mjs>
#
# Refuses to run when WrongIndex is already installed for this user (it would
# replace that installation's uninstall entry).
param(
  [Parameter(Mandatory)] [string] $PlaywrightCore,
  [string] $Installer,
  [int] $Port = 9333
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
if (-not $Installer) {
  $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
  $version = ((cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).packages | Where-Object name -eq 'wrongindexd').version
  $candidates = @(Get-ChildItem (Join-Path $target 'release\bundle\nsis') -Filter "WrongIndex_${version}_*-setup.exe")
  if ($candidates.Count -ne 1) { throw "expected one installer for $version, found $($candidates.Count)" }
  $Installer = $candidates[0].FullName
}
$existing = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
  ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' }
if ($existing) { throw "WrongIndex is already installed ($($existing.InstallLocation)); not touching it" }
# The installer remembers its directory; a later real install must not go to
# this test's deleted temporary directory.
Import-Module (Join-Path $PSScriptRoot 'InstallLocation.psm1') -Force
$installLocation = Save-InstallLocation

$work = Join-Path ([IO.Path]::GetTempPath()) ("wi-smoke-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$dir = Join-Path $work 'app'
$home_ = Join-Path $work 'home'
$proj = Join-Path $work 'proj'
$shots = Join-Path $work 'shots'
New-Item -ItemType Directory -Force $work, $shots | Out-Null
Copy-Item -Recurse (Join-Path $root 'tests\fixtures\projects\treesitter-mix') $proj
$env:WRONGINDEX_HOME = $home_
# WrongStack's index locations resolve into the work folder too: a click in
# the test's window (Add project, by a person or anything reaching its
# remote-debugging port) can never claim one of this machine's real indexes.
$env:WRONGSTACK_HOME = Join-Path $work 'wrongstack-home'
$result = [ordered]@{ installer = $Installer; work = $work }
$desktop = $null
try {
  $p = Start-Process $Installer -ArgumentList '/S', "/D=$dir" -Wait -PassThru
  $result.install = $p.ExitCode
  # The license inventory ships next to the binaries (WI-703).
  $result.licensesInstalled = Test-Path (Join-Path $dir 'THIRD-PARTY-LICENSES.txt')
  $result.nativeGuiInstalled = Test-Path (Join-Path $dir 'wrongindex-gpui.exe')
  if (-not $result.nativeGuiInstalled) { throw 'Native GPUI client missing from installer' }
  # The install directory goes on the user's PATH (and comes off on uninstall).
  $userPath = (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
  $result.onUserPath = @(($userPath -split ';') | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }).Count -gt 0
  $cli = Join-Path $dir 'wrongindex.exe'
  & $cli daemon start --wait-seconds 20 | Out-Null
  $add = & $cli --json project add $proj --index-dir (Join-Path $work 'idx') --watch pinned | ConvertFrom-Json
  $pid_ = $add.project.projectId
  # Pinned: claimed and indexed by the installed service.
  $deadline = (Get-Date).AddSeconds(90)
  do {
    Start-Sleep -Milliseconds 500
    $st = & $cli --json project status $pid_ | ConvertFrom-Json
  } until ($st.freshness -eq 'current' -or (Get-Date) -gt $deadline)
  $result.freshness = $st.freshness
  $result.generation = $st.generation

  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port"
  $desktop = Start-Process (Join-Path $dir 'wrongindex-desktop.exe') -PassThru
  $deadline = (Get-Date).AddSeconds(30)
  do {
    Start-Sleep -Milliseconds 500
    try { $ok = (Invoke-WebRequest "http://127.0.0.1:$Port/json/version" -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200 } catch { $ok = $false }
  } until ($ok -or (Get-Date) -gt $deadline)
  $gui = node (Join-Path $root 'scripts\release\smoke-gui.mjs') $PlaywrightCore $Port $pid_ $shots
  $result.guiExit = $LASTEXITCODE
  $result.gui = $gui | Select-Object -Last 1 | ConvertFrom-Json
  # What the GUI changed is what the service holds.
  $settings = & $cli --json settings get | ConvertFrom-Json
  $result.settingsRevision = $settings.revision
  $result.pausedAfterGui = $settings.settings.paused
  $result.parallelJobsAfterGui = $settings.settings.parallelJobs
} finally {
  if ($desktop -and -not $desktop.HasExited) { Stop-Process -Id $desktop.Id -Force }
  Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
  $cli = Join-Path $dir 'wrongindex.exe'
  if (Test-Path $cli) { & $cli daemon stop --wait-seconds 30 | Out-Null }
  Start-Sleep -Seconds 2
  $un = Join-Path $dir 'uninstall.exe'
  if (Test-Path $un) {
    $u = Start-Process $un -ArgumentList '/S' -Wait -PassThru
    $result.uninstall = $u.ExitCode
  }
  Start-Sleep -Seconds 3
  $userPath = (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
  $result.leftOnUserPath = @(($userPath -split ';') | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }).Count -gt 0
  $result.filesLeft = if (Test-Path $dir) { (Get-ChildItem $dir -Recurse -File | Measure-Object).Count } else { 0 }
  $result.uninstallEntryLeft = [bool](Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
    ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' })
  try { $result.installLocationRestored = Restore-InstallLocation $installLocation }
  catch { $result.installLocationRestored = $false; $result.installLocationError = $_.Exception.Message }
  Remove-Item Env:WRONGINDEX_HOME -ErrorAction SilentlyContinue
  Remove-Item Env:WRONGSTACK_HOME -ErrorAction SilentlyContinue
}
# Every expectation is checked here: the gate passes only when all hold.
$failures = @()
if ($result.install -ne 0) { $failures += "install exit $($result.install)" }
if (-not $result.licensesInstalled) { $failures += 'THIRD-PARTY-LICENSES.txt not installed' }
if (-not $result.onUserPath) { $failures += 'install directory not put on the user PATH' }
if ($result.leftOnUserPath) { $failures += 'install directory left on the user PATH after uninstall' }
if ($result.freshness -ne 'current') { $failures += "freshness $($result.freshness)" }
if ($result.guiExit -ne 0) { $failures += "GUI checks exit $($result.guiExit)" }
if ($result.pausedAfterGui -ne $false) { $failures += 'GUI pause not undone' }
if ($result.parallelJobsAfterGui -ne 2) { $failures += 'refused setting was applied' }
if ($result.uninstall -ne 0) { $failures += "uninstall exit $($result.uninstall)" }
if ($result.filesLeft -ne 0) { $failures += "$($result.filesLeft) files left after uninstall" }
if ($result.uninstallEntryLeft) { $failures += 'uninstall entry left' }
if (-not $result.installLocationRestored) { $failures += 'remembered install location not restored' }
$result.failures = $failures
$result | ConvertTo-Json -Depth 6
if ($failures.Count) { exit 1 }
