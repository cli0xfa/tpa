param([string]$Platform = "Windows-X64")
$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version = "([^"]+)"')
if (-not $versionMatch.Success) { throw 'Package version was not found' }
$version = $versionMatch.Groups[1].Value
$executable = if ($Platform.StartsWith('Windows')) { 'tpa.exe' } else { 'tpa' }
$binary = Join-Path $projectRoot "target/release/$executable"
if (-not (Test-Path -LiteralPath $binary)) { throw 'Run cargo build --release first' }
$packageName = "tpa-$version-$Platform"
$packageDirectory = Join-Path $projectRoot "dist/$packageName"
New-Item -ItemType Directory -Path $packageDirectory -Force | Out-Null
Copy-Item -LiteralPath $binary -Destination $packageDirectory
foreach ($document in @('README.md', 'LICENSE', 'CHANGELOG.md')) {
    Copy-Item -LiteralPath (Join-Path $projectRoot $document) -Destination $packageDirectory
}
Copy-Item -LiteralPath (Join-Path $projectRoot 'docs') -Destination $packageDirectory -Recurse -Force
$archive = Join-Path $projectRoot "dist/$packageName.zip"
Compress-Archive -LiteralPath $packageDirectory -DestinationPath $archive -Force
Get-FileHash -LiteralPath $archive -Algorithm SHA256 | Format-List
Write-Output $archive
