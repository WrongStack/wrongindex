param([Parameter(Mandatory)][string]$File)
$ErrorActionPreference = 'Stop'
$signature = Get-AuthenticodeSignature -LiteralPath $File
$digest = $null
if ($signature.SignerCertificate) {
  $hash = [Security.Cryptography.SHA256]::Create()
  try { $digest = [Convert]::ToHexString($hash.ComputeHash($signature.SignerCertificate.RawData)).ToLowerInvariant() } finally { $hash.Dispose() }
}
@{ status = $signature.Status.ToString(); certificateSha256 = $digest } | ConvertTo-Json -Compress
