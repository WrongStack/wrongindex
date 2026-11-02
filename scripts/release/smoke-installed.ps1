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
  $Installer = Get-ChildItem (Join-Path $target 'release\bundle\nsis') -Filter *-setup.exe | Select-Object -First 1 -ExpandProperty FullName
}
$existing = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
  ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' }
if ($existing) { throw "WrongIndex is already installed ($($existing.InstallLocation)); not touching it" }

$work = Join-Path ([IO.Path]::GetTempPath()) ("wi-smoke-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$dir = Join-Path $work 'app'
$home_ = Join-Path $work 'home'
$proj = Join-Path $work 'proj'
$shots = Join-Path $work 'shots'
New-Item -ItemType Directory -Force $work, $shots | Out-Null
Copy-Item -Recurse (Join-Path $root 'tests\fixtures\projects\treesitter-mix') $proj
$env:WRONGINDEX_HOME = $home_
$result = [ordered]@{ installer = $Installer; work = $work }
$desktop = $null
try {
  $p = Start-Process $Installer -ArgumentList '/S', "/D=$dir" -Wait -PassThru
  $result.install = $p.ExitCode
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
  $result.filesLeft = if (Test-Path $dir) { (Get-ChildItem $dir -Recurse -File | Measure-Object).Count } else { 0 }
  $result.uninstallEntryLeft = [bool](Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
    ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' })
  Remove-Item Env:WRONGINDEX_HOME -ErrorAction SilentlyContinue
}
$result | ConvertTo-Json -Depth 6
