# Provider-independent regressions. Each test gets a fresh process so private
# trust doubles and expected native exit codes cannot affect another test.
$ErrorActionPreference = 'Stop'
function Invoke-RegressionProgram([string]$Program, [string[]]$Arguments) {
  $info = [Diagnostics.ProcessStartInfo]::new()
  $info.FileName = $Program
  $info.UseShellExecute = $false; $info.CreateNoWindow = $true
  $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
  foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
  $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
  try {
    $null = $process.Start()
    $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit(240000)) {
      $process.Kill($true); $null = $process.WaitForExit(5000)
      throw 'Signing regression exceeded its four-minute process deadline'
    }
    Write-Output $stdout.GetAwaiter().GetResult()
    $errorText = $stderr.GetAwaiter().GetResult()
    if ($errorText) { Write-Output $errorText }
    if ($process.ExitCode -ne 0) { throw "Signing regression failed: $([IO.Path]::GetFileName($Arguments[-1]))" }
  } finally { $process.Dispose() }
}
Invoke-RegressionProgram (Get-Command node).Source @((Join-Path $PSScriptRoot '../pe-signature-content.test.mjs'))
foreach ($test in @('courier.test.ps1','lease.test.ps1','archive.test.ps1','promotion.test.ps1','configuration.test.ps1')) {
  Invoke-RegressionProgram (Get-Command pwsh).Source @('-NoProfile','-NonInteractive','-File',(Join-Path $PSScriptRoot $test))
}
Write-Output 'PASS: provider-independent signing regressions; real provider signing and hosted qualification are separate'
