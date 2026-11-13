# Run the existing real six-project soak against a frozen daemon copy.
# This script is the hidden worker, launched with Start-Process; it does not
# alter power settings, production data, registration or autostart.
param(
  [Parameter(Mandatory)] [string] $Daemon,
  [Parameter(Mandatory)] [string] $TestExecutable,
  [Parameter(Mandatory)] [string] $OutDir,
  [ValidateRange(30, 604800)] [int] $Seconds = 86400
)
$ErrorActionPreference = 'Stop'
$daemonFile = (Resolve-Path -LiteralPath $Daemon).Path
$testFile = (Resolve-Path -LiteralPath $TestExecutable).Path
if (Test-Path -LiteralPath $OutDir) { throw 'Use a fresh output directory for each soak' }
$output = New-Item -ItemType Directory -Path $OutDir
$subject = New-Item -ItemType Directory -Path (Join-Path $output.FullName 'subject')
$frozenDaemon = Join-Path $subject.FullName 'wrongindexd.exe'
$frozenTest = Join-Path $subject.FullName 'soak.exe'
Copy-Item -LiteralPath $daemonFile -Destination $frozenDaemon
Copy-Item -LiteralPath $testFile -Destination $frozenTest
$statusFile = Join-Path $output.FullName 'status.json'
$stdout = Join-Path $output.FullName 'stdout.log'
$stderr = Join-Path $output.FullName 'stderr.log'
$state = [ordered]@{
  schema = 'wrongindex.soak-run/1'
  status = 'RUNNING'
  boundary = 'Native six-project JSON/Rust/TypeScript soak; excludes GPUI/helper-language/installed-product qualification'
  workerPid = $PID
  startedAt = [DateTimeOffset]::UtcNow.ToString('o')
  requestedSeconds = $Seconds
  daemonSha256 = (Get-FileHash -LiteralPath $frozenDaemon -Algorithm SHA256).Hash.ToLowerInvariant()
  testSha256 = (Get-FileHash -LiteralPath $frozenTest -Algorithm SHA256).Hash.ToLowerInvariant()
  sourceBuildInfo = ((& $frozenDaemon --build-info) | ConvertFrom-Json)
}
$start = [DateTimeOffset]::UtcNow
try {
  $env:WRONGINDEX_SOAK_SECS = "$Seconds"
  $env:WRONGINDEX_TEST_DAEMON = $frozenDaemon
  $process = Start-Process -FilePath $frozenTest -ArgumentList '--ignored','--nocapture','--exact','many_projects_under_steady_edits_plateau_and_converge' -WorkingDirectory $output.FullName -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
  $null = $process.Handle
  $state.testPid = $process.Id
  $state | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $statusFile -Encoding utf8
  $process.WaitForExit()
  $state.exitCode = $process.ExitCode
  $state.elapsedSeconds = ([DateTimeOffset]::UtcNow - $start).TotalSeconds
  $log = (Get-Content -LiteralPath $stdout,$stderr -Raw) -join "`n"
  $state.status = if ($log -match 'NOT RUN:') { 'NOT RUN' } elseif ($process.ExitCode -eq 0 -and $state.elapsedSeconds -ge $Seconds -and $log -match '1 passed; 0 failed') { 'PASS' } else { 'FAIL' }
  $state.is24HourRun = $state.status -eq 'PASS' -and $Seconds -ge 86400
} catch {
  $state.status = 'FAIL'
  $state.error = $_.Exception.Message
} finally {
  $state.finishedAt = [DateTimeOffset]::UtcNow.ToString('o')
  $state | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $statusFile -Encoding utf8
}
if ($state.status -ne 'PASS') { exit 1 }
