[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$InstallDir = "",
    [switch]$NoModifyPath
)

$ErrorActionPreference = "Stop"
$Repository = "openprx/prx"

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $localAppData = [Environment]::GetFolderPath("LocalApplicationData")
    $InstallDir = Join-Path $localAppData "Programs\PRX"
}

if ($Version -eq "latest") {
    $releasePath = "latest/download"
}
elseif ($Version -match '^v?\d+\.\d+\.\d+(?:[-.][0-9A-Za-z.-]+)?$') {
    if (-not $Version.StartsWith("v")) {
        $Version = "v$Version"
    }
    $releasePath = "download/$Version"
}
else {
    throw "Version must be 'latest' or a semantic version such as v0.8.20."
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($architecture -ne [System.Runtime.InteropServices.Architecture]::X64) {
    throw "Unsupported Windows architecture: $architecture. Current releases support Windows x64."
}

$archiveName = "prx-windows-amd64.zip"
$baseUrl = "https://github.com/$Repository/releases/$releasePath"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("prx-install-" + [guid]::NewGuid())
$archivePath = Join-Path $tempDir $archiveName
$checksumPath = "$archivePath.sha256"
$extractDir = Join-Path $tempDir "extract"

New-Item -ItemType Directory -Path $tempDir | Out-Null
try {
    Write-Host "==> Downloading $archiveName"
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$archiveName" -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$archiveName.sha256" -OutFile $checksumPath

    $checksumContent = Get-Content -Raw $checksumPath
    $checksumMatch = [regex]::Match($checksumContent, '(?i)\b[0-9a-f]{64}\b')
    if (-not $checksumMatch.Success) {
        throw "Release checksum is malformed."
    }
    $expected = $checksumMatch.Value.ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA-256 verification failed."
    }

    Expand-Archive -Path $archivePath -DestinationPath $extractDir
    $sourceBinary = Join-Path $extractDir "prx.exe"
    if (-not (Test-Path -PathType Leaf $sourceBinary)) {
        throw "Release archive does not contain prx.exe."
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $destination = Join-Path $InstallDir "prx.exe"
    $temporary = Join-Path $InstallDir (".prx-" + [guid]::NewGuid() + ".tmp")
    Copy-Item $sourceBinary $temporary
    Move-Item -Force $temporary $destination

    if (-not $NoModifyPath) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $pathParts = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($pathParts -notcontains $InstallDir) {
            $newPath = (($pathParts + $InstallDir) -join ';')
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        }
        if (($env:Path -split ';') -notcontains $InstallDir) {
            $env:Path = "$InstallDir;$env:Path"
        }
    }

    Write-Host "==> Installed PRX to $destination"
    & $destination --version
    Write-Host ""
    Write-Host "Next:"
    Write-Host "  prx onboard --interactive"
    Write-Host ""
    Write-Host "Optional daemon service:"
    Write-Host "  prx service install"
    Write-Host "  prx service start"
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tempDir
}
