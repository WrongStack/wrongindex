param([string]$ModuleSource = (Join-Path $PSScriptRoot 'Signing.psm1'))
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ('.tmp-wi-signing-lease-' + [guid]::NewGuid().ToString('N'))
$modules = Join-Path $root 'signpath'; $pending = Join-Path $root 'courier/uninstaller'
$null = New-Item -ItemType Directory -Path $modules,$pending
Copy-Item (Join-Path $PSScriptRoot '../pe-signature-content.mjs') (Join-Path $root 'pe-signature-content.mjs')
$code = Get-Content -LiteralPath $ModuleSource -Raw
$code += @'

# Private adversarial test double: signature trust is assumed only for the
# foreign image. Swap in matching bytes during the content check, then restore
# the foreign image before staging. A read lease must reject the first write.
function Get-ApprovedSigningIdentity([string]$File,[string]$CertificateSha256) {
  return @{sha256=(Get-FileHash -LiteralPath $File).Hash.ToLowerInvariant();certificateSha256=$CertificateSha256}
}
$script:contentCheck = (Get-Command Test-SigningContent).ScriptBlock
function Test-SigningContent([string]$Unsigned,[string]$Signed) {
  $original = [IO.File]::ReadAllBytes($Signed)
  try { [IO.File]::WriteAllBytes($Signed,[IO.File]::ReadAllBytes((Join-Path $PSScriptRoot '../matching.exe'))) }
  catch [IO.IOException] { 'blocked' | Set-Content (Join-Path $PSScriptRoot '../replacement-blocked'); throw }
  try { return & $script:contentCheck $Unsigned $Signed }
  finally { [IO.File]::WriteAllBytes($Signed,$original) }
}
'@
$moduleFile = Join-Path $modules 'Signing.psm1'; $code | Set-Content $moduleFile
Import-Module $moduleFile -Force
$unsigned = [byte[]]::new(501)
[BitConverter]::GetBytes([uint16]0x5a4d).CopyTo($unsigned,0)
[BitConverter]::GetBytes([uint32]0x80).CopyTo($unsigned,0x3c)
[BitConverter]::GetBytes([uint32]0x4550).CopyTo($unsigned,0x80)
[BitConverter]::GetBytes([uint16]240).CopyTo($unsigned,0x94)
[BitConverter]::GetBytes([uint16]0x20b).CopyTo($unsigned,0x98)
[BitConverter]::GetBytes([uint32]16).CopyTo($unsigned,0x104)
$unsigned[420]=71
$matching = [byte[]]::new(520); $unsigned.CopyTo($matching,0)
[BitConverter]::GetBytes([uint32]504).CopyTo($matching,0x128)
[BitConverter]::GetBytes([uint32]16).CopyTo($matching,0x12c)
$foreign = [byte[]]$matching.Clone(); $foreign[420]=72
[IO.File]::WriteAllBytes((Join-Path $root 'matching.exe'),$matching)
$candidate = Join-Path $root 'foreign.exe'; [IO.File]::WriteAllBytes($candidate,$foreign)
$inputFile = Join-Path $pending 'uninstall.exe'; [IO.File]::WriteAllBytes($inputFile,$unsigned)
$nonce=[guid]::NewGuid().ToString('N'); $courier=Join-Path $root 'courier'
@{nonce=$nonce;certificateSha256=('0'*64)} | ConvertTo-Json | Set-Content (Join-Path $courier 'state.json')
@{nonce=$nonce;phase='uninstaller';filename='uninstall.exe';unsignedSha256=(Get-FileHash $inputFile).Hash.ToLowerInvariant()} | ConvertTo-Json | Set-Content (Join-Path $pending 'request.json')
'ready' | Set-Content (Join-Path $pending 'request.ready')
$refused=$false
try { Complete-SigningRequest -Directory $courier -Phase uninstaller -SignedFile $candidate } catch { $refused=$true }
if (-not $refused -or -not (Test-Path (Join-Path $root 'replacement-blocked')) -or (Test-Path (Join-Path $pending 'response.ready'))) { throw 'A signing response replacement was accepted or not blocked by the read lease' }
if (-not [Linq.Enumerable]::SequenceEqual([byte[]][IO.File]::ReadAllBytes($candidate),[byte[]]$foreign)) { throw 'The refused replacement changed the candidate' }
@{status='PASS';replacement='REFUSED before content validation/staging';boundary='Adversarial private-module trust double; synthetic images never executed or installed';evidence=$root} | ConvertTo-Json
