$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'Promotion.psm1') -Force
function Assert-Fails([scriptblock]$Action, [string]$Pattern) {
  try { & $Action; throw 'Expected a refusal' } catch { if ($_.Exception.Message -notmatch $Pattern) { throw } }
}
$sha = 'a' * 40
$run = @{status='completed';conclusion='success';event='workflow_dispatch';path='.github/workflows/signpath-preview.yml';repository=@{full_name='WrongStack/wrongindex'};head_repository=@{full_name='WrongStack/wrongindex'};head_branch='main';head_sha=$sha}
if ((Test-SigningWorkflowRun $run) -cne $sha) { throw 'Canonical workflow control failed' }
foreach ($change in @(@{status='in_progress'},@{conclusion='failure'},@{event='pull_request'},@{path='.github/workflows/release.yml'},@{head_branch='other'},@{head_sha='bad'},@{repository=@{full_name='other/repo'}},@{head_repository=@{full_name='other/repo'}})) {
  $bad = $run.Clone()
  foreach ($key in $change.Keys) { $bad[$key] = $change[$key] }
  Assert-Fails { Test-SigningWorkflowRun $bad } 'successful canonical-main'
}
# Signature verification is mocked only in an isolated module directory. This
# proves promotion's manifest/hash bindings, not genuine signing acceptance.
$root = Join-Path ([IO.Path]::GetTempPath()) ('.tmp-wi-promotion-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $root,(Join-Path $root 'nsis'),(Join-Path $root 'shipped')
Copy-Item (Join-Path $PSScriptRoot 'Promotion.psm1') $root
@'
function Get-ApprovedSigningIdentity([string]$File,[string]$CertificateSha256) { return @{sha256=(Get-FileHash -LiteralPath $File).Hash.ToLowerInvariant();certificateSha256=$CertificateSha256} }
Export-ModuleMember -Function Get-ApprovedSigningIdentity
'@ | Set-Content (Join-Path $root 'Signing.psm1')
Import-Module (Join-Path $root 'Promotion.psm1') -Force
$cert = 'b' * 64
$installerName = 'WrongIndex_0.2.0_x64-setup.exe'
$installer = Join-Path $root "nsis/$installerName"
[IO.File]::WriteAllBytes($installer,[byte[]](1,2,3))
$installerHash = (Get-FileHash $installer).Hash.ToLowerInvariant()
$manifest = @{source=@{commit=$sha;dirtyWorktree=$false;version='0.2.0'};codeSigning=@{status='Valid';certificateSha256=$cert};binaries=@{}}
foreach ($name in @('wrongindex.exe','wrongindexd.exe','wrongindex-gpui.exe','wrongindex-desktop.exe','uninstall.exe')) {
  $file = Join-Path $root "shipped/$name"; [IO.File]::WriteAllBytes($file,[byte[]](4,5,6))
  $entry = @{sha256=(Get-FileHash $file).Hash.ToLowerInvariant();authenticode='Valid'}
  if ($name -ceq 'uninstall.exe') { $manifest.uninstaller=$entry } else { $manifest.binaries[$name]=$entry }
}
$preview = @{sourceCommit=$sha;status='PASS';installedCore='PASS';certificateSha256=$cert;version='0.2.0';installerSha256=$installerHash}
$manifestFile = Join-Path $root 'nsis/RELEASE-MANIFEST.json'
$previewFile = Join-Path $root 'nsis/SIGNED-PREVIEW.json'
$manifest | ConvertTo-Json -Depth 8 | Set-Content $manifestFile
$preview | ConvertTo-Json | Set-Content $previewFile
[IO.File]::WriteAllText((Join-Path $root 'nsis/SHA256SUMS.txt'),$installerHash+'  '+$installerName+"`n")
$result = Test-PromotionFiles $root $sha $cert
if ($result.subjects.Count -ne 6 -or $result.tag -cne 'v0.2.0') { throw 'Unexpected final subject inventory' }
Assert-Fails { Test-PromotionFiles $root ('c'*40) $cert } 'identity mismatch'
Assert-Fails { Test-PromotionFiles $root $sha ('c'*64) } 'identity mismatch'
$manifest.source.dirtyWorktree=$true; $manifest | ConvertTo-Json -Depth 8 | Set-Content $manifestFile
Assert-Fails { Test-PromotionFiles $root $sha $cert } 'identity mismatch'
$manifest.source.dirtyWorktree=$false; $manifest | ConvertTo-Json -Depth 8 | Set-Content $manifestFile
$preview.installedCore='FAIL'; $preview | ConvertTo-Json | Set-Content $previewFile
Assert-Fails { Test-PromotionFiles $root $sha $cert } 'identity mismatch'
$preview.installedCore='PASS'; $preview | ConvertTo-Json | Set-Content $previewFile
[IO.File]::WriteAllBytes((Join-Path $root 'shipped/uninstall.exe'),[byte[]](4,5,7))
Assert-Fails { Test-PromotionFiles $root $sha $cert } 'uninstaller manifest mismatch'
[IO.File]::WriteAllBytes((Join-Path $root 'shipped/uninstall.exe'),[byte[]](4,5,6))
[IO.File]::WriteAllBytes($installer,[byte[]](1,2,4))
Assert-Fails { Test-PromotionFiles $root $sha $cert } 'installer hash mismatch'
@{status='PASS';workflowRefusals=8;artifactRefusals=6;subjects=6;boundary='Private signature mock for manifest/hash binding only; no provider signing, upload or publication';evidence=$root} | ConvertTo-Json
