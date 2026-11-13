param([Parameter(Mandatory)][string]$Repo, [Parameter(Mandatory)][string]$Config, [Parameter(Mandatory)][string]$Courier)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'Signing.psm1') -Force
$state = Get-Content (Join-Path $Courier 'state.json') -Raw | ConvertFrom-Json
$result = @{ nonce = $state.nonce; status = 'RUNNING'; startedAt = [DateTimeOffset]::UtcNow.ToString('o') }
try {
  Set-Location (Join-Path $Repo 'crates/wrongindex-desktop')
  cargo tauri bundle --config tauri.bundle.json --config $Config --bundles nsis --no-binary-patching *> (Join-Path $Courier 'bundle.log')
  $result.exitCode = $LASTEXITCODE
  if ($LASTEXITCODE) { throw 'Signed NSIS bundling failed' }
  $result.status = 'PASS'
} catch {
  $result.status = 'FAIL'; $result.reason = $_.Exception.Message
  Stop-SigningCourier $Courier
} finally {
  $result.finishedAt = [DateTimeOffset]::UtcNow.ToString('o')
  $result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Courier 'worker-result.json')
  'ready' | Set-Content -LiteralPath (Join-Path $Courier 'worker-result.ready')
}
if ($result.status -ne 'PASS') { exit 1 }
