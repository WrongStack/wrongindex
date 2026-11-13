# Signing uses a public certificate digest, never a private key or API token.
$ErrorActionPreference = 'Stop'

function Get-SigningHash([byte[]]$Bytes) {
  $hash = [Security.Cryptography.SHA256]::Create()
  try { return [Convert]::ToHexString($hash.ComputeHash($Bytes)).ToLowerInvariant() } finally { $hash.Dispose() }
}

function Get-ApprovedSigningIdentity {
  param([string]$File, [string]$CertificateSha256)
  if ($CertificateSha256 -notmatch '^[a-fA-F0-9]{64}$') { throw 'An approved public certificate SHA-256 is required' }
  $lease = [IO.File]::Open($File, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
  try {
  $info = [Diagnostics.ProcessStartInfo]::new()
  $info.FileName = (Get-Command pwsh).Source
  $info.UseShellExecute = $false; $info.CreateNoWindow = $true
  $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
  foreach ($argument in @('-NoProfile','-File',(Join-Path $PSScriptRoot 'read-signature.ps1'),'-File',$File)) { $info.ArgumentList.Add($argument) }
  $child = [Diagnostics.Process]::new(); $child.StartInfo = $info
  try {
    $null = $child.Start()
    $stdout = $child.StandardOutput.ReadToEndAsync(); $stderr = $child.StandardError.ReadToEndAsync()
    if (-not $child.WaitForExit(30000)) { $child.Kill($true); $null = $child.WaitForExit(5000); throw 'Authenticode verification exceeded its deadline' }
    if ($child.ExitCode -ne 0) { throw 'Authenticode verification failed' }
    $signature = $stdout.GetAwaiter().GetResult() | ConvertFrom-Json
  } finally { $child.Dispose() }
  if ($signature.status -cne 'Valid' -or -not $signature.certificateSha256) { throw 'Artifact has no valid Windows Authenticode signature' }
  $actual = $signature.certificateSha256
  if ($actual -cne $CertificateSha256.ToLowerInvariant()) { throw 'Artifact was signed by an unapproved certificate' }
  return @{ certificateSha256 = $actual; sha256 = (Get-FileHash -LiteralPath $File).Hash.ToLowerInvariant() }
  } finally { $lease.Dispose() }
}

function Test-SigningContent([string]$Unsigned, [string]$Signed) {
  $program = Join-Path $PSScriptRoot '../pe-signature-content.mjs'
  $output = & node $program $Unsigned $Signed 2>$null
  if ($LASTEXITCODE) { throw 'Signing changed the requested executable content' }
  return $output | ConvertFrom-Json
}

function Write-SigningJson([string]$File, $Value) {
  # Exclusive create plus a separate ready marker provides the delivery barrier.
  $bytes = [Text.Encoding]::UTF8.GetBytes(($Value | ConvertTo-Json -Depth 8))
  $stream = [IO.File]::Open($File, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
  try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
}

function Read-ProductIdentity([string]$File) { return (Get-Item -LiteralPath $File).VersionInfo }

function Expand-SigningArchive {
  param([string]$ArchiveDirectory, [string]$Destination, [hashtable]$UnsignedFiles)
  if (Test-Path -LiteralPath $Destination) { throw 'Preserve existing extracted signing response' }
  $files = @(Get-ChildItem -LiteralPath $ArchiveDirectory -File)
  if ($files.Count -ne 1 -or $files[0].Extension -cne '.zip' -or $files[0].Length -gt 512MB) { throw 'Expected one bounded signing-response ZIP' }
  $archive = [IO.Compression.ZipFile]::OpenRead($files[0].FullName)
  try {
    if ($archive.Entries.Count -ne $UnsignedFiles.Count) { throw 'Signing archive has an unexpected entry count' }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($entry in $archive.Entries) {
      if ($entry.FullName -cnotin @($UnsignedFiles.Keys) -or -not $seen.Add($entry.FullName) -or (($entry.ExternalAttributes -shr 16) -band 0xf000) -eq 0xa000) { throw 'Signing archive contains an unexpected path or link' }
      $limit = (Get-Item -LiteralPath $UnsignedFiles[$entry.FullName]).Length + 4MB
      if ($entry.Length -le 0 -or $entry.Length -gt $limit) { throw 'Signing archive entry exceeds the original-image size bound' }
    }
    $null = New-Item -ItemType Directory -Path $Destination
    foreach ($entry in $archive.Entries) {
      # The destination name is an expected literal, never a ZIP path.
      $file = Join-Path $Destination $entry.FullName
      $source = $entry.Open()
      $output = [IO.File]::Open($file, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
      try {
        $buffer = [byte[]]::new(65536); $copied = 0L
        while (($read = $source.Read($buffer,0,$buffer.Length)) -gt 0) {
          $copied += $read
          if ($copied -gt $entry.Length) { throw 'Signing ZIP exceeded its declared size' }
          $output.Write($buffer,0,$read)
        }
        if ($copied -ne $entry.Length) { throw 'Signing ZIP is truncated' }
        $output.Flush($true)
      } finally { $source.Dispose(); $output.Dispose() }
    }
  } finally { $archive.Dispose() }
}

function Enter-SigningFence([string]$Directory) {
  $timer = [Diagnostics.Stopwatch]::StartNew()
  while ($true) {
    try { return [IO.File]::Open((Join-Path $Directory 'commit.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
    catch [IO.IOException] {
      if ($timer.Elapsed.TotalSeconds -gt 30) { throw 'Signing commit fence deadline exceeded' }
      Start-Sleep -Milliseconds 50
    }
  }
}

function Stop-SigningCourier([string]$Directory) {
  $fence = Enter-SigningFence $Directory
  try {
    if (-not (Test-Path -LiteralPath (Join-Path $Directory 'abort.json'))) {
      $state = Get-Content (Join-Path $Directory 'state.json') -Raw | ConvertFrom-Json
      Write-SigningJson (Join-Path $Directory 'abort.json') @{ nonce = $state.nonce; reason = 'Signing cancelled' }
    }
  } finally { $fence.Dispose() }
}

function Initialize-SigningCourier {
  param([string]$Directory, [string]$PayloadDirectory, [string]$BuildRoot, [string]$Version, [string]$CertificateSha256, [string]$VendorManifest = (Join-Path $PSScriptRoot '../../../.signpath/nsis-vendor.json'))
  if (Test-Path -LiteralPath $Directory) { throw 'Preserve existing signing state' }
  if ($Version -notmatch '^\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.+-]+)?$') { throw 'Invalid signing version' }
  $allowed = @{}
  foreach ($name in @('wrongindex.exe', 'wrongindexd.exe', 'wrongindex-gpui.exe', 'wrongindex-desktop.exe')) {
    $file = Join-Path $PayloadDirectory $name
    $product = Read-ProductIdentity $file
    if ($product.ProductName -cne 'WrongIndex' -or $product.ProductVersion -cne $Version -or $product.FileVersion -cne $Version) { throw 'Signed payload product/version mismatch' }
    $identity = Get-ApprovedSigningIdentity $file $CertificateSha256
    $allowed[$name] = $identity.sha256
  }
  $vendor = Get-Content -LiteralPath $VendorManifest -Raw | ConvertFrom-Json
  $vendorNames = @('NSISdl.dll','StartMenu.dll','System.dll','nsDialogs.dll','nsis_tauri_utils.dll')
  if ($vendor.schema -cne 'wrongindex.nsis-vendor/1' -or $vendor.tauriCli -cne '2.12.4' -or $vendor.tauriBundler -cne '2.10.4' -or @($vendor.files.PSObject.Properties).Count -ne 5) { throw 'Unexpected pinned NSIS vendor baseline' }
  foreach ($name in $vendorNames) {
    if ($vendor.files.$name -cnotmatch '^[a-f0-9]{64}$') { throw 'Missing pinned vendor file digest' }
  }
  $buildPath = (Resolve-Path -LiteralPath $BuildRoot).Path
  $null = New-Item -ItemType Directory -Path $Directory
  Write-SigningJson (Join-Path $Directory 'state.json') @{ schema = 'wrongindex.signing-courier/1'; nonce = [guid]::NewGuid().ToString('N'); version = $Version; certificateSha256 = $CertificateSha256.ToLowerInvariant(); allowed = $allowed; vendor = $vendor.files; vendorDirectory = (Join-Path $buildPath 'release/nsis/x64/Plugins/x86-unicode'); roots = @($buildPath, [IO.Path]::GetTempPath().TrimEnd('\','/')) }
}

function Assert-CourierPath($State, [string]$Source) {
  $inside = $false
  foreach ($root in $State.roots) {
    $prefix = [IO.Path]::GetFullPath($root).TrimEnd('\','/') + [IO.Path]::DirectorySeparatorChar
    if ($Source.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { $inside = $true; break }
  }
  if (-not $inside) { throw 'Signing callback is outside the build and compiler temporary roots' }
  $item = Get-Item -LiteralPath $Source
  while ($item) {
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Signing callback crosses a reparse point' }
    $item = if ($item -is [IO.FileInfo]) { $item.Directory } else { $item.Parent }
  }
}

function Assert-CourierSource($State, [string]$Source) {
  Assert-CourierPath $State $Source
  $info = Read-ProductIdentity $Source
  if ($info.ProductName -cne 'WrongIndex' -or $info.ProductVersion -cne $State.version -or $info.FileVersion -cne $State.version) { throw 'Signing callback has the wrong product/version resources' }
}

function Read-SigningRequest {
  param([string]$Directory, [ValidateSet('uninstaller', 'installer')][string]$Phase)
  $state = Get-Content (Join-Path $Directory 'state.json') -Raw | ConvertFrom-Json
  $folder = Join-Path $Directory $Phase
  if (-not (Test-Path -LiteralPath (Join-Path $folder 'request.ready'))) { return $null }
  $request = Get-Content (Join-Path $folder 'request.json') -Raw | ConvertFrom-Json
  $expected = if ($Phase -eq 'uninstaller') { 'uninstall.exe' } else { "WrongIndex_$($state.version)_x64-setup.exe" }
  if ($request.nonce -cne $state.nonce -or $request.phase -cne $Phase -or $request.filename -cne $expected) { throw 'Signing request identity mismatch' }
  $unsigned = Join-Path $folder $expected
  if ((Get-FileHash -LiteralPath $unsigned).Hash.ToLowerInvariant() -cne $request.unsignedSha256) { throw 'Signing request bytes changed' }
  return @{ state = $state; request = $request; folder = $folder; unsigned = $unsigned }
}

function Wait-SigningRequest {
  param([string]$Directory, [ValidateSet('uninstaller', 'installer')][string]$Phase, [int]$TimeoutSeconds = 900)
  $deadline = [Diagnostics.Stopwatch]::StartNew()
  while ($deadline.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
    if (Test-Path -LiteralPath (Join-Path $Directory 'abort.json')) { throw 'Signing bundler aborted' }
    $pending = Read-SigningRequest $Directory $Phase
    if ($pending) { return $pending.unsigned }
    Start-Sleep -Milliseconds 200
  }
  throw "No $Phase signing request within deadline"
}

function Complete-SigningRequest {
  param([string]$Directory, [ValidateSet('uninstaller', 'installer')][string]$Phase, [string]$SignedFile)
  if (Test-Path -LiteralPath (Join-Path $Directory 'abort.json')) { throw 'Signing request was cancelled' }
  $pending = Read-SigningRequest $Directory $Phase
  if (-not $pending) { throw 'No matching pending signing request' }
  # Keep the same bytes across trust, content binding and staging. A digest
  # checked before a later unconstrained read would leave a replacement race.
  $signedLease = [IO.File]::Open($SignedFile, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
  try {
  $unsignedLease = [IO.File]::Open($pending.unsigned, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
  try {
  if ((Get-FileHash -LiteralPath $pending.unsigned).Hash.ToLowerInvariant() -cne $pending.request.unsignedSha256) { throw 'Signing request bytes changed before delivery' }
  $identity = Get-ApprovedSigningIdentity $SignedFile $pending.state.certificateSha256
  $null = Test-SigningContent $pending.unsigned $SignedFile
  $fence = Enter-SigningFence $Directory
  try {
  if (Test-Path -LiteralPath (Join-Path $Directory 'abort.json')) { throw 'Signing request was cancelled' }
  $responseFile = Join-Path $pending.folder 'response.exe'
  if (Test-Path -LiteralPath $responseFile) { throw 'Signing response already exists' }
  [IO.File]::Copy((Resolve-Path -LiteralPath $SignedFile).Path, $responseFile, $false)
  if ((Get-FileHash -LiteralPath $responseFile).Hash.ToLowerInvariant() -cne $identity.sha256) { throw 'Signing response changed while staged' }
  Write-SigningJson (Join-Path $pending.folder 'response.json') @{ nonce = $pending.state.nonce; phase = $Phase; unsignedSha256 = $pending.request.unsignedSha256; signedSha256 = $identity.sha256 }
  Write-SigningJson (Join-Path $pending.folder 'response.ready') @{ ready = $true }
  } finally { $fence.Dispose() }
  } finally { $unsignedLease.Dispose() }
  } finally { $signedLease.Dispose() }
}

function Invoke-SigningHook {
  param([string]$Directory, [string]$File, [int]$TimeoutSeconds = 5400)
  $state = Get-Content (Join-Path $Directory 'state.json') -Raw | ConvertFrom-Json
  if (Test-Path -LiteralPath (Join-Path $Directory 'abort.json')) { throw 'Signing job aborted' }
  $source = (Resolve-Path -LiteralPath $File).Path
  $name = [IO.Path]::GetFileName($source)
  Assert-CourierPath $state $source
  # Tauri calls the hook for sidecars under <name>-<target>.exe too. The
  # validated bytes, not a filename alias, identify an already-signed image.
  $currentSha = (Get-FileHash -LiteralPath $source).Hash.ToLowerInvariant()
  if ($name.EndsWith('.dll', [StringComparison]::OrdinalIgnoreCase)) {
    # These five pinned upstream plugins keep their original bytes. They are
    # never submitted to SignPath or represented as WrongIndex-signed files.
    $vendorEntry = @($state.vendor.PSObject.Properties | Where-Object { $_.Name -ceq $name })
    $vendorDirectory = if ($name -ceq 'nsis_tauri_utils.dll') { Join-Path $state.vendorDirectory 'additional' } else { $state.vendorDirectory }
    if ([IO.Path]::GetDirectoryName($source) -ine $vendorDirectory -or $vendorEntry.Count -ne 1 -or $vendorEntry[0].Value -cne $currentSha) { throw 'Unrecognized or modified upstream NSIS plugin' }
    Write-Output "Preserved pinned upstream plugin: $name"
    return
  }
  $known = @($state.allowed.PSObject.Properties | Where-Object { $_.Value -ceq $currentSha })
  if ($known.Count) {
    $identity = Get-ApprovedSigningIdentity $source $state.certificateSha256
    if ($identity.sha256 -cne $currentSha) { throw 'Already-signed payload bytes changed' }
    return
  }
  Assert-CourierSource $state $source
  $phase = if ($name -ceq "WrongIndex_$($state.version)_x64-setup.exe") { 'installer' } else { 'uninstaller' }
  if ($phase -eq 'installer' -and -not (Test-Path -LiteralPath (Join-Path $Directory 'uninstaller/completed.json'))) { throw 'Installer arrived before the signed uninstaller' }
  $folder = Join-Path $Directory $phase
  if (Test-Path -LiteralPath $folder) { throw 'Unexpected duplicate signing request' }
  $null = New-Item -ItemType Directory -Path $folder
  # NSIS is waiting for its hook; retain exclusive ownership of the exact file
  # through delivery so a late response cannot overwrite a changed executable.
  $stream = [IO.File]::Open($source, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
  try {
    if ($stream.Length -gt 512MB) { throw 'Signing input exceeds the size bound' }
    $memory = [IO.MemoryStream]::new()
    try { $stream.CopyTo($memory); $bytes = $memory.ToArray() } finally { $memory.Dispose() }
    $unsignedSha = Get-SigningHash $bytes
    $filename = if ($phase -eq 'uninstaller') { 'uninstall.exe' } else { $name }
    $unsigned = Join-Path $folder $filename
    [IO.File]::WriteAllBytes($unsigned, $bytes)
    Write-SigningJson (Join-Path $folder 'request.json') @{ nonce = $state.nonce; phase = $phase; filename = $filename; unsignedSha256 = $unsignedSha }
    Write-SigningJson (Join-Path $folder 'request.ready') @{ ready = $true }
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath (Join-Path $folder 'response.ready'))) {
      if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw 'Signing response deadline expired' }
      if (Test-Path -LiteralPath (Join-Path $Directory 'abort.json')) { throw 'Signing job aborted' }
      Start-Sleep -Milliseconds 200
    }
    $response = Get-Content (Join-Path $folder 'response.json') -Raw | ConvertFrom-Json
    if ($response.nonce -cne $state.nonce -or $response.phase -cne $phase -or $response.unsignedSha256 -cne $unsignedSha) { throw 'Signing delivery identity mismatch' }
    $signed = Join-Path $folder 'response.exe'
    $deliveryLease = [IO.File]::Open($signed, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
    $identity = Get-ApprovedSigningIdentity $signed $state.certificateSha256
    if ($identity.sha256 -cne $response.signedSha256) { throw 'Delivered signing bytes changed' }
    $null = Test-SigningContent $unsigned $signed
    $signedBytes = [IO.File]::ReadAllBytes($signed)
    $fence = Enter-SigningFence $Directory
    try {
    if (Test-Path -LiteralPath (Join-Path $Directory 'abort.json')) { throw 'Signing job aborted before commit' }
    $stream.Position = 0; $stream.SetLength($signedBytes.Length)
    $stream.Write($signedBytes, 0, $signedBytes.Length); $stream.Flush($true)
    Write-SigningJson (Join-Path $folder 'completed.json') @{ nonce = $state.nonce; phase = $phase; sha256 = $identity.sha256; certificateSha256 = $identity.certificateSha256 }
    } finally { $fence.Dispose() }
    } finally { $deliveryLease.Dispose() }
  } finally { $stream.Dispose() }
}

Export-ModuleMember -Function Get-ApprovedSigningIdentity, Test-SigningContent, Initialize-SigningCourier, Read-SigningRequest, Wait-SigningRequest, Complete-SigningRequest, Invoke-SigningHook, Stop-SigningCourier, Expand-SigningArchive
