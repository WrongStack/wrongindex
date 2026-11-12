$ErrorActionPreference = 'Stop'
$modulePath = Join-Path $PSScriptRoot 'Signing.psm1'
Import-Module $modulePath -Force
$realModule = Get-Module | Where-Object Path -eq (Resolve-Path -LiteralPath $modulePath).Path
$root = Join-Path ([IO.Path]::GetTempPath()) ('.tmp-wi-signing-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $root
$testModuleDir = Join-Path $root 'scripts/signpath'
$null = New-Item -ItemType Directory -Path $testModuleDir
$payload = Join-Path $root 'payload'
$null = New-Item -ItemType Directory -Path $payload
$cert = '0' * 64

function Assert-Fails([scriptblock]$Action, [string]$Pattern) {
  try { & $Action; throw 'Expected a refusal' } catch { if ($_.Exception.Message -notmatch $Pattern) { throw } }
}
# Real trust boundary control, outside all mocks. No private key or trust change.
$git = Join-Path $env:ProgramFiles 'Git/cmd/git.exe'
$trustControl = 'NOT RUN: Git signed control is absent'
if (Test-Path -LiteralPath $git) {
  $sig = Get-AuthenticodeSignature -LiteralPath $git
  if ($sig.Status.ToString() -eq 'Valid') {
    $hash = [Security.Cryptography.SHA256]::Create()
    try { $digest = [Convert]::ToHexString($hash.ComputeHash($sig.SignerCertificate.RawData)).ToLowerInvariant() } finally { $hash.Dispose() }
    $null = Get-ApprovedSigningIdentity $git $digest
    Assert-Fails { Get-ApprovedSigningIdentity $git $cert } 'unapproved certificate'
    $trustControl = 'PASS: trusted embedded signature and wrong-certificate refusal'
  }
}

# Only external OS trust/resource lookups are mocked in this private module copy.
# The production module has no testing flag or environment bypass.
$code = Get-Content -LiteralPath $modulePath -Raw
$code += @'

function Get-ApprovedSigningIdentity([string]$File, [string]$CertificateSha256) {
  return @{ certificateSha256 = $CertificateSha256; sha256 = (Get-FileHash -LiteralPath $File).Hash.ToLowerInvariant() }
}
function Read-ProductIdentity([string]$File) {
  return @{ProductName='WrongIndex';ProductVersion='0.2.0';FileVersion='0.2.0'}
}
function Start-Sleep([int]$Milliseconds) {
  $definition = Join-Path $PSScriptRoot 'completion-gate.json'
  if ((Test-Path -LiteralPath $definition) -and @(Get-PSCallStack | Where-Object FunctionName -eq 'Invoke-SigningHook').Count) {
    $names = Get-Content $definition -Raw | ConvertFrom-Json
    $entered = [Threading.EventWaitHandle]::OpenExisting($names.entered)
    $release = [Threading.EventWaitHandle]::OpenExisting($names.release)
    try { $null = $entered.Set(); if (-not $release.WaitOne(20000)) { throw 'Test completion barrier timed out' } } finally { $entered.Dispose(); $release.Dispose() }
  } else { Microsoft.PowerShell.Utility\Start-Sleep -Milliseconds $Milliseconds }
}
Export-ModuleMember -Function Get-ApprovedSigningIdentity, Test-SigningContent, Initialize-SigningCourier, Read-SigningRequest, Wait-SigningRequest, Complete-SigningRequest, Invoke-SigningHook
'@
$code | Set-Content -LiteralPath (Join-Path $testModuleDir 'Signing.psm1') -Encoding utf8
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'hook.ps1') -Destination $testModuleDir
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../pe-signature-content.mjs') -Destination (Join-Path $root 'scripts/pe-signature-content.mjs')
$vendorDirectory = Join-Path $root '.signpath'
$null = New-Item -ItemType Directory -Path $vendorDirectory
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../../../.signpath/nsis-vendor.json') -Destination $vendorDirectory
# A private synthetic vendor fixture exercises hash/path admission. The real
# vendor baseline is separately reproduced from the two pinned release assets.
$pluginDirectory = Join-Path $root 'release/nsis/x64/Plugins/x86-unicode'
$null = New-Item -ItemType Directory -Path $pluginDirectory
$plugin = Join-Path $pluginDirectory 'NSISdl.dll'
[IO.File]::WriteAllBytes($plugin, [byte[]](1,2,3,4))
$vendorFile = Join-Path $vendorDirectory 'nsis-vendor.json'
$vendor = Get-Content $vendorFile -Raw | ConvertFrom-Json
$vendor.files.'NSISdl.dll' = (Get-FileHash $plugin).Hash.ToLowerInvariant()
$vendor | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $vendorFile
Import-Module (Join-Path $testModuleDir 'Signing.psm1') -Force
$unsigned = [byte[]]::new(501)
[BitConverter]::GetBytes([uint16]0x5a4d).CopyTo($unsigned,0)
[BitConverter]::GetBytes([uint32]0x80).CopyTo($unsigned,0x3c)
[BitConverter]::GetBytes([uint32]0x4550).CopyTo($unsigned,0x80)
[BitConverter]::GetBytes([uint16]240).CopyTo($unsigned,0x94)
[BitConverter]::GetBytes([uint16]0x20b).CopyTo($unsigned,0x98)
[BitConverter]::GetBytes([uint32]16).CopyTo($unsigned,0x104)
$unsigned[420] = 71
$signed = [byte[]]::new(520); $unsigned.CopyTo($signed,0)
[BitConverter]::GetBytes([uint32]504).CopyTo($signed,0x128)
[BitConverter]::GetBytes([uint32]16).CopyTo($signed,0x12c)
$fakeSignature = Join-Path $root 'fake-signature.exe'; [IO.File]::WriteAllBytes($fakeSignature,$signed)
Assert-Fails { & $realModule { param($file,$digest) Get-ApprovedSigningIdentity $file $digest } $fakeSignature $cert } 'no valid Windows Authenticode'
foreach ($name in @('wrongindex.exe','wrongindexd.exe','wrongindex-gpui.exe','wrongindex-desktop.exe')) { [IO.File]::WriteAllBytes((Join-Path $payload $name),$signed) }
$courier = Join-Path $root 'courier'
Initialize-SigningCourier -Directory $courier -PayloadDirectory $payload -BuildRoot $root -Version 0.2.0 -CertificateSha256 $cert -VendorManifest $vendorFile
Invoke-SigningHook -Directory $courier -File (Join-Path $payload 'wrongindex.exe')
$null = Invoke-SigningHook -Directory $courier -File $plugin
$movedPlugin = Join-Path $root 'NSISdl.dll'; Copy-Item $plugin $movedPlugin
Assert-Fails { Invoke-SigningHook -Directory $courier -File $movedPlugin } 'Unrecognized or modified upstream'
[IO.File]::WriteAllBytes($plugin, [byte[]](1,2,3,5))
Assert-Fails { Invoke-SigningHook -Directory $courier -File $plugin } 'Unrecognized or modified upstream'
Assert-Fails { Wait-SigningRequest -Directory $courier -Phase installer -TimeoutSeconds 0 } 'deadline'
$earlyInstaller = Join-Path $root 'WrongIndex_0.2.0_x64-setup.exe'; [IO.File]::WriteAllBytes($earlyInstaller,$unsigned)
Assert-Fails { Invoke-SigningHook -Directory $courier -File $earlyInstaller } 'before the signed uninstaller'
Assert-Fails { Invoke-SigningHook -Directory $courier -File (Join-Path $env:SystemRoot 'System32/cmd.exe') } 'outside the build'

$source = Join-Path $root 'compiler-output.tmp'; [IO.File]::WriteAllBytes($source,$unsigned)
$child = Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList @('-NoProfile','-File',(Join-Path $testModuleDir 'hook.ps1'),'-Directory',$courier,'-File',$source) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $root 'hook.stdout') -RedirectStandardError (Join-Path $root 'hook.stderr')
$null = $child.Handle
try {
  $pending = Wait-SigningRequest -Directory $courier -Phase uninstaller -TimeoutSeconds 20
  $requestFile = Join-Path $courier 'uninstaller/request.json'
  $originalRequest = Get-Content -LiteralPath $requestFile -Raw
  $wrongRequest = $originalRequest | ConvertFrom-Json; $wrongRequest.nonce = 'stale'
  $wrongRequest | ConvertTo-Json | Set-Content -LiteralPath $requestFile
  Assert-Fails { Read-SigningRequest -Directory $courier -Phase uninstaller } 'identity mismatch'
  $originalRequest | Set-Content -LiteralPath $requestFile
  $candidate = Join-Path $root 'signed.exe'; [IO.File]::WriteAllBytes($candidate,$signed)
  $changed = [byte[]]$signed.Clone(); $changed[420] = 72
  $bad = Join-Path $root 'changed.exe'; [IO.File]::WriteAllBytes($bad,$changed)
  Assert-Fails { Complete-SigningRequest -Directory $courier -Phase uninstaller -SignedFile $bad } 'changed the requested executable content'
  if ((Get-FileHash -LiteralPath $pending).Hash -cne (Get-FileHash -LiteralPath (Join-Path $courier 'uninstaller/uninstall.exe')).Hash) { throw 'Unsigned snapshot changed' }
  Complete-SigningRequest -Directory $courier -Phase uninstaller -SignedFile $candidate
  if (-not $child.WaitForExit(20000) -or $child.ExitCode -ne 0) { throw 'Gated hook completion failed' }
  if ((Get-FileHash -LiteralPath $source).Hash -cne (Get-FileHash -LiteralPath $candidate).Hash) { throw 'Delivered source differs' }
  Assert-Fails { Complete-SigningRequest -Directory $courier -Phase uninstaller -SignedFile $candidate } 'already exists'
} finally { if (-not $child.HasExited) { $child.Kill($true); $null = $child.WaitForExit(5000) }; $child.Dispose() }
$cancelCourier = Join-Path $root 'cancel-courier'
Initialize-SigningCourier -Directory $cancelCourier -PayloadDirectory $payload -BuildRoot $root -Version 0.2.0 -CertificateSha256 $cert -VendorManifest $vendorFile
$cancelSource = Join-Path $root 'cancel-source.tmp'; [IO.File]::WriteAllBytes($cancelSource,$unsigned)
$gateId = [guid]::NewGuid().ToString('N')
$enteredName = 'Local\WrongIndexEntered-' + $gateId; $releaseName = 'Local\WrongIndexRelease-' + $gateId
$entered = [Threading.EventWaitHandle]::new($false,[Threading.EventResetMode]::ManualReset,$enteredName)
$release = [Threading.EventWaitHandle]::new($false,[Threading.EventResetMode]::ManualReset,$releaseName)
@{entered=$enteredName;release=$releaseName}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $testModuleDir 'completion-gate.json')
$child = Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList @('-NoProfile','-File',(Join-Path $testModuleDir 'hook.ps1'),'-Directory',$cancelCourier,'-File',$cancelSource) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $root 'cancel.stdout') -RedirectStandardError (Join-Path $root 'cancel.stderr')
$null = $child.Handle
try {
  $null = Wait-SigningRequest -Directory $cancelCourier -Phase uninstaller -TimeoutSeconds 20
  if (-not $entered.WaitOne(20000)) { throw 'Hook did not reach the completion barrier' }
  Complete-SigningRequest -Directory $cancelCourier -Phase uninstaller -SignedFile $candidate
  @{reason='Cancellation before gated response completion'}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $cancelCourier 'abort.json')
  $null = $release.Set()
  if (-not $child.WaitForExit(20000)) { throw 'Cancelled hook did not finish' }
  if ($child.ExitCode -eq 0 -or -not [Linq.Enumerable]::SequenceEqual([byte[]][IO.File]::ReadAllBytes($cancelSource),[byte[]]$unsigned)) { throw 'Cancelled signing response was applied' }
} finally {
  $null = $release.Set()
  if (-not $child.HasExited) { $child.Kill($true); $null = $child.WaitForExit(5000) }
  $child.Dispose(); $entered.Dispose(); $release.Dispose()
}
@{status='PASS';trustControl=$trustControl;courier='PASS: known payload, outside-root/early installer refusals, deadline, stale identity, altered code refusal, gated response, duplicate refusal';fakeSignature='REFUSED by real verifier';boundary='OS trust/resource mocks only for courier state machine; no genuine signing request, no trust changes';evidence=$root}|ConvertTo-Json -Depth 4
