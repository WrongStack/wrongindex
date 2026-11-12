$ErrorActionPreference = 'Stop'
function Run-Preflight([string]$Script,[string]$Mode,[hashtable]$Values,[bool]$Expected) {
  $info = [Diagnostics.ProcessStartInfo]::new()
  $info.FileName=(Get-Command pwsh).Source; $info.UseShellExecute=$false; $info.CreateNoWindow=$true
  $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
  foreach ($arg in @('-NoProfile','-File',(Join-Path $PSScriptRoot $Script),'-Mode',$Mode)) { $info.ArgumentList.Add($arg) }
  foreach ($key in @('GITHUB_REPOSITORY','GITHUB_REF','GITHUB_EVENT_NAME','SIGNPATH_ORGANIZATION_ID','SIGNPATH_PROJECT_SLUG','SIGNPATH_POLICY_SLUG','SIGNPATH_CERTIFICATE_SHA256','SIGNPATH_TOKEN_CONFIGURED','SOURCE_SIGNING_RUN_ID','QUALIFIED_SIGNING_RUN_ID')) { $null=$info.Environment.Remove($key) }
  foreach ($key in $Values.Keys) { $info.Environment[$key]=$Values[$key] }
  $child=[Diagnostics.Process]::new(); $child.StartInfo=$info
  try {
    $null=$child.Start(); $stdout=$child.StandardOutput.ReadToEndAsync(); $stderr=$child.StandardError.ReadToEndAsync()
    if (-not $child.WaitForExit(20000)) { $child.Kill($true); throw 'Preflight test deadline exceeded' }
    if (($child.ExitCode -eq 0) -ne $Expected) { throw 'Unexpected preflight acceptance/refusal' }
  } finally { $child.Dispose() }
}
$valid=@{GITHUB_REPOSITORY='WrongStack/wrongindex';GITHUB_REF='refs/heads/main';GITHUB_EVENT_NAME='workflow_dispatch';SIGNPATH_ORGANIZATION_ID='11111111-1111-1111-1111-111111111111';SIGNPATH_PROJECT_SLUG='wrongindex';SIGNPATH_POLICY_SLUG='release';SIGNPATH_CERTIFICATE_SHA256=('a'*64);SIGNPATH_TOKEN_CONFIGURED='true'}
# This success means configuration is present, not that an account/key exists.
Run-Preflight 'ci.ps1' 'CheckConfig' $valid $true
$changes=@(@{GITHUB_REPOSITORY='other/repo'},@{GITHUB_REF='refs/heads/other'},@{GITHUB_EVENT_NAME='pull_request'},@{SIGNPATH_ORGANIZATION_ID=''},@{SIGNPATH_PROJECT_SLUG=''},@{SIGNPATH_POLICY_SLUG=''},@{SIGNPATH_CERTIFICATE_SHA256=('0'*64)},@{SIGNPATH_TOKEN_CONFIGURED='false'})
foreach ($change in $changes) { $bad=$valid.Clone(); foreach($key in $change.Keys){$bad[$key]=$change[$key]}; Run-Preflight 'ci.ps1' 'CheckConfig' $bad $false }
Run-Preflight 'promote.ps1' 'Inspect' @{} $false
$promotion=$valid.Clone(); $promotion.SOURCE_SIGNING_RUN_ID='12'; $promotion.QUALIFIED_SIGNING_RUN_ID='13'
Run-Preflight 'promote.ps1' 'Inspect' $promotion $false
@{status='PASS';signingRefusals=8;promotionRefusals=2;boundary='Configuration presence/refusal only; no credentials, network request, signing or publication'} | ConvertTo-Json
