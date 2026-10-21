param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$BinaryDirectory = "wxclipd/target/i686-pc-windows-msvc/release"
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+([-.][A-Za-z0-9.-]+)?$') { throw 'Invalid release version' }
$Root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Dist = Join-Path $Root 'dist'
$Package = Join-Path $Dist 'package'
if (Test-Path $Package) { Remove-Item $Package -Recurse -Force }
New-Item -ItemType Directory -Force $Package | Out-Null
foreach ($Name in @('wxclipd.exe', 'wxclipd-background.exe')) {
    $Source = Join-Path (Join-Path $Root $BinaryDirectory) $Name
    if (-not (Test-Path $Source)) { throw "Missing executable: $Name" }
    Copy-Item $Source $Package
}
$WpfPublish = Join-Path $Dist 'wpf'
if (Test-Path $WpfPublish) { Remove-Item $WpfPublish -Recurse -Force }
& dotnet publish (Join-Path $Root 'ui/WeTypeClipboard.Wpf/WeTypeClipboard.Wpf.csproj') `
    --configuration Release --runtime win-x86 --self-contained true --output $WpfPublish `
    "-p:Version=$Version"
if ($LASTEXITCODE -ne 0) { throw 'WPF x86 self-contained publication failed' }
foreach ($Name in @('wxclipd-ui.exe', 'wxclipd-ui.dll', 'wxclipd-ui.runtimeconfig.json', 'coreclr.dll', 'hostfxr.dll', 'PresentationFramework.dll')) {
    if (-not (Test-Path (Join-Path $WpfPublish $Name) -PathType Leaf)) { throw "Missing self-contained WPF file: $Name" }
}
Copy-Item $WpfPublish (Join-Path $Package 'ui') -Recurse
Copy-Item (Join-Path $Root 'LICENSE') $Package
Copy-Item (Join-Path $Root 'THIRD_PARTY.md') $Package
Copy-Item (Join-Path $PSScriptRoot 'README-Windows.md') $Package
Copy-Item (Join-Path $PSScriptRoot 'config.example.json') $Package
Copy-Item (Join-Path $Root 'wxclipd/assets/wetype.ico') $Package
New-Item -ItemType Directory -Force (Join-Path $Package 'licenses') | Out-Null
Copy-Item (Join-Path $Root 'wxclipd/assets/README.md') (Join-Path $Package 'licenses/wetype-icon-NOTICE.md')
Copy-Item (Join-Path $Root 'vendor/wx-ime-sdk/LICENSE') (Join-Path $Package 'licenses/wx-ime-sdk-LICENSE')
$NugetDirectory = if ($env:NUGET_PACKAGES) { $env:NUGET_PACKAGES } else { Join-Path $env:USERPROFILE '.nuget/packages' }
$WpfDependencies = Get-Content (Join-Path $WpfPublish 'wxclipd-ui.deps.json') -Raw | ConvertFrom-Json
$RuntimePacks = @($WpfDependencies.libraries.PSObject.Properties.Name | Where-Object { $_ -match '^runtimepack\.Microsoft\.(NETCore|WindowsDesktop)\.App\.Runtime\.win-x86/' })
if ($RuntimePacks.Count -ne 2) { throw 'Cannot identify both published .NET runtime packs' }
foreach ($RuntimePack in $RuntimePacks) {
    $PackagePath = $RuntimePack.Substring('runtimepack.'.Length).ToLowerInvariant()
    $RuntimeDirectory = Join-Path $NugetDirectory $PackagePath
    $Notices = @(Get-ChildItem $RuntimeDirectory -File | Where-Object { $_.Name -match '^(LICENSE|THIRD-PARTY-NOTICES)(\..*)?$' })
    if (-not ($Notices.Name -match '^LICENSE(\..*)?$')) { throw "Missing .NET runtime license: $RuntimePack" }
    if ($PackagePath.StartsWith('microsoft.netcore.') -and -not ($Notices.Name -match '^THIRD-PARTY-NOTICES(\..*)?$')) {
        throw 'Missing .NET Core third-party notices'
    }
    $Destination = Join-Path $Package "licenses/dotnet/$($PackagePath.Replace('/', '-'))"
    New-Item -ItemType Directory -Force $Destination | Out-Null
    $Notices | Copy-Item -Destination $Destination
}
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
