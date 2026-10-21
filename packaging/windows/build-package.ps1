param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$BinaryDirectory = "wxclipd/target/i686-pc-windows-msvc/release"
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+([-.][A-Za-z0-9.-]+)?$') { throw 'Invalid release version' }
$Root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Dist = Join-Path $Root 'dist'
$Package = Join-Path $Dist 'package'
New-Item -ItemType Directory -Force $Package | Out-Null
foreach ($Name in @('wxclipd.exe', 'wxclipd-background.exe')) {
    $Source = Join-Path (Join-Path $Root $BinaryDirectory) $Name
    if (-not (Test-Path $Source)) { throw "Missing executable: $Name" }
    Copy-Item $Source $Package
}
Copy-Item (Join-Path $Root 'LICENSE') $Package
Copy-Item (Join-Path $Root 'THIRD_PARTY.md') $Package
Copy-Item (Join-Path $PSScriptRoot 'README-Windows.md') $Package
Copy-Item (Join-Path $PSScriptRoot 'config.example.json') $Package
Copy-Item (Join-Path $Root 'wxclipd/assets/wetype.ico') $Package
New-Item -ItemType Directory -Force (Join-Path $Package 'licenses') | Out-Null
Copy-Item (Join-Path $Root 'wxclipd/assets/README.md') (Join-Path $Package 'licenses/wetype-icon-NOTICE.md')
Copy-Item (Join-Path $Root 'vendor/wx-ime-sdk/LICENSE') (Join-Path $Package 'licenses/wx-ime-sdk-LICENSE')
$CargoDirectory = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
# Include the license/notice files shipped by every downloaded registry crate.
# Some are build/test-only; including their notices is harmless and keeps the
# package independent of a separate license-generator executable.
$RegistryRoots = Get-ChildItem (Join-Path $CargoDirectory 'registry/src') -Directory
foreach ($RegistryRoot in $RegistryRoots) {
    foreach ($Crate in Get-ChildItem $RegistryRoot.FullName -Directory) {
        $Notices = Get-ChildItem $Crate.FullName | Where-Object {
            $_.Name -match '^(?i:licen[cs]e|copying|copyright|notice)([-._].*)?$' -or
            ($_.PSIsContainer -and $_.Name -match '^(?i:licenses|licences)$')
        }
        if ($Notices) {
            $Destination = Join-Path $Package "licenses/crates/$($Crate.Name)"
            New-Item -ItemType Directory -Force $Destination | Out-Null
            foreach ($Notice in $Notices) {
                Copy-Item $Notice.FullName $Destination -Recurse -Force
            }
        }
    }
}
$OpenSslLicenses = Get-ChildItem (Join-Path $CargoDirectory 'registry/src') -Filter LICENSE.txt -Recurse |
    Where-Object { $_.FullName -match 'openssl-src-[^\\]+\\openssl\\LICENSE.txt$' }
foreach ($License in $OpenSslLicenses) {
    $VersionDirectory = $License.Directory.Parent.Name
    Copy-Item $License.FullName (Join-Path $Package "licenses/$VersionDirectory-LICENSE.txt")
}
if (-not $OpenSslLicenses) { throw 'Vendored OpenSSL license was not found' }
$Portable = Join-Path $Dist "wetype-clipboard-sync-$Version-windows-x86-portable.zip"
Compress-Archive -Path (Join-Path $Package '*') -DestinationPath $Portable -Force
$Compiler = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6/ISCC.exe'
if (-not (Test-Path $Compiler)) { throw 'Inno Setup 6 is required to build the installer' }
& $Compiler "/DAppVersion=$Version" "/DPackageDir=$Package" "/DOutputDirPath=$Dist" (Join-Path $PSScriptRoot 'installer.iss')
if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
$Artifacts = Get-ChildItem $Dist -File | Where-Object { $_.Extension -in @('.exe', '.zip') }
$Lines = foreach ($Artifact in $Artifacts) {
    $Digest = (Get-FileHash $Artifact.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Digest  $($Artifact.Name)"
}
$Lines | Set-Content (Join-Path $Dist 'SHA256SUMS.txt') -Encoding ascii
