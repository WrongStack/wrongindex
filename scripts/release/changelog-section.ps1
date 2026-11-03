# The lines of CHANGELOG.md under `## <version>`, up to the next `## `
# heading, trimmed of blank lines at both ends. Nothing when the version has
# no section (the release workflow refuses to publish then).
#
#   pwsh scripts/release/changelog-section.ps1 -Version 0.1.1
param([Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = 'Stop'
$file = Join-Path $PSScriptRoot '..\..\CHANGELOG.md'
$lines = Get-Content -LiteralPath $file -Encoding UTF8
$start = [array]::IndexOf($lines, "## $Version")
if ($start -lt 0) { return }
$end = $lines.Count
for ($i = $start + 1; $i -lt $lines.Count; $i++) {
  if ($lines[$i] -match '^## ') { $end = $i; break }
}
if ($end -le $start + 1) { return } # an empty section ([a..b] with b < a counts down)
$section = @($lines[($start + 1)..($end - 1)])
while ($section.Count -and -not $section[0].Trim()) { $section = @($section | Select-Object -Skip 1) }
while ($section.Count -and -not $section[-1].Trim()) { $section = @($section | Select-Object -SkipLast 1) }
$section
