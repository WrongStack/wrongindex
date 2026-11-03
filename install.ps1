# WrongIndex installer for Windows: downloads a release from GitHub, verifies
# it, installs it for the current user (no administrator rights), and puts the
# `wrongindex` CLI on the user's PATH. Running it again updates or repairs.
#
#   irm https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.ps1 | iex
#
# or, with options:
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.ps1))) -Version v0.1.0 -InstallDir D:\Tools\WrongIndex
#
# Options (also as environment variables, for the `| iex` form):
#   -Version     release tag to install (WRONGINDEX_VERSION; default: latest)
#   -InstallDir  install directory (WRONGINDEX_INSTALL_DIR; default: the
#                installer's per-user default, or the current install)
#   -Repo        owner/name on GitHub (WRONGINDEX_REPO; default WrongStack/wrongindex)
#
# Trust: releases are not code-signed. The installer is checked against the
# release's SHA256SUMS.txt (a mismatch stops here, nothing is installed) and,
# when the GitHub CLI (`gh`) is installed and signed in, against the release's
# build-provenance attestation. Once installed, `wrongindex self-update`
# keeps it current.
#
# Everything runs in a child scope: under `irm | iex` this text runs in the
# caller's session, which must not keep its preferences or variables.
& {
  param(
    [string]$Version = $env:WRONGINDEX_VERSION,
    [string]$InstallDir = $env:WRONGINDEX_INSTALL_DIR,
    [string]$Repo = $(if ($env:WRONGINDEX_REPO) { $env:WRONGINDEX_REPO } else { 'WrongStack/wrongindex' })
  )
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'

  function Say([string]$m) { Write-Host "wrongindex-install: $m" }
  # Exit code of a native program, its output discarded. Windows PowerShell
  # turns a native program's stderr into errors, which 'Stop' makes
  # terminating even when redirected; only the exit code counts here.
  function Invoke-Native([string]$exe) {
    $ErrorActionPreference = 'Continue'
    & $exe @args *> $null
    $LASTEXITCODE
  }
  # Its standard output as one string; $LASTEXITCODE is left for the caller.
  function Invoke-NativeOutput([string]$exe) {
    $ErrorActionPreference = 'Continue'
    ((& $exe @args 2> $null) | Out-String).Trim()
  }

  if ($env:OS -ne 'Windows_NT') {
    throw 'WrongIndex currently runs on Windows only.'
  }
  if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'WrongIndex needs 64-bit Windows.'
  }
  # Windows PowerShell 5.1 may default to older TLS versions.
  try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch {}

  $api = "https://api.github.com/repos/$Repo/releases"
  $headers = @{ 'User-Agent' = 'wrongindex-install'; 'Accept' = 'application/vnd.github+json' }
  $url = if ($Version) { "$api/tags/$(if ($Version.StartsWith('v')) { $Version } else { "v$Version" })" } else { "$api/latest" }
  try {
    $release = Invoke-RestMethod -Uri $url -Headers $headers
  } catch {
    throw "no release found at $url ($($_.Exception.Message))"
  }
  $installerAsset = $release.assets | Where-Object { $_.name -like '*_x64-setup.exe' } | Select-Object -First 1
  $sumsAsset = $release.assets | Where-Object { $_.name -eq 'SHA256SUMS.txt' } | Select-Object -First 1
  if (-not $installerAsset -or -not $sumsAsset) {
    throw "release $($release.tag_name) has no Windows installer or no SHA256SUMS.txt"
  }
  Say "release $($release.tag_name): $($installerAsset.name)"

  $work = Join-Path ([IO.Path]::GetTempPath()) ('wrongindex-install-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
  New-Item -ItemType Directory -Force $work | Out-Null
  try {
    $installer = Join-Path $work $installerAsset.name
    $sums = Join-Path $work 'SHA256SUMS.txt'
    Invoke-WebRequest -Uri $sumsAsset.browser_download_url -OutFile $sums -Headers @{ 'User-Agent' = 'wrongindex-install' } -UseBasicParsing
    Invoke-WebRequest -Uri $installerAsset.browser_download_url -OutFile $installer -Headers @{ 'User-Agent' = 'wrongindex-install' } -UseBasicParsing

    # Checksum: required.
    $want = (Get-Content $sums | ForEach-Object {
        $parts = $_.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $installerAsset.name) { $parts[0].ToLower() }
      }) | Select-Object -First 1
    if (-not $want) { throw "SHA256SUMS.txt does not list $($installerAsset.name)" }
    # .NET rather than Get-FileHash: in Windows PowerShell that is a script
    # function, missing when a PowerShell 7 PSModulePath was inherited.
    $sha = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::OpenRead($installer)
    try { $got = -join ($sha.ComputeHash($stream) | ForEach-Object { $_.ToString('x2') }) } finally { $stream.Dispose(); $sha.Dispose() }
    if ($got -ne $want) { throw "checksum mismatch for $($installerAsset.name): expected $want, got $got. Nothing was installed." }
    Say 'SHA-256 verified'

    # Build provenance: when the GitHub CLI is there and signed in (it needs
    # an account to read attestations). A failed check stops the install.
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
      Say 'GitHub CLI not found: build-provenance attestation not checked (the SHA-256 was)'
    } else {
      if ((Invoke-Native gh auth status) -ne 0) {
        Say 'GitHub CLI not signed in: build-provenance attestation not checked (the SHA-256 was)'
      } else {
        if ((Invoke-Native gh attestation verify $installer --repo $Repo) -ne 0) { throw "build-provenance attestation of $($installerAsset.name) did not verify against $Repo. Nothing was installed." }
        Say 'build provenance verified (gh attestation verify)'
      }
    }

    # An existing installation is updated in place unless a directory is given.
    if (-not $InstallDir) {
      $entry = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' -ErrorAction SilentlyContinue |
        ForEach-Object { Get-ItemProperty $_.PSPath } |
        Where-Object { $_.DisplayName -like 'WrongIndex*' -and $_.InstallLocation } | Select-Object -First 1
      if ($entry) {
        $InstallDir = $entry.InstallLocation.Trim('"')
        Say "updating the installation in $InstallDir"
      }
    }
    $installArgs = @('/S')
    # NSIS: /D= must come last and is taken verbatim (no quotes).
    if ($InstallDir) { $installArgs += "/D=$InstallDir" }
    Say 'installing (per user, silent)'
    # Wait for the installer only: `Start-Process -Wait` also waits for its
    # descendants, and the installer restarts a service that was running.
    $p = Start-Process -FilePath $installer -ArgumentList $installArgs -PassThru
    $null = $p.Handle # keeps the handle, so ExitCode is there after the exit
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "the installer exited with $($p.ExitCode)" }
  } finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
  }

  # Where it went, and the CLI for this session too.
  $entry = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' -ErrorAction SilentlyContinue |
    ForEach-Object { Get-ItemProperty $_.PSPath } |
    Where-Object { $_.DisplayName -like 'WrongIndex*' } | Select-Object -First 1
  $dir = if ($InstallDir) { $InstallDir } elseif ($entry -and $entry.InstallLocation) { $entry.InstallLocation.Trim('"') } else { $null }
  $cli = if ($dir) { Join-Path $dir 'wrongindex.exe' } else { $null }
  if (-not $cli -or -not (Test-Path $cli)) { throw 'installed, but wrongindex.exe was not found where the installer reported' }
  if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
  # It must also run here: a binary missing a DLL prints nothing and fails.
  $v = Invoke-NativeOutput $cli --version
  if ($LASTEXITCODE -ne 0 -or -not $v) {
    $why = if ($LASTEXITCODE -eq -1073741515) { 'a DLL it needs is missing' } else { 'exit code 0x{0:X8}' -f $LASTEXITCODE }
    throw "installed in $dir, but wrongindex.exe does not run ($why). Please report this at https://github.com/$Repo/issues"
  }
  Say "installed $v in $dir"
  Say 'new terminals have `wrongindex` on PATH; start the service with `wrongindex daemon start` or open WrongIndex from the Start menu'
  Say 'update later with `wrongindex self-update`'
} @args
