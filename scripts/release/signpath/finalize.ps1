param([Parameter(Mandatory)][string]$GuiActorTest)
$ErrorActionPreference='Stop'
$repo=(Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
Set-Location $repo
Import-Module (Join-Path $PSScriptRoot 'Signing.psm1') -Force
$work=Join-Path $repo 'target/signpath-ci'
$inputs=Get-Content (Join-Path $work 'inputs.json') -Raw|ConvertFrom-Json
$out=Join-Path $repo 'target/release/bundle/nsis'
$installerName="WrongIndex_$($inputs.version)_x64-setup.exe"
$installer=Join-Path $out $installerName
$identity=Get-ApprovedSigningIdentity $installer $inputs.certificateSha256
$done=Get-Content (Join-Path $work 'courier/installer/completed.json') -Raw|ConvertFrom-Json
if($identity.sha256 -cne $done.sha256){throw 'Final installer changed after signing'}
$manifestFile=Join-Path $out 'RELEASE-MANIFEST.json'
$shipped=Join-Path $repo 'target/release/bundle/shipped'
node scripts/release/manifest.mjs manifest --bin target/release --bundle $out --installer $installerName --out $manifestFile --shipped-out $shipped
if($LASTEXITCODE){throw 'Signed release manifest generation failed'}
& scripts/release/check-pe-metadata.ps1 -Directory $shipped -Version $inputs.version
$manifest=Get-Content $manifestFile -Raw|ConvertFrom-Json
foreach($name in @('wrongindex.exe','wrongindexd.exe','wrongindex-gpui.exe')){
  $null=Get-ApprovedSigningIdentity (Join-Path $shipped $name) $inputs.certificateSha256
  if($manifest.binaries.$name.authenticode -cne 'Valid'){throw 'Extracted signature is not valid'}
}
$uninstaller=Join-Path $work 'courier/uninstaller/response.exe'
$uninstallIdentity=Get-ApprovedSigningIdentity $uninstaller $inputs.certificateSha256
& scripts/release/smoke-installed-core.ps1 -Installer $installer -Manifest $manifestFile -GuiActorTest $GuiActorTest -Result (Join-Path $work 'installed-core.json') -CertificateSha256 $inputs.certificateSha256 -SignedUninstaller $uninstaller -KeepUninstaller (Join-Path $shipped 'uninstall.exe')
if($LASTEXITCODE){throw 'Signed installed core/uninstaller acceptance failed'}
$core=Get-Content (Join-Path $work 'installed-core.json') -Raw|ConvertFrom-Json
if($core.status -cne 'PASS' -or $core.uninstallerSha256 -cne $uninstallIdentity.sha256){throw 'Signed installed core did not pass'}
$manifest|Add-Member -NotePropertyName codeSigning -NotePropertyValue @{provider='SignPath Foundation';certificateSha256=$inputs.certificateSha256;status='Valid'} -Force
$manifest|Add-Member -NotePropertyName uninstaller -NotePropertyValue @{sha256=$uninstallIdentity.sha256;authenticode='Valid';path='shipped/uninstall.exe'} -Force
$manifest|ConvertTo-Json -Depth 30|Set-Content -LiteralPath $manifestFile -Encoding utf8
[IO.File]::WriteAllText((Join-Path $out 'SHA256SUMS.txt'),($identity.sha256+'  '+$installerName+"`n"))
@{schema='wrongindex.signed-preview/1';status='PASS';sourceCommit=$inputs.commit;version=$inputs.version;installerSha256=$identity.sha256;certificateSha256=$inputs.certificateSha256;installedCore='PASS';releaseReady=$false;boundary='Signed preview only; clean-Windows, GUI, final 24-hour soak and owner release qualification remain separate'}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $out 'SIGNED-PREVIEW.json')
