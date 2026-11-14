# WrongIndex installer for Windows: downloads a release from GitHub, verifies
# it, installs it for the current user (no administrator rights), and puts the
# `wrongindex` CLI on the user's PATH. Running it again updates or repairs:
# the running service and an open WrongIndex window are stopped first and
# started again afterwards, on the new version.
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
  # Processes whose program lies in $dir.
  function Get-RunningFrom([string]$dir) {
    $prefix = [IO.Path]::GetFullPath($dir).TrimEnd('\') + '\'
    @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) })
  }
  # The service this user's data root has, as `daemon status` reports it
  # (version and pid), or $null when none answers.
  # Name of the threat Windows Security removed `path` for, or $null. Releases
  # are unsigned, so its machine-learning detection can flag a new program.
  function Get-DefenderRemoval([string]$path) {
    try {
      $d = Get-MpThreatDetection -ErrorAction Stop | Where-Object { $_.Resources -and ($_.Resources -join ';') -like "*$path*" } |
        Sort-Object InitialDetectionTime -Descending | Select-Object -First 1
      if ($d) { $n = (Get-MpThreat -ThreatID $d.ThreatID -ErrorAction SilentlyContinue).ThreatName; if ($n) { $n } else { 'a threat' } }
    } catch { }
  }
  function Get-DefenderAdvice([string]$path) {
    $t = Get-DefenderRemoval $path
    if (-not $t) { return '' }
    " Windows Security removed it as $t, a false positive of its machine-learning detection (these releases are not code-signed). Restore it in Windows Security > Virus & threat protection > Protection history > Actions > Restore, check it with ``sha256sum -c SHA256SUMS.txt`` and ``gh attestation verify``, report it at https://www.microsoft.com/wdsi/filesubmission, and run the installer again. Leave real-time protection on."
  }
  function Get-WiService([string]$cli) {
    $s = Invoke-NativeOutput $cli daemon status
    if ($LASTEXITCODE -ne 0 -or $s -notmatch 'pid=(\d+)') { return $null }
    $id = [int]$Matches[1]
    $ver = if ($s -match 'version=(\S+)') { $Matches[1] } else { '?' }
    [pscustomobject]@{ Pid = $id; Version = $ver }
  }
  # `daemon start` in a window of its own, waited for by itself: started with
  # redirected output, an older CLI could hand that pipe to the service and
  # this script would wait until the service exits.
  function Start-WiService([string]$cli) {
    $p = Start-Process -FilePath $cli -ArgumentList 'daemon', 'start', '--wait-seconds', '30' -WindowStyle Hidden -PassThru
    $null = $p.Handle
    $p.WaitForExit()
    Get-WiService $cli
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
    # An update stops what runs from the installation first (its files are
    # locked while it runs) and starts it again afterwards, on the new
    # version: the service of this user's data root if it was running (as the
    # installer itself and `self-update` do), the window if it was open (up
    # to 0.2.0 the window was wrongindex-desktop.exe; now wrongindex-gpui.exe).
    $restartService = $false
    $reopenDesktop = $false
    $openWindow = $null
    $oldCli = if ($InstallDir) { Join-Path $InstallDir 'wrongindex.exe' } else { $null }
    if ($oldCli -and (Test-Path $oldCli)) {
      $svc = Get-WiService $oldCli
      $restartService = [bool]$svc
      $openWindow = Get-RunningFrom $InstallDir | Where-Object { $_.Name -in 'wrongindex-gpui.exe', 'wrongindex-desktop.exe' } | Select-Object -First 1 -ExpandProperty Name
      $reopenDesktop = [bool]$openWindow
      if ($svc) {
        Say "stopping the service ($($svc.Version), pid $($svc.Pid))"
        $null = Invoke-Native $oldCli daemon stop --wait-seconds 30
      }
      $left = Get-RunningFrom $InstallDir
      if ($left) {
        Say "ending what still runs from ${InstallDir}: $(($left | ForEach-Object { "$($_.Name) (pid $($_.ProcessId))" }) -join ', ')"
        # A CLI older than 0.1.2 lacks the command; the installer then ends them.
        $null = Invoke-Native $oldCli setup stop-installed $InstallDir
      }
      $deadline = (Get-Date).AddSeconds(30)
      while (($left = Get-RunningFrom $InstallDir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
      if ($left) {
        if ($restartService) { $null = Start-WiService $oldCli }
        throw "still running from ${InstallDir}: $(($left | ForEach-Object { "$($_.Name) (pid $($_.ProcessId))" }) -join ', '). Close it and run the installer again. Nothing was installed."
      }
    }
    $installArgs = @('/S')
    # NSIS: /D= must come last and is taken verbatim (no quotes).
    if ($InstallDir) { $installArgs += "/D=$InstallDir" }
    Say 'installing (per user, silent)'
    # Wait for the installer only: `Start-Process -Wait` also waits for its
    # descendants.
    $p = Start-Process -FilePath $installer -ArgumentList $installArgs -PassThru
    $null = $p.Handle # keeps the handle, so ExitCode is there after the exit
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) {
      # The previous version is still in place: start what was stopped.
      if ($restartService -and (Test-Path $oldCli)) { $null = Start-WiService $oldCli }
      if ($reopenDesktop -and (Test-Path (Join-Path $InstallDir $openWindow))) { Start-Process (Join-Path $InstallDir $openWindow) }
      throw "the installer exited with $($p.ExitCode)"
    }
  } finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
  }

  # Where it went, and the CLI for this session too.
  $entry = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' -ErrorAction SilentlyContinue |
    ForEach-Object { Get-ItemProperty $_.PSPath } |
    Where-Object { $_.DisplayName -like 'WrongIndex*' } | Select-Object -First 1
  $dir = if ($InstallDir) { $InstallDir } elseif ($entry -and $entry.InstallLocation) { $entry.InstallLocation.Trim('"') } else { $null }
  $cli = if ($dir) { Join-Path $dir 'wrongindex.exe' } else { $null }
  if (-not $cli -or -not (Test-Path $cli)) { throw ('installed, but wrongindex.exe was not found where the installer reported.' + $(if ($cli) { Get-DefenderAdvice $cli })) }
  if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
  # It must also run here: a binary missing a DLL prints nothing and fails.
  $v = Invoke-NativeOutput $cli --version
  if ($LASTEXITCODE -ne 0 -or -not $v) {
    $why = if ($LASTEXITCODE -eq -1073741515) { 'a DLL it needs is missing' } else { 'exit code 0x{0:X8}' -f $LASTEXITCODE }
    throw ("installed in $dir, but wrongindex.exe does not run ($why)." + $(Get-DefenderAdvice $cli) + " Please report this at https://github.com/$Repo/issues")
  }
  Say "installed $v in $dir"
  $serviceFailed = $false
  if ($restartService) {
    $svc = Start-WiService $cli
    if ($svc) {
      Say "service started again ($($svc.Version), pid $($svc.Pid))"
    } else {
      $serviceFailed = $true
    }
  }
  if ($reopenDesktop) {
    Start-Process (Join-Path $dir 'wrongindex-gpui.exe')
    Say 'window reopened'
  }
  Say 'new terminals have `wrongindex` on PATH'
  if (-not $restartService) {
    Say 'start the service with `wrongindex daemon start` or open WrongIndex from the Start menu'
  }
  Say 'update later with `wrongindex self-update`'
  if ($serviceFailed) {
    throw "the service ran before the update but did not start again; start it with ``wrongindex daemon start`` (``wrongindex doctor`` shows why it does not)"
  }
} @args
