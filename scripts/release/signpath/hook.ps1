param([Parameter(Mandatory)][string]$Directory, [Parameter(Mandatory)][string]$File)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'Signing.psm1') -Force
Invoke-SigningHook -Directory $Directory -File $File
