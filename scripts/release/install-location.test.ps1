# Regression for InstallLocation.psm1 on a disposable HKCU key: a temporary
# install's remembered location must not outlive the test that made it.
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'InstallLocation.psm1') -Force
$base = "HKCU:\Software\WrongIndexTest-$([guid]::NewGuid().ToString('N'))"
$key = "$base\Vendor\WrongIndex"
$failures = @()
function Check([string]$Name, [bool]$Ok) { if ($Ok) { "PASS $Name" } else { "FAIL $Name"; $script:failures += $Name } }
# What the installer does: remember its directory in the default value.
function Install-Like([string]$Dir) {
  if (-not (Test-Path -LiteralPath $key)) { $null = New-Item -Path $key -Force }
  Set-Item -LiteralPath $key -Value $Dir
}
try {
  $null = New-Item -Path $base -Force

  # 1. Neither the key nor its parent existed: both go again.
  $state = Save-InstallLocation $key
  Install-Like 'C:\Temp\wi-smoke-1\app'
  Check 'absent key: restore reports success' (Restore-InstallLocation $state)
  Check 'absent key: key removed' (-not (Test-Path -LiteralPath $key))
  Check 'absent key: parent the test created removed' (-not (Test-Path -LiteralPath "$base\Vendor"))

  # 2. The owner's install was remembered: its value and kind come back, and a
  #    value the temporary install added goes.
  $null = New-Item -Path $key -Force
  Set-Item -LiteralPath $key -Value 'C:\Users\someone\AppData\Local\WrongIndex'
  $null = New-ItemProperty -LiteralPath $key -Name 'Installer Language' -PropertyType DWord -Value 1033
  $null = New-ItemProperty -LiteralPath $key -Name 'Home' -PropertyType ExpandString -Value '%LOCALAPPDATA%\WrongIndex'
  $state = Save-InstallLocation $key
  Install-Like 'C:\Temp\wi-smoke-2\app'
  Remove-ItemProperty -LiteralPath $key -Name 'Installer Language'
  $null = New-ItemProperty -LiteralPath $key -Name 'Extra' -PropertyType String -Value 'x'
  Check 'present key: restore reports success' (Restore-InstallLocation $state)
  $k = Get-Item -LiteralPath $key
  Check 'present key: default value restored' ($k.GetValue('') -ceq 'C:\Users\someone\AppData\Local\WrongIndex')
  Check 'present key: DWord value restored' ($k.GetValue('Installer Language') -eq 1033 -and $k.GetValueKind('Installer Language') -eq 'DWord')
  Check 'present key: expandable value kept unexpanded' ($k.GetValue('Home', $null, 'DoNotExpandEnvironmentNames') -ceq '%LOCALAPPDATA%\WrongIndex' -and $k.GetValueKind('Home') -eq 'ExpandString')
  Check 'present key: added value removed' (-not ($k.GetValueNames() -contains 'Extra'))

  # 3. The parent existed with other products: only the key goes.
  Remove-Item -LiteralPath $key -Recurse
  $null = New-Item -Path "$base\Vendor\WrongStack Desktop" -Force
  $state = Save-InstallLocation $key
  Install-Like 'C:\Temp\wi-smoke-3\app'
  Check 'shared parent: restore reports success' (Restore-InstallLocation $state)
  Check 'shared parent: key removed' (-not (Test-Path -LiteralPath $key))
  Check 'shared parent: sibling product kept' (Test-Path -LiteralPath "$base\Vendor\WrongStack Desktop")

  # 4. Something else created a subkey under the absent key meanwhile: refuse
  #    to delete it and report failure instead.
  $state = Save-InstallLocation $key
  Install-Like 'C:\Temp\wi-smoke-4\app'
  $null = New-Item -Path "$key\Other" -Force
  Check 'unknown subkey: restore refuses' (-not (Restore-InstallLocation $state))
  Check 'unknown subkey: kept' (Test-Path -LiteralPath "$key\Other")
} finally {
  if (Test-Path -LiteralPath $base) { Remove-Item -LiteralPath $base -Recurse -Force }
}
if ($failures.Count) { throw "install-location regressions failed: $($failures -join '; ')" }
'PASS: install location is saved and restored exactly'
