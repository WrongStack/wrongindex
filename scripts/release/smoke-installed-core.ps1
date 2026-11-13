param([Parameter(Mandatory)][string]$Installer,[Parameter(Mandatory)][string]$Manifest,[Parameter(Mandatory)][string]$GuiActorTest,[Parameter(Mandatory)][string]$Result,[string]$CertificateSha256,[string]$SignedUninstaller,[string]$KeepUninstaller)
$ErrorActionPreference='Stop'
if($CertificateSha256){
  if(-not $SignedUninstaller){throw 'Signed acceptance requires the expected signed uninstaller'}
  Import-Module (Join-Path $PSScriptRoot 'signpath/Signing.psm1') -Force
  $null=Get-ApprovedSigningIdentity $Installer $CertificateSha256
  $expectedUninstallerIdentity=Get-ApprovedSigningIdentity $SignedUninstaller $CertificateSha256
}
$existing=@(Get-ChildItem HKCU:/Software/Microsoft/Windows/CurrentVersion/Uninstall|ForEach-Object{Get-ItemProperty $_.PSPath}|Where-Object{$_.DisplayName -like 'WrongIndex*'})
if($existing.Count){throw 'An existing installation must not be replaced by a fixture test'}
$runKey=Get-Item HKCU:/Software/Microsoft/Windows/CurrentVersion/Run -ErrorAction SilentlyContinue
if($runKey -and $runKey.GetValueNames() -contains 'WrongIndex'){throw 'An existing autostart entry must not be replaced by a fixture test'}
$work=Join-Path ([IO.Path]::GetTempPath()) ('wi-native-package-'+[guid]::NewGuid().ToString('N').Substring(0,10))
New-Item -ItemType Directory -Path $work|Out-Null
$app=Join-Path $work 'app'
$project=Join-Path $work 'project'
New-Item -ItemType Directory -Path $project|Out-Null
Set-Content -LiteralPath (Join-Path $project 'entry.ts') -Value 'export function installedNativeProof() { return 1; }' -Encoding utf8
$oldWi=[Environment]::GetEnvironmentVariable('WRONGINDEX_HOME','Process')
$oldWs=[Environment]::GetEnvironmentVariable('WRONGSTACK_HOME','Process')
$oldTestDaemon=[Environment]::GetEnvironmentVariable('WRONGINDEX_TEST_DAEMON','Process')
$env:WRONGINDEX_HOME=Join-Path $work 'service'
$env:WRONGSTACK_HOME=Join-Path $work 'wrongstack'
$userPathBefore=(Get-Item HKCU:/Environment).GetValue('Path','','DoNotExpandEnvironmentNames')
$environmentKey=Get-Item HKCU:/Environment
$pathKindBefore=if($environmentKey.GetValueNames() -contains 'Path'){$environmentKey.GetValueKind('Path').ToString()}else{'Absent'}
$nativeShortcut=Join-Path ([Environment]::GetFolderPath('Programs')) 'WrongIndex Native (Preview).lnk'
$shortcutBefore=Test-Path -LiteralPath $nativeShortcut
if($shortcutBefore){throw 'An existing native shortcut must not be replaced by a fixture test'}
$desktopLinks=@((Join-Path ([Environment]::GetFolderPath('Programs')) 'WrongIndex.lnk'),(Join-Path ([Environment]::GetFolderPath('Desktop')) 'WrongIndex.lnk'))
if(@($desktopLinks|Where-Object{Test-Path -LiteralPath $_}).Count){throw 'An existing desktop shortcut must not be replaced by a fixture test'}
# The installer remembers its directory; a later real install must not go to
# this test's deleted temporary directory.
Import-Module (Join-Path $PSScriptRoot 'InstallLocation.psm1') -Force
$installLocation=Save-InstallLocation
$expected=Get-Content -LiteralPath $Manifest -Raw|ConvertFrom-Json
$report=[ordered]@{status='RUNNING';work=$work;installer=$Installer;boundary='Temporary install/core/actor/hash/uninstall proof; graphical window, clean-machine, DPI and screen-reader acceptance NOT RUN'}
$cli=Join-Path $app 'wrongindex.exe'
try {
  $setup=Start-Process -FilePath $Installer -ArgumentList '/S',"/D=$app" -PassThru -WindowStyle Hidden
  $null=$setup.Handle
  if(-not $setup.WaitForExit(180000)){throw 'Installer timeout'}
  if($setup.ExitCode -ne 0){throw "Installer exit $($setup.ExitCode)"}
  $report.installerExit=$setup.ExitCode
  $report.hashes=[ordered]@{}
  foreach($name in @('wrongindex.exe','wrongindexd.exe','wrongindex-desktop.exe','wrongindex-gpui.exe')){
    $got=(Get-FileHash -LiteralPath (Join-Path $app $name) -Algorithm SHA256).Hash.ToLowerInvariant()
    if($got -ne $expected.binaries.$name.sha256){throw "Installed hash mismatch: $name"}
    $report.hashes[$name]=$got
    if($CertificateSha256){$null=Get-ApprovedSigningIdentity (Join-Path $app $name) $CertificateSha256}
  }
  $installedUninstaller=Join-Path $app 'uninstall.exe'
  if($CertificateSha256){
    $installedIdentity=Get-ApprovedSigningIdentity $installedUninstaller $CertificateSha256
    if($installedIdentity.sha256 -cne $expectedUninstallerIdentity.sha256){throw 'Installed uninstaller differs from the signed compiler output'}
    $report.uninstallerSha256=$installedIdentity.sha256
    $report.signatureAcceptance='PASS: installer, four programs and installed uninstaller'
    if($KeepUninstaller){Copy-Item -LiteralPath $installedUninstaller -Destination $KeepUninstaller}
  }else{$report.signatureAcceptance='NOT RUN: unsigned engineering core acceptance'}
  if(-not (Test-Path -LiteralPath (Join-Path $app 'THIRD-PARTY-LICENSES.txt'))){throw 'License inventory not installed'}
  if(-not (Test-Path -LiteralPath $nativeShortcut)){throw 'Native shortcut not installed'}
  $report.nativeVersion=(node -e 'process.stdout.write(require("node:child_process").execFileSync(process.argv[1],["--version"],{windowsHide:true,timeout:15000}));' (Join-Path $app 'wrongindex-gpui.exe'))
  if($report.nativeVersion -notmatch [regex]::Escape($expected.source.version)){throw 'Wrong native GUI version'}
  & $cli daemon start --wait-seconds 20|Out-Null
  if($LASTEXITCODE){throw 'Installed daemon did not start'}
  $registration=& $cli --json project add $project --index-dir (Join-Path $work 'index') --watch pinned|ConvertFrom-Json
  if($LASTEXITCODE){throw 'Registration failed'}
  $projectId=$registration.project.projectId
  $deadline=(Get-Date).AddSeconds(60)
  do { Start-Sleep -Milliseconds 300; $state=& $cli --json project status $projectId|ConvertFrom-Json } until($state.freshness -eq 'current' -or (Get-Date) -gt $deadline)
  if($state.freshness -ne 'current'){throw 'Installed pinned project did not become current'}
  $found=& $cli --json search $projectId installedNativeProof
  if($LASTEXITCODE -or ($found -join '') -notmatch 'installedNativeProof'){throw 'Installed search failed'}
  $report.generation=$state.generation
  $report.indexing='PASS'
  $env:WRONGINDEX_TEST_DAEMON=Join-Path $app 'wrongindexd.exe'
  node -e 'const {spawnSync}=require("node:child_process"); const r=spawnSync(process.argv[1],["--ignored","--nocapture","--skip","debug_start_waits_for_draining_service_and_preserves_its_job"],{encoding:"utf8",windowsHide:true,timeout:300000}); require("node:fs").writeFileSync(process.argv[2],(r.stdout??"")+(r.stderr??"")); process.exit(r.status??1);' $GuiActorTest (Join-Path $work 'native-actor.log')
  if($LASTEXITCODE){throw 'Installed native actor integration failed'}
  if((Get-Content (Join-Path $work 'native-actor.log') -Raw) -notmatch '4 passed; 0 failed; 0 ignored'){throw 'All four shipped-service actor tests must run'}
  $report.nativeActor='PASS (4 tests including cold/idempotent start and pause/resume; debug-only drain covered separately)'
  $report.status='PASS'
} catch {
  $report.status='FAIL'
  $report.error=$_.Exception.Message
} finally {
  if(Test-Path -LiteralPath $cli){& $cli daemon stop --wait-seconds 30|Out-Null}
  $uninstaller=Join-Path $app 'uninstall.exe'
  $uninstallArguments=@('/S')
  if($CertificateSha256 -and (Test-Path -LiteralPath $uninstaller)){
    try{
      $identity=Get-ApprovedSigningIdentity $uninstaller $CertificateSha256
      if($identity.sha256 -cne $expectedUninstallerIdentity.sha256){throw 'Uninstaller changed'}
    }catch{
      # Do not execute a mismatched installed image during failure cleanup.
      # The verified compiler output handles the same owned fixture directory.
      $null=Get-ApprovedSigningIdentity $SignedUninstaller $CertificateSha256
      $uninstaller=$SignedUninstaller
      $uninstallArguments=@('/S',"_?=$app")
      $report.status='FAIL';$report.uninstallerMismatch=$true
    }
  }
  if(Test-Path -LiteralPath $uninstaller){$un=Start-Process -FilePath $uninstaller -ArgumentList $uninstallArguments -PassThru -WindowStyle Hidden;$null=$un.Handle;if(-not $un.WaitForExit(120000)){$report.status='FAIL';$report.uninstall='timeout'}else{$report.uninstall=$un.ExitCode}}
  # NSIS's original uninstaller exits after launching a temporary copy.
  # Wait for actual cleanup rather than treating parent exit as completion.
  $cleanupDeadline=(Get-Date).AddSeconds(60)
  do {
    $leftFiles=if(Test-Path -LiteralPath $app){@(Get-ChildItem -LiteralPath $app -Recurse -File).Count}else{0}
    $pathMatches=((Get-Item HKCU:/Environment).GetValue('Path','','DoNotExpandEnvironmentNames') -ceq $userPathBefore)
    $environmentKey=Get-Item HKCU:/Environment
    $pathKindAfter=if($environmentKey.GetValueNames() -contains 'Path'){$environmentKey.GetValueKind('Path').ToString()}else{'Absent'}
    $shortcutsMatch=(@($desktopLinks|Where-Object{Test-Path -LiteralPath $_}).Count -eq 0) -and ((Test-Path -LiteralPath $nativeShortcut) -eq $shortcutBefore)
    $leftEntry=@(Get-ChildItem HKCU:/Software/Microsoft/Windows/CurrentVersion/Uninstall|ForEach-Object{Get-ItemProperty $_.PSPath}|Where-Object{$_.DisplayName -like 'WrongIndex*'}).Count
    if($leftFiles -eq 0 -and $pathMatches -and $pathKindAfter -ceq $pathKindBefore -and $shortcutsMatch -and $leftEntry -eq 0){break}
    Start-Sleep -Milliseconds 500
  } while((Get-Date) -lt $cleanupDeadline)
  $report.filesLeft=if(Test-Path -LiteralPath $app){@(Get-ChildItem -LiteralPath $app -Recurse -File).Count}else{0}
  $report.pathRestored=((Get-Item HKCU:/Environment).GetValue('Path','','DoNotExpandEnvironmentNames') -ceq $userPathBefore)
  $report.pathKindRestored=($pathKindAfter -ceq $pathKindBefore)
  $report.desktopShortcutsRestored=(@($desktopLinks|Where-Object{Test-Path -LiteralPath $_}).Count -eq 0)
  $runKey=Get-Item HKCU:/Software/Microsoft/Windows/CurrentVersion/Run -ErrorAction SilentlyContinue
  $report.autostartRestored=(-not $runKey -or $runKey.GetValueNames() -notcontains 'WrongIndex')
  $report.nativeShortcutRestored=((Test-Path -LiteralPath $nativeShortcut) -eq $shortcutBefore)
  $report.uninstallEntryLeft=@(Get-ChildItem HKCU:/Software/Microsoft/Windows/CurrentVersion/Uninstall|ForEach-Object{Get-ItemProperty $_.PSPath}|Where-Object{$_.DisplayName -like 'WrongIndex*'}).Count
  try{$report.installLocationRestored=Restore-InstallLocation $installLocation}catch{$report.installLocationRestored=$false;$report.installLocationError=$_.Exception.Message}
  if($report.filesLeft -ne 0 -or -not $report.pathRestored -or -not $report.pathKindRestored -or -not $report.desktopShortcutsRestored -or -not $report.autostartRestored -or -not $report.nativeShortcutRestored -or -not $report.installLocationRestored -or $report.uninstallEntryLeft -ne 0 -or $report.uninstall -ne 0){$report.status='FAIL'}
  [Environment]::SetEnvironmentVariable('WRONGINDEX_HOME',$oldWi,'Process')
  [Environment]::SetEnvironmentVariable('WRONGSTACK_HOME',$oldWs,'Process')
  [Environment]::SetEnvironmentVariable('WRONGINDEX_TEST_DAEMON',$oldTestDaemon,'Process')
  $report|ConvertTo-Json -Depth 12|Set-Content -LiteralPath $Result -Encoding utf8
}
if($report.status -ne 'PASS'){exit 1}
