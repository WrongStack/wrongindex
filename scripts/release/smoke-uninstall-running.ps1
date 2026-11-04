# Upgrade and uninstall while WrongIndex runs, as a user would: the service
# holds a watched project's index, the desktop window is open, and a second
# service runs from the same installation with another data root
# (`--data-root`), which the installer's graceful `daemon stop` does not
# reach.
#
# 1. The installer runs again over the installation (an upgrade in place):
#    it succeeds and starts the service again (it was running).
# 2. `uninstall.exe /S` runs with everything running again: it stops both
#    services, closes the desktop, removes every file, the PATH entry and
#    the uninstall entry, and keeps the data roots and the project's index.
#
#   pwsh scripts/release/smoke-uninstall-running.ps1 [-Installer <setup.exe>]
#
# Installs into a throw-away directory with a throw-away data root
# (WRONGINDEX_HOME), so nothing of an existing data root is touched. Refuses
# to run when WrongIndex is already installed (the uninstall entry would be
# replaced). Prints a JSON result; exits 1 when an expectation fails.
param(
  [string]$Installer
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $Installer) {
  $Installer = Get-ChildItem (Join-Path $root 'target\release\bundle\nsis') -Filter 'WrongIndex_*_x64-setup.exe' |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $Installer -or -not (Test-Path $Installer)) { throw 'no installer: build one (scripts/release/package-windows.ps1) or pass -Installer' }
$existing = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
  ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' }
if ($existing) { throw "WrongIndex is installed ($($existing.InstallLocation)); this smoke test would replace its uninstall entry" }

$work = Join-Path ([IO.Path]::GetTempPath()) ('wi-unrun-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$dir = Join-Path $work 'app'
$home_ = Join-Path $work 'home'
$other = Join-Path $work 'other-home'
$proj = Join-Path $work 'project'
$idx = Join-Path $work 'project-index'
New-Item -ItemType Directory -Force (Join-Path $proj 'src') | Out-Null
foreach ($i in 0..9) { [IO.File]::WriteAllText((Join-Path $proj "src\m$i.ts"), "export function fn_$i(): number { return $i; }`n") }
$env:WRONGINDEX_HOME = $home_
$result = [ordered]@{ installer = (Split-Path -Leaf $Installer) }
$inDir = { param($p) $p.Path -and $p.Path.StartsWith($dir, [StringComparison]::OrdinalIgnoreCase) }
# Wait for the program itself only: `Start-Process -Wait` also waits for
# what it started, and the installer starts the service, which runs on.
function Invoke-Program([string]$exe, [string[]]$arguments) {
  $p = Start-Process $exe -ArgumentList $arguments -PassThru
  $null = $p.Handle # keeps the exit code readable
  $p.WaitForExit()
  $p.ExitCode
}
try {
  $result.install = Invoke-Program $Installer @('/S', "/D=$dir")
  $cli = Join-Path $dir 'wrongindex.exe'
  & $cli daemon start --wait-seconds 30 | Out-Null
  $id = (& $cli --json project add $proj --index-dir $idx --watch pinned | ConvertFrom-Json).project.projectId
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ((& $cli --json project status $id | ConvertFrom-Json).freshness -ne 'current' -and $sw.Elapsed.TotalSeconds -lt 120) { Start-Sleep -Milliseconds 300 }
  $result.freshnessBefore = (& $cli --json project status $id | ConvertFrom-Json).freshness
  & $cli --data-root $other daemon start --wait-seconds 30 | Out-Null
  $desktop = Start-Process (Join-Path $dir 'wrongindex-desktop.exe') -PassThru
  Start-Sleep -Seconds 5
  $result.runningBefore = @(Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | ForEach-Object { $_.Name } | Sort-Object)

  # 1. An upgrade in place with everything running.
  $result.upgrade = Invoke-Program $Installer @('/S', "/D=$dir")
  $sw = [Diagnostics.Stopwatch]::StartNew()
  do {
    Start-Sleep -Milliseconds 500
    & $cli daemon status *> $null
    $back = $LASTEXITCODE -eq 0
  } until ($back -or $sw.Elapsed.TotalSeconds -gt 30)
  $result.serviceBackAfterUpgrade = $back
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ((& $cli --json project status $id | ConvertFrom-Json).freshness -ne 'current' -and $sw.Elapsed.TotalSeconds -lt 120) { Start-Sleep -Milliseconds 300 }
  $result.freshnessAfterUpgrade = (& $cli --json project status $id | ConvertFrom-Json).freshness
  # Everything running again for the uninstall.
  & $cli --data-root $other daemon start --wait-seconds 30 | Out-Null
  $desktop = Start-Process (Join-Path $dir 'wrongindex-desktop.exe') -PassThru
  Start-Sleep -Seconds 5
  $result.runningBeforeUninstall = @(Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | ForEach-Object { $_.Name } | Sort-Object)

  # 2. The user uninstalls with everything running.
  $result.uninstall = Invoke-Program (Join-Path $dir 'uninstall.exe') @('/S')
  # The uninstaller runs on from a copy in %TEMP%: wait for the directory
  # to empty.
  $sw = [Diagnostics.Stopwatch]::StartNew()
  do {
    Start-Sleep -Milliseconds 500
    $left = if (Test-Path $dir) { @(Get-ChildItem $dir -Recurse -File -ErrorAction SilentlyContinue).Count } else { 0 }
  } until ($left -eq 0 -or $sw.Elapsed.TotalSeconds -gt 60)
  $result.filesLeft = $left
  $result.filesLeftNames = if ($left) { @(Get-ChildItem $dir -Recurse -File | ForEach-Object { $_.Name }) } else { @() }
  $result.runningAfter = @(Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | ForEach-Object { "$($_.Name) $($_.Id)" })
  $userPath = (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
  $result.leftOnUserPath = @(($userPath -split ';') | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }).Count -gt 0
  $result.uninstallEntryLeft = [bool](Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
    ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' })
  $result.dataRootKept = Test-Path (Join-Path $home_ 'registry.db')
  $result.indexKept = Test-Path (Join-Path $idx 'index.db')
} finally {
  # Whatever is left of this throw-away installation goes.
  Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | Stop-Process -Force -ErrorAction SilentlyContinue
  Remove-Item Env:WRONGINDEX_HOME -ErrorAction SilentlyContinue
}
$failures = @()
if ($result.install -ne 0) { $failures += "install exit $($result.install)" }
if ($result.freshnessBefore -ne 'current') { $failures += "project not current before the uninstall ($($result.freshnessBefore))" }
if (($result.runningBefore -join ',') -ne 'wrongindex-desktop,wrongindexd,wrongindexd') { $failures += "not running before the uninstall (desktop and two services expected): $($result.runningBefore -join ', ')" }
if ($result.upgrade -ne 0) { $failures += "upgrade exit $($result.upgrade)" }
if (-not $result.serviceBackAfterUpgrade) { $failures += 'service not running again after the upgrade' }
if ($result.freshnessAfterUpgrade -ne 'current') { $failures += "project not current after the upgrade ($($result.freshnessAfterUpgrade))" }
if (($result.runningBeforeUninstall -join ',') -ne 'wrongindex-desktop,wrongindexd,wrongindexd') { $failures += "not running before the uninstall: $($result.runningBeforeUninstall -join ', ')" }
if ($result.uninstall -ne 0) { $failures += "uninstall exit $($result.uninstall)" }
if ($result.filesLeft -ne 0) { $failures += "$($result.filesLeft) files left: $($result.filesLeftNames -join ', ')" }
if (@($result.runningAfter).Count) { $failures += "still running: $($result.runningAfter -join ', ')" }
if ($result.leftOnUserPath) { $failures += 'install directory left on the user PATH' }
if ($result.uninstallEntryLeft) { $failures += 'uninstall entry left' }
if (-not $result.dataRootKept) { $failures += 'data root removed' }
if (-not $result.indexKept) { $failures += 'project index removed' }
$result.failures = $failures
$result | ConvertTo-Json -Depth 4
if ($failures.Count) { exit 1 }
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
