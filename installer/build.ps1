# Builds the release exe and the per-user MSI into installer\out\.
# Needs the WiX v5 dotnet tool: dotnet tool install --global wix
param([string]$Version = "")
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Version) {
  $toml = Get-Content "$root\Cargo.toml" -Raw
  if ($toml -match 'version\s*=\s*"([^"]+)"') { $Version = $Matches[1] } else { $Version = "0.1.0" }
}

cargo build --release -p cc-app
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$exe = "$root\target\release\casecomplete.exe"
if (-not (Test-Path "$PSScriptRoot\casecomplete.ico")) { python "$PSScriptRoot\make_icon.py" }
New-Item -ItemType Directory -Force "$PSScriptRoot\out" | Out-Null
$msi = "$PSScriptRoot\out\CaseComplete-$Version.msi"

Set-Location $PSScriptRoot
wix build Package.wxs -ext WixToolset.UI.wixext -ext WixToolset.Util.wixext `
  -d "Version=$Version" -d "ExePath=$exe" -arch x64 -o $msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
Write-Output "MSI: $msi"
