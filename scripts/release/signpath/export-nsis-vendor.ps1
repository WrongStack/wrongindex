param([Parameter(Mandatory)][string]$NsisZip,[Parameter(Mandatory)][string]$TauriUtilsDll,[Parameter(Mandatory)][string]$NsisSha256,[Parameter(Mandatory)][string]$TauriUtilsSha256,[Parameter(Mandatory)][string]$Output)
$ErrorActionPreference='Stop'
if((Get-FileHash $NsisZip).Hash.ToLowerInvariant() -cne $NsisSha256 -or (Get-FileHash $TauriUtilsDll).Hash.ToLowerInvariant() -cne $TauriUtilsSha256){throw 'Pinned vendor asset SHA-256 mismatch'}
if((Get-FileHash $NsisZip -Algorithm SHA1).Hash -cne 'DB14F8AF2EA346B786C6AE19343F61AAF5E09B8E' -or (Get-FileHash $TauriUtilsDll -Algorithm SHA1).Hash -cne '75197FEE3C6A814FE035788D1C34EAD39349B860'){throw 'Vendor assets differ from the pinned Tauri bundler baseline'}
$zip=[IO.Compression.ZipFile]::OpenRead((Resolve-Path $NsisZip).Path)
$files=@{}
try{
  foreach($name in @('NSISdl.dll','StartMenu.dll','System.dll','nsDialogs.dll')){
    $suffix='Plugins/x86-unicode/'+$name
    $entries=@($zip.Entries|Where-Object{ $_.FullName -ceq $suffix -or $_.FullName.EndsWith('/'+$suffix,[StringComparison]::Ordinal) })
    if($entries.Count -ne 1 -or $entries[0].Length -gt 10MB){throw 'Unexpected NSIS vendor archive layout'}
    $stream=$entries[0].Open();$hash=[Security.Cryptography.SHA256]::Create()
    try{$files[$name]=[Convert]::ToHexString($hash.ComputeHash($stream)).ToLowerInvariant()}finally{$stream.Dispose();$hash.Dispose()}
  }
}finally{$zip.Dispose()}
$files['nsis_tauri_utils.dll']=$TauriUtilsSha256
@{schema='wrongindex.nsis-vendor/1';tauriCli='2.12.4';tauriBundler='2.10.4';nsisVersion='3.13';nsisArchiveUrl='https://github.com/tauri-apps/binary-releases/releases/download/nsis-3.13/nsis-3.13.zip';nsisArchiveSha256=$NsisSha256;tauriUtilsUrl='https://github.com/tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll';tauriUtilsSha256=$TauriUtilsSha256;files=$files;policy='Preserve exact upstream bytes without signing them as WrongIndex'}|ConvertTo-Json -Depth 5|Set-Content -LiteralPath $Output -Encoding utf8
