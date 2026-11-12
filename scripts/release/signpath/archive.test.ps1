$ErrorActionPreference='Stop'
Import-Module (Join-Path $PSScriptRoot 'Signing.psm1') -Force
$root=Join-Path ([IO.Path]::GetTempPath()) ('.tmp-wi-signing-zip-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $root
$original=Join-Path $root 'original.exe'; [IO.File]::WriteAllBytes($original,[byte[]](1..32))
function Archive([string]$Name,[object[]]$Entries){
  $dir=Join-Path $root $Name; $null=New-Item -ItemType Directory -Path $dir
  $zip=[IO.Compression.ZipFile]::Open((Join-Path $dir 'response.zip'),[IO.Compression.ZipArchiveMode]::Create)
  try{foreach($spec in $Entries){
    $entry=$zip.CreateEntry($spec.name)
    if($spec.link){$entry.ExternalAttributes=-1577123840}
    $stream=$entry.Open()
    try{$bytes=if($spec.large){[byte[]]::new(4MB+33)}else{[byte[]](1..32)};$stream.Write($bytes,0,$bytes.Length)}finally{$stream.Dispose()}
  }}finally{$zip.Dispose()}
  return $dir
}
function Refuse([string]$Name,[object[]]$Entries,[hashtable]$Expected,[string]$Pattern){
  $archive=Archive $Name $Entries; $dest=Join-Path $root ($Name+'-extracted')
  try{Expand-SigningArchive -ArchiveDirectory $archive -Destination $dest -UnsignedFiles $Expected;throw 'Expected refusal'}catch{if($_.Exception.Message -notmatch $Pattern){throw}}
  if(Test-Path -LiteralPath $dest){throw 'Invalid ZIP created an extraction directory'}
}
$expected=@{'own.exe'=$original}
$valid=Archive valid @(@{name='own.exe'})
$decoded=Join-Path $root 'valid-output'
Expand-SigningArchive -ArchiveDirectory $valid -Destination $decoded -UnsignedFiles $expected
if((Get-FileHash (Join-Path $decoded 'own.exe')).Hash -cne (Get-FileHash $original).Hash){throw 'Valid ZIP bytes changed'}
Refuse traversal @(@{name='../escape.exe'}) $expected 'unexpected path'
Refuse absolute @(@{name='C:/escape.exe'}) $expected 'unexpected path'
Refuse case @(@{name='Own.exe'}) $expected 'unexpected path'
Refuse duplicate @(@{name='own.exe'},@{name='own.exe'}) @{'own.exe'=$original;'other.exe'=$original} 'unexpected path'
Refuse symlink @(@{name='own.exe';link=$true}) $expected 'path or link'
Refuse oversized @(@{name='own.exe';large=$true}) $expected 'size bound'
Refuse count @(@{name='own.exe'},@{name='extra.exe'}) $expected 'entry count'
@{status='PASS';cases=8;boundary='ZIP extraction only; no PE signature or provider acceptance claim';evidence=$root}|ConvertTo-Json
