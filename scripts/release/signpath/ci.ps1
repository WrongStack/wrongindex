param(
  [Parameter(Mandatory)][ValidateSet('CheckConfig','Prepare','AcceptPayloads','StartBundle','WaitUninstaller','CompleteUninstaller','WaitInstaller','CompleteInstaller','FinishBundle','Abort')][string]$Mode,
  [string]$SignedDirectory
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
Set-Location $repo
$work = Join-Path $repo 'target/signpath-ci'
$courier = Join-Path $work 'courier'
$receiptFile = Join-Path $work 'inputs.json'
$names = @('wrongindex.exe','wrongindexd.exe','wrongindex-gpui.exe')
Import-Module (Join-Path $PSScriptRoot 'Signing.psm1') -Force
function Output-Value([string]$Name, [string]$Value) {
  if ($Value -match "[\r\n]") { throw 'Unsafe workflow output' }
  if ($env:GITHUB_OUTPUT) { Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "$Name=$Value" }
}
function Inputs { return Get-Content -LiteralPath $receiptFile -Raw | ConvertFrom-Json }
function Require-TrustedContext {
  if ($env:GITHUB_REPOSITORY -cne 'WrongStack/wrongindex' -or $env:GITHUB_EVENT_NAME -cne 'workflow_dispatch' -or $env:GITHUB_REF -cne 'refs/heads/main') { throw 'Signing is restricted to manual runs of the canonical main branch' }
  if ($env:SIGNPATH_ORGANIZATION_ID -notmatch '^[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}$' -or $env:SIGNPATH_ORGANIZATION_ID -eq [guid]::Empty.ToString() -or $env:SIGNPATH_PROJECT_SLUG -notmatch '^[A-Za-z0-9_-]+$' -or $env:SIGNPATH_POLICY_SLUG -notmatch '^[A-Za-z0-9_-]+$') { throw 'SignPath public account/project/policy configuration is missing' }
  if ($env:SIGNPATH_CERTIFICATE_SHA256 -notmatch '^[a-fA-F0-9]{64}$' -or $env:SIGNPATH_CERTIFICATE_SHA256 -eq ('0' * 64) -or $env:SIGNPATH_TOKEN_CONFIGURED -cne 'true') { throw 'Approved signing certificate or protected submitter credential is missing' }
}
if ($Mode -eq 'CheckConfig') { Require-TrustedContext; Write-Output 'PASS: signing context and required configuration present'; exit 0 }
if ($Mode -eq 'Prepare') {
  Require-TrustedContext
  if (Test-Path -LiteralPath $work) { throw 'Preserve prior signing artifacts' }
  $manifest = Get-Content 'target/release/bundle/nsis/RELEASE-MANIFEST.json' -Raw | ConvertFrom-Json
  $head = (git rev-parse HEAD).Trim()
  git diff --quiet
  if ($LASTEXITCODE -or $head -cne $env:GITHUB_SHA -or $manifest.source.commit -cne $head -or $manifest.source.dirtyWorktree -or (git status --porcelain --untracked-files=normal)) { throw 'Signing source does not match the clean workflow commit' }
  $version = $manifest.source.version
  & (Join-Path $PSScriptRoot '../check-pe-metadata.ps1') -Directory 'target/release/bundle/shipped' -Version $version
  $unsigned = Join-Path $work 'unsigned'
  $null = New-Item -ItemType Directory -Path $unsigned
  $hashes = @{}
  foreach ($name in $names) {
    $source = Join-Path $repo "target/release/bundle/shipped/$name"
    Copy-Item -LiteralPath $source -Destination (Join-Path $unsigned $name)
    $hashes[$name] = (Get-FileHash -LiteralPath $source).Hash.ToLowerInvariant()
  }
  @{ schema = 'wrongindex.signing-inputs/1'; commit = $head; version = $version; certificateSha256 = $env:SIGNPATH_CERTIFICATE_SHA256.ToLowerInvariant(); unsigned = $hashes } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $receiptFile
  Output-Value 'version' $version
  Output-Value 'payloads' $unsigned
  exit 0
}
if ($Mode -eq 'AcceptPayloads') {
  $inputs = Inputs
  $originals = @{}
  foreach ($name in $names) { $originals[$name] = Join-Path $work "unsigned/$name" }
  $decoded = Join-Path $work 'decoded-payloads'
  Expand-SigningArchive -ArchiveDirectory $SignedDirectory -Destination $decoded -UnsignedFiles $originals
  $SignedDirectory = $decoded
  $files = @(Get-ChildItem -LiteralPath $SignedDirectory -File)
  if ($files.Count -ne 3 -or @($files | Where-Object { $_.Name -cnotin $names }).Count) { throw 'Expected exactly the three signed payload files' }
  $verified = @{}
  foreach ($name in $names) {
    $original = Join-Path $work "unsigned/$name"
    if ((Get-FileHash -LiteralPath $original).Hash.ToLowerInvariant() -cne $inputs.unsigned.$name) { throw 'Unsigned payload changed after submission' }
    $signed = Join-Path $SignedDirectory $name
    $identity = Get-ApprovedSigningIdentity $signed $inputs.certificateSha256
    $null = Test-SigningContent $original $signed
    $verified[$name] = $identity.sha256
  }
  & (Join-Path $PSScriptRoot '../check-pe-metadata.ps1') -Directory $SignedDirectory -Version $inputs.version
  $triple = ((rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value)
  foreach ($name in $names) {
    $signed = Join-Path $SignedDirectory $name
    Copy-Item -LiteralPath $signed -Destination (Join-Path $repo "target/release/$name")
    # The desktop is the bundle's main binary (target/release); the service
    # and the CLI are its sidecars, named <name>-<triple>.exe.
    if ($name -ne 'wrongindex-gpui.exe') { Copy-Item -LiteralPath $signed -Destination (Join-Path $repo ('crates/wrongindex-gpui/binaries/' + $name.Replace('.exe', "-$triple.exe"))) }
    if ((Get-FileHash -LiteralPath (Join-Path $repo "target/release/$name")).Hash.ToLowerInvariant() -cne $verified[$name]) { throw 'Signed payload changed while staged' }
  }
  Initialize-SigningCourier -Directory $courier -PayloadDirectory $SignedDirectory -BuildRoot (Join-Path $repo 'target') -Version $inputs.version -CertificateSha256 $inputs.certificateSha256
  exit 0
}
if ($Mode -eq 'StartBundle') {
  $null = Inputs
  $config = Join-Path $work 'tauri-signing.json'
  @{ bundle = @{ windows = @{ signCommand = @{ cmd = (Get-Command pwsh).Source; args = @('-NoProfile','-File',(Join-Path $PSScriptRoot 'hook.ps1'),'-Directory',$courier,'-File','%1') } } } } | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath $config
  $worker = Join-Path $PSScriptRoot 'bundle-worker.ps1'
  $arguments = @('-NoProfile','-File',('"'+$worker+'"'),'-Repo',('"'+$repo+'"'),'-Config',('"'+$config+'"'),'-Courier',('"'+$courier+'"'))
  $process = Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $courier 'worker.stdout') -RedirectStandardError (Join-Path $courier 'worker.stderr')
  try {
    $null = $process.Handle
    @{ pid = $process.Id; startTicks = $process.StartTime.ToUniversalTime().Ticks } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $courier 'worker.json')
  } catch {
    Stop-SigningCourier $courier
    if (-not $process.HasExited) { $process.Kill($true); $null = $process.WaitForExit(5000) }
    throw
  } finally { $process.Dispose() }
  exit 0
}
if ($Mode -in @('WaitUninstaller','WaitInstaller')) {
  $phase = if ($Mode -eq 'WaitUninstaller') { 'uninstaller' } else { 'installer' }
  $file = Wait-SigningRequest -Directory $courier -Phase $phase
  Output-Value 'file' $file
  exit 0
}
if ($Mode -in @('CompleteUninstaller','CompleteInstaller')) {
  $inputs = Inputs
  $phase = if ($Mode -eq 'CompleteUninstaller') { 'uninstaller' } else { 'installer' }
  $name = if ($phase -eq 'uninstaller') { 'uninstall.exe' } else { "WrongIndex_$($inputs.version)_x64-setup.exe" }
  $pending = Read-SigningRequest -Directory $courier -Phase $phase
  if (-not $pending) { throw 'Signing request is not pending' }
  $decoded = Join-Path $work "decoded-$phase"
  Expand-SigningArchive -ArchiveDirectory $SignedDirectory -Destination $decoded -UnsignedFiles @{ $name = $pending.unsigned }
  $SignedDirectory = $decoded
  $files = @(Get-ChildItem -LiteralPath $SignedDirectory -File)
  if ($files.Count -ne 1 -or $files[0].Name -cne $name) { throw 'Signing response has an unexpected file set' }
  Complete-SigningRequest -Directory $courier -Phase $phase -SignedFile $files[0].FullName
  exit 0
}
if ($Mode -eq 'FinishBundle') {
  $timer = [Diagnostics.Stopwatch]::StartNew()
  while (-not (Test-Path -LiteralPath (Join-Path $courier 'worker-result.ready'))) {
    if ($timer.Elapsed.TotalSeconds -gt 900) { throw 'Signed bundler completion deadline exceeded' }
    if (Test-Path -LiteralPath (Join-Path $courier 'abort.json')) { throw 'Signed bundler failed' }
    Start-Sleep -Milliseconds 200
  }
  $state = Get-Content (Join-Path $courier 'state.json') -Raw | ConvertFrom-Json
  $result = Get-Content (Join-Path $courier 'worker-result.json') -Raw | ConvertFrom-Json
  if ($result.status -cne 'PASS' -or $result.nonce -cne $state.nonce) { throw 'Signed bundler did not complete for this signing request' }
  foreach ($name in $names) {
    $identity = Get-ApprovedSigningIdentity (Join-Path $repo "target/release/$name") $state.certificateSha256
    if ($identity.sha256 -cne $state.allowed.$name) { throw 'Bundling changed a signed payload' }
  }
  foreach ($phase in @('uninstaller','installer')) {
    $done = Get-Content (Join-Path $courier "$phase/completed.json") -Raw | ConvertFrom-Json
    if ($done.nonce -cne $state.nonce -or $done.certificateSha256 -cne $state.certificateSha256) { throw 'A signing phase is missing or belongs to another request' }
    $file = if ($phase -eq 'installer') { Join-Path $repo "target/release/bundle/nsis/WrongIndex_$($state.version)_x64-setup.exe" } else { Join-Path $courier 'uninstaller/response.exe' }
    $identity = Get-ApprovedSigningIdentity $file $state.certificateSha256
    if ($identity.sha256 -cne $done.sha256) { throw 'A signed final artifact changed after delivery' }
  }
  Write-Output 'PASS: signed payloads, uninstaller and installer phases completed'
  exit 0
}
if ($Mode -eq 'Abort' -and (Test-Path -LiteralPath $courier)) {
  $finished = Get-Content (Join-Path $courier 'worker-result.json') -Raw -ErrorAction SilentlyContinue | ConvertFrom-Json
  if ($finished -and $finished.status -ceq 'PASS') { exit 0 }
  Stop-SigningCourier $courier
  $record = Get-Content (Join-Path $courier 'worker.json') -Raw -ErrorAction SilentlyContinue | ConvertFrom-Json
  if ($record) {
    $process = Get-Process -Id $record.pid -ErrorAction SilentlyContinue
    if ($process -and $process.StartTime.ToUniversalTime().Ticks -eq $record.startTicks) { $process.Kill($true); $null = $process.WaitForExit(5000) }
  }
}
