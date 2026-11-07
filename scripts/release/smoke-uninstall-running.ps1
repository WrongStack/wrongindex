# Upgrade, self-update and uninstall while WrongIndex runs, as a user would:
# the service holds a watched project's index, the desktop window is open,
# and a second service runs from the same installation with another data
# root (`--data-root`), which the installer's graceful `daemon stop` does not
# reach.
#
# 1. The installer runs again over the installation (an upgrade in place):
#    it succeeds and starts the service again (it was running).
# 2. `wrongindex self-update --version v<this version>` runs with everything
#    running again, against a local release server (Node, on 127.0.0.1,
#    through WRONGINDEX_UPDATE_URL) that offers this installer: it ends the
#    other data root's service and names it, installs, starts the service
#    again and reopens the desktop.
# 3. `uninstall.exe /S` runs with everything running again: it stops both
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
# WrongStack's index locations resolve into the work folder too: a click in
# the test's window (Add project, by a person or anything reaching its
# remote-debugging port) can never claim one of this machine's real indexes.
$env:WRONGSTACK_HOME = Join-Path $work 'wrongstack-home'
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
# A release of this version, as the GitHub API shows one, served from a
# directory by Node on a free local port.
$version = [regex]::Match((Split-Path -Leaf $Installer), 'WrongIndex_(.+)_x64-setup\.exe').Groups[1].Value
$srv = Join-Path $work 'release-server'
$server = $null
function Start-ReleaseServer {
  $tag = "v$version"
  $name = Split-Path -Leaf $Installer
  New-Item -ItemType Directory -Force (Join-Path $srv "dl\$tag"), (Join-Path $srv 'releases\tags') | Out-Null
  Copy-Item $Installer (Join-Path $srv "dl\$tag\$name")
  $hash = (Get-FileHash $Installer -Algorithm SHA256).Hash.ToLower()
  [IO.File]::WriteAllText((Join-Path $srv "dl\$tag\SHA256SUMS.txt"), "$hash  $name`n")
  $portFile = Join-Path $work 'release-server.port'
  $js = @'
const http = require('node:http'), fs = require('node:fs'), path = require('node:path');
const [root, portFile] = process.argv.slice(2);
const s = http.createServer((req, res) => {
  const file = path.join(root, decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (!file.startsWith(root) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { res.writeHead(404); return res.end('{"message":"Not Found"}'); }
  res.writeHead(200); fs.createReadStream(file).pipe(res);
});
s.listen(0, '127.0.0.1', () => fs.writeFileSync(portFile, String(s.address().port)));
'@
  $jsFile = Join-Path $work 'release-server.js'
  [IO.File]::WriteAllText($jsFile, $js)
  $p = Start-Process node -ArgumentList "`"$jsFile`"", "`"$srv`"", "`"$portFile`"" -PassThru -WindowStyle Hidden
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while (-not (Test-Path $portFile) -and $sw.Elapsed.TotalSeconds -lt 15) { Start-Sleep -Milliseconds 100 }
  $base = "http://127.0.0.1:$(Get-Content $portFile)"
  $release = [ordered]@{
    tag_name = $tag; draft = $false; html_url = "$base/release/$tag"
    assets = @(
      @{ name = 'SHA256SUMS.txt'; browser_download_url = "$base/dl/$tag/SHA256SUMS.txt" },
      @{ name = $name; browser_download_url = "$base/dl/$tag/$name" }
    )
  }
  [IO.File]::WriteAllText((Join-Path $srv "releases\tags\$tag"), ($release | ConvertTo-Json -Depth 4))
  $env:WRONGINDEX_UPDATE_URL = "$base/releases"
  $p
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
  # 2. A self-update with everything running again.
  & $cli --data-root $other daemon start --wait-seconds 30 | Out-Null
  $otherPid = (Get-Content (Join-Path $other 'runtime\endpoint.json') -Raw | ConvertFrom-Json).pid
  $desktop = Start-Process (Join-Path $dir 'wrongindex-desktop.exe') -PassThru
  Start-Sleep -Seconds 5
  $result.runningBeforeSelfUpdate = @(Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | ForEach-Object { $_.Name } | Sort-Object)
  $server = Start-ReleaseServer
  $errFile = Join-Path $work 'self-update.err'
  $updated = & $cli --json self-update --version "v$version" 2> $errFile
  $result.selfUpdateExit = $LASTEXITCODE
  $result.selfUpdate = try { $updated | ConvertFrom-Json } catch { "$updated $(Get-Content $errFile -Raw)" }
  $result.selfUpdateEndedOther = @($result.selfUpdate.stopped | Where-Object { $_.pid -eq $otherPid -and $_.program -like '*\wrongindexd.exe' }).Count -eq 1
  Start-Sleep -Seconds 5
  $result.runningAfterSelfUpdate = @(Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | ForEach-Object { $_.Name } | Sort-Object)
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ((& $cli --json project status $id | ConvertFrom-Json).freshness -ne 'current' -and $sw.Elapsed.TotalSeconds -lt 120) { Start-Sleep -Milliseconds 300 }
  $result.freshnessAfterSelfUpdate = (& $cli --json project status $id | ConvertFrom-Json).freshness
  # Everything running again for the uninstall.
  & $cli --data-root $other daemon start --wait-seconds 30 | Out-Null
  if (-not (Get-Process wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ })) {
    $desktop = Start-Process (Join-Path $dir 'wrongindex-desktop.exe') -PassThru
  }
  Start-Sleep -Seconds 5
  $result.runningBeforeUninstall = @(Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | ForEach-Object { $_.Name } | Sort-Object)

  # 3. The user uninstalls with everything running.
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
  # The copy in %TEMP% removes the uninstall entry last, after the files:
  # give it the time the files had.
  $entry = { [bool](Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
      ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like 'WrongIndex*' }) }
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ((& $entry) -and $sw.Elapsed.TotalSeconds -lt 60) { Start-Sleep -Milliseconds 500 }
  $result.uninstallEntryLeft = & $entry
  $result.dataRootKept = Test-Path (Join-Path $home_ 'registry.db')
  $result.indexKept = Test-Path (Join-Path $idx 'index.db')
} finally {
  # Whatever is left of this throw-away installation goes.
  Get-Process wrongindexd, wrongindex-desktop -ErrorAction SilentlyContinue | Where-Object { & $inDir $_ } | Stop-Process -Force -ErrorAction SilentlyContinue
  if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
  Remove-Item Env:WRONGINDEX_HOME -ErrorAction SilentlyContinue
  Remove-Item Env:WRONGSTACK_HOME -ErrorAction SilentlyContinue
  Remove-Item Env:WRONGINDEX_UPDATE_URL -ErrorAction SilentlyContinue
}
$failures = @()
if ($result.install -ne 0) { $failures += "install exit $($result.install)" }
if ($result.freshnessBefore -ne 'current') { $failures += "project not current before the uninstall ($($result.freshnessBefore))" }
if (($result.runningBefore -join ',') -ne 'wrongindex-desktop,wrongindexd,wrongindexd') { $failures += "not running before the uninstall (desktop and two services expected): $($result.runningBefore -join ', ')" }
if ($result.upgrade -ne 0) { $failures += "upgrade exit $($result.upgrade)" }
if (-not $result.serviceBackAfterUpgrade) { $failures += 'service not running again after the upgrade' }
if ($result.freshnessAfterUpgrade -ne 'current') { $failures += "project not current after the upgrade ($($result.freshnessAfterUpgrade))" }
if (($result.runningBeforeSelfUpdate -join ',') -ne 'wrongindex-desktop,wrongindexd,wrongindexd') { $failures += "not running before the self-update: $($result.runningBeforeSelfUpdate -join ', ')" }
if ($result.selfUpdateExit -ne 0) { $failures += "self-update exit $($result.selfUpdateExit): $($result.selfUpdate)" }
if ($result.selfUpdate.updated -ne $true -or $result.selfUpdate.to -ne $version) { $failures += "self-update did not install $version" }
if ($result.selfUpdate.serviceRestarted -ne $true) { $failures += 'service not restarted by the self-update' }
if ($result.selfUpdate.desktopReopened -ne $true) { $failures += 'desktop not reopened by the self-update' }
if (-not $result.selfUpdateEndedOther) { $failures += "the other data root's service was not ended and named by the self-update" }
if (($result.runningAfterSelfUpdate -join ',') -ne 'wrongindex-desktop,wrongindexd') { $failures += "after the self-update (desktop and one service expected): $($result.runningAfterSelfUpdate -join ', ')" }
if ($result.freshnessAfterSelfUpdate -ne 'current') { $failures += "project not current after the self-update ($($result.freshnessAfterSelfUpdate))" }
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
