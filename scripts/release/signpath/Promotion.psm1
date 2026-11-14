$ErrorActionPreference='Stop'
function Test-SigningWorkflowRun($Run) {
  if($Run.status -cne 'completed' -or $Run.conclusion -cne 'success' -or $Run.event -cne 'workflow_dispatch' -or $Run.path -cne '.github/workflows/signpath-preview.yml' -or $Run.repository.full_name -cne 'WrongStack/wrongindex' -or $Run.head_repository.full_name -cne 'WrongStack/wrongindex' -or $Run.head_branch -cne 'main' -or $Run.head_sha -notmatch '^[a-f0-9]{40}$') {throw 'Run is not a successful canonical-main signing preview'}
  return $Run.head_sha
}
function Test-PromotionFiles {
  param([string]$Directory,[string]$SourceCommit,[string]$CertificateSha256)
  Import-Module (Join-Path $PSScriptRoot 'Signing.psm1') -Force
  $out=Join-Path $Directory 'nsis'
  $manifest=Get-Content (Join-Path $out 'RELEASE-MANIFEST.json') -Raw|ConvertFrom-Json
  $preview=Get-Content (Join-Path $out 'SIGNED-PREVIEW.json') -Raw|ConvertFrom-Json
  if($manifest.source.commit -cne $SourceCommit -or $manifest.source.dirtyWorktree -or $preview.sourceCommit -cne $SourceCommit -or $preview.status -cne 'PASS' -or $preview.installedCore -cne 'PASS' -or $manifest.codeSigning.status -cne 'Valid' -or $manifest.codeSigning.certificateSha256 -cne $CertificateSha256.ToLowerInvariant() -or $preview.certificateSha256 -cne $CertificateSha256.ToLowerInvariant()) {throw 'Signed qualification artifact identity mismatch'}
  $version=$manifest.source.version
  if($version -notmatch '^\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.+-]+)?$' -or $preview.version -cne $version){throw 'Invalid signed artifact version'}
  $installerName="WrongIndex_${version}_x64-setup.exe"
  $installer=Join-Path $out $installerName
  $identity=Get-ApprovedSigningIdentity $installer $CertificateSha256
  if($identity.sha256 -cne $preview.installerSha256){throw 'Signed installer hash mismatch'}
  $checksum=[IO.File]::ReadAllText((Join-Path $out 'SHA256SUMS.txt')).TrimEnd("`r","`n")
  if($checksum -cne ($identity.sha256+'  '+$installerName)){throw 'Signed installer checksum file mismatch'}
  $subjects=@($installer)
  foreach($name in @('wrongindex.exe','wrongindexd.exe','wrongindex-gpui.exe')){
    $file=Join-Path $Directory "shipped/$name"
    $signed=Get-ApprovedSigningIdentity $file $CertificateSha256
    if($signed.sha256 -cne $manifest.binaries.$name.sha256 -or $manifest.binaries.$name.authenticode -cne 'Valid'){throw 'Signed program manifest mismatch'}
    $subjects+=$file
  }
  $uninstaller=Join-Path $Directory 'shipped/uninstall.exe'
  $signed=Get-ApprovedSigningIdentity $uninstaller $CertificateSha256
  if($signed.sha256 -cne $manifest.uninstaller.sha256 -or $manifest.uninstaller.authenticode -cne 'Valid'){throw 'Signed uninstaller manifest mismatch'}
  $subjects+=$uninstaller
  return @{version=$version;tag="v$version";installer=$installer;out=$out;subjects=$subjects;installerSha256=$identity.sha256}
}
Export-ModuleMember -Function Test-SigningWorkflowRun,Test-PromotionFiles
