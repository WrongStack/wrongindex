param([Parameter(Mandatory)][ValidateSet('Inspect','Verify','Publish')][string]$Mode)
$ErrorActionPreference='Stop'
$repo=(Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
Set-Location $repo
Import-Module (Join-Path $PSScriptRoot 'Promotion.psm1') -Force
if($env:GITHUB_REPOSITORY -cne 'WrongStack/wrongindex' -or $env:GITHUB_REF -cne 'refs/heads/main' -or $env:GITHUB_EVENT_NAME -cne 'workflow_dispatch'){throw 'Promotion requires the canonical manual main workflow'}
if($env:SOURCE_SIGNING_RUN_ID -notmatch '^[1-9][0-9]+$' -or $env:SOURCE_SIGNING_RUN_ID -cne $env:QUALIFIED_SIGNING_RUN_ID){throw 'The exact signing run has not been marked qualified'}
if($env:SIGNPATH_CERTIFICATE_SHA256 -notmatch '^[a-fA-F0-9]{64}$' -or $env:SIGNPATH_CERTIFICATE_SHA256 -eq ('0'*64)){throw 'Approved public signing certificate is missing'}
$work=Join-Path $repo 'target/promoted'
$sourceFile=Join-Path $repo 'target/promotion-source-run.json'
if($Mode -eq 'Inspect'){
  $run=gh api "repos/WrongStack/wrongindex/actions/runs/$($env:SOURCE_SIGNING_RUN_ID)"|ConvertFrom-Json
  if($LASTEXITCODE){throw 'Cannot read the qualified workflow run'}
  $sha=Test-SigningWorkflowRun $run
  if([string]$run.id -cne $env:SOURCE_SIGNING_RUN_ID){throw 'Workflow run ID mismatch'}
  $run|ConvertTo-Json -Depth 30|Set-Content -LiteralPath $sourceFile
  Write-Output "Qualified source commit: $sha"
  exit 0
}
$run=Get-Content $sourceFile -Raw|ConvertFrom-Json
$sha=Test-SigningWorkflowRun $run
$files=Test-PromotionFiles -Directory $work -SourceCommit $sha -CertificateSha256 $env:SIGNPATH_CERTIFICATE_SHA256
if($Mode -eq 'Verify'){
  foreach($subject in $files.subjects){
    gh attestation verify $subject --repo WrongStack/wrongindex --signer-workflow WrongStack/wrongindex/.github/workflows/signpath-preview.yml --source-digest $sha --deny-self-hosted-runners *> (Join-Path $repo ('target/attestation-'+[IO.Path]::GetFileName($subject)+'.log'))
    if($LASTEXITCODE){throw 'Exact signed-byte/source-workflow attestation failed'}
  }
  @{runId=$env:SOURCE_SIGNING_RUN_ID;sourceCommit=$sha;installerSha256=$files.installerSha256;certificateSha256=$env:SIGNPATH_CERTIFICATE_SHA256.ToLowerInvariant()}|ConvertTo-Json|Set-Content target/promotion-verified.json
  exit 0
}
$verified=Get-Content target/promotion-verified.json -Raw|ConvertFrom-Json
if($verified.runId -cne $env:SOURCE_SIGNING_RUN_ID -or $verified.sourceCommit -cne $sha -or $verified.installerSha256 -cne $files.installerSha256){throw 'Promotion verification does not match these final bytes'}
$body=Join-Path $repo 'target/promotion-release-notes.md'
$changelog=git show "${sha}:CHANGELOG.md"
if($LASTEXITCODE){throw 'Cannot read release notes from the signed source'}
$lines=@($changelog);$section=@();$collect=$false
foreach($line in $lines){if($line -ceq "## $($files.version)"){$collect=$true;continue};if($collect -and $line -match '^## '){break};if($collect){$section+=$line}}
if(-not $section.Count){throw 'Signed source has no matching changelog section'}
($section+@('','---','','Signed Windows installer. Exact qualified signing run: '+$env:SOURCE_SIGNING_RUN_ID+'.','Verify SHA256SUMS.txt, RELEASE-MANIFEST.json and GitHub build-provenance attestations.')) -join "`n"|Set-Content -LiteralPath $body -Encoding utf8
$assets=@($files.installer,(Join-Path $files.out 'SHA256SUMS.txt'),(Join-Path $files.out 'RELEASE-MANIFEST.json'),(Join-Path $files.out 'THIRD-PARTY-LICENSES.txt'))
gh release create $files.tag --target $sha --title "WrongIndex $($files.tag)" --notes-file $body --latest @assets
if($LASTEXITCODE){throw 'Exact-artifact publication failed; no asset replacement attempted'}
