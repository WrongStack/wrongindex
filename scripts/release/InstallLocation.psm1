# The installer remembers where WrongIndex was installed in
# HKCU\Software\WrongStack\WrongIndex (default value), and a later install
# without /D goes there. The uninstaller keeps that key unless the user also
# deletes the application data. A throw-away install by a smoke test therefore
# left the key pointing at its deleted temporary directory, and the owner's
# next real install landed in that directory.
#
# Save-InstallLocation records the key (and whether its parent existed) before
# a temporary install. Restore-InstallLocation puts it back exactly afterwards:
# it removes a key, or a parent, that was not there, and restores every value
# with its kind. It returns $true only when the key reads back as saved.
$script:DefaultKey = 'HKCU:\Software\WrongStack\WrongIndex'

function Read-KeyValues([string]$Path) {
  $values = [ordered]@{}
  $key = Get-Item -LiteralPath $Path -ErrorAction SilentlyContinue
  if (-not $key) { return $null }
  foreach ($name in ($key.GetValueNames() | Sort-Object)) {
    $values[$name] = [ordered]@{
      kind  = $key.GetValueKind($name).ToString()
      value = $key.GetValue($name, $null, 'DoNotExpandEnvironmentNames')
    }
  }
  $values
}

function Get-StateText($Values) {
  if ($null -eq $Values) { return '<absent>' }
  ($Values.GetEnumerator() | ForEach-Object { "$($_.Key)|$($_.Value.kind)|$(@($_.Value.value) -join ',')" }) -join "`n"
}

function Save-InstallLocation([string]$Path = $script:DefaultKey) {
  $parent = Split-Path -Parent $Path
  [pscustomobject]@{
    path          = $Path
    parentExisted = [bool](Test-Path -LiteralPath $parent)
    values        = Read-KeyValues $Path
  }
}

function Restore-InstallLocation($State) {
  $path = $State.path
  $parent = Split-Path -Parent $path
  if ($null -eq $State.values) {
    if (Test-Path -LiteralPath $path) {
      # Only what the installer wrote: never a subkey the test did not see.
      if (@(Get-ChildItem -LiteralPath $path).Count) { return $false }
      Remove-Item -LiteralPath $path
    }
    if (-not $State.parentExisted -and (Test-Path -LiteralPath $parent)) {
      $p = Get-Item -LiteralPath $parent
      if ($p.SubKeyCount -eq 0 -and $p.ValueCount -eq 0) { Remove-Item -LiteralPath $parent }
    }
  } else {
    if (-not (Test-Path -LiteralPath $path)) { $null = New-Item -Path $path -Force }
    $key = Get-Item -LiteralPath $path
    foreach ($name in $key.GetValueNames()) {
      if (-not $State.values.Contains($name)) { Remove-ItemProperty -LiteralPath $path -Name $(if ($name -eq '') { '(default)' } else { $name }) }
    }
    foreach ($entry in $State.values.GetEnumerator()) {
      # The default value is named '(default)' for the provider.
      $name = if ($entry.Key -eq '') { '(default)' } else { $entry.Key }
      $null = New-ItemProperty -LiteralPath $path -Name $name -PropertyType $entry.Value.kind -Value $entry.Value.value -Force
    }
  }
  $now = Read-KeyValues $path
  $parentOk = $State.parentExisted -eq [bool](Test-Path -LiteralPath $parent)
  ((Get-StateText $now) -ceq (Get-StateText $State.values)) -and $parentOk
}

Export-ModuleMember -Function Save-InstallLocation, Restore-InstallLocation
