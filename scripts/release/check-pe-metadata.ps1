param(
  [Parameter(Mandatory)][string]$Directory,
  [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$Version,
  [string]$Result
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Windows PE metadata acceptance requires Windows' }
if ($Result -and (Test-Path -LiteralPath $Result)) { throw 'Preserve previous metadata evidence' }
$programs = foreach ($name in @('wrongindex.exe', 'wrongindexd.exe', 'wrongindex-gpui.exe', 'wrongindex-desktop.exe')) {
  $file = Join-Path $Directory $name
  if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
    @{ name = $name; status = 'FAIL'; reason = 'Missing program' }
    continue
  }
  $info = (Get-Item -LiteralPath $file).VersionInfo
  $valid = $info.ProductName -ceq 'WrongIndex' -and $info.ProductVersion -ceq $Version -and $info.FileVersion -ceq $Version
  @{ name = $name; status = $(if ($valid) { 'PASS' } else { 'FAIL' }); productName = $info.ProductName; productVersion = $info.ProductVersion; fileVersion = $info.FileVersion }
}
$report = @{ schema = 'wrongindex.pe-metadata/1'; status = $(if (@($programs | Where-Object status -ne 'PASS').Count) { 'FAIL' } else { 'PASS' }); expectedVersion = $Version; programs = @($programs); boundary = 'PE resource metadata only; does not establish code signing or runtime acceptance' }
$json = $report | ConvertTo-Json -Depth 6
if ($Result) { $json | Set-Content -LiteralPath $Result -Encoding utf8 }
Write-Output $json
if ($report.status -ne 'PASS') { throw 'Windows product/version resource acceptance failed' }
