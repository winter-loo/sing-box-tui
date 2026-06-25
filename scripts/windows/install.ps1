[CmdletBinding()]
param(
    [string]$Repo = "winter-loo/sing-box-tui",
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\sing-box-tui",
    [string]$SingBoxRepo = "winter-loo/sing-box",
    [string]$SingBoxVersion = "v1.13.13-winterloo.2",
    [string]$SingBoxSha256 = "dcf5be84da3361eadd22efb23df5d5426826ad51b2a7d0c07f90d938da684ec9",
    [switch]$SkipSingBox,
    [switch]$AddToPath,
    [switch]$NoPath,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

function Write-Step([string]$Message) {
    Write-Host "==> $Message"
}

function Get-ReleaseAsset {
    $releaseUrl = if ($Version -eq "latest") {
        "https://api.github.com/repos/$Repo/releases/latest"
    } else {
        "https://api.github.com/repos/$Repo/releases/tags/$Version"
    }
    $release = Invoke-RestMethod -Uri $releaseUrl -Headers @{ "User-Agent" = "sing-box-tui-installer" }
    $asset = $release.assets | Where-Object {
        $_.name -match "windows" -and $_.name -match "x86_64|x64" -and $_.name.EndsWith(".zip")
    } | Select-Object -First 1
    if (-not $asset) {
        throw "No Windows x64 zip asset found in release '$($release.tag_name)'"
    }
    return $asset
}

function Get-SingBoxReleaseAsset {
    $releaseUrl = "https://api.github.com/repos/$SingBoxRepo/releases/tags/$SingBoxVersion"
    $release = Invoke-RestMethod -Uri $releaseUrl -Headers @{ "User-Agent" = "sing-box-tui-installer" }
    $asset = $release.assets | Where-Object {
        $_.name -match "windows-amd64" -and $_.name.EndsWith(".exe")
    } | Select-Object -First 1
    if (-not $asset) {
        throw "No Windows amd64 sing-box exe asset found in $SingBoxRepo release '$SingBoxVersion'"
    }
    return $asset
}

function Add-UserPath([string]$PathToAdd) {
    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if ($current) {
        $parts = $current -split ";" | Where-Object { $_ }
    }
    if ($parts -contains $PathToAdd) {
        return
    }
    $next = (@($parts) + $PathToAdd) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $next, "User")
    $env:Path = "$env:Path;$PathToAdd"
}

function Install-SingBoxCore {
    $coreDir = Join-Path $InstallDir "core"
    $coreExe = Join-Path $coreDir "sing-box.exe"
    if ((Test-Path $coreExe) -and -not $Force) {
        Write-Step "sing-box core already installed at $coreExe"
        Add-UserPath $coreDir
        return
    }

    New-Item -ItemType Directory -Force -Path $coreDir | Out-Null
    $asset = Get-SingBoxReleaseAsset
    $download = Join-Path $env:TEMP $asset.name
    Write-Step "Downloading sing-box core $($asset.name)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $download

    if ($SingBoxSha256) {
        $actual = (Get-FileHash -Path $download -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $SingBoxSha256.ToLowerInvariant()) {
            Remove-Item -Force $download
            throw "sing-box SHA256 mismatch. Expected $SingBoxSha256 but got $actual"
        }
    }

    Copy-Item -Path $download -Destination $coreExe -Force
    Remove-Item -Force $download
    Add-UserPath $coreDir
    Write-Step "Installed sing-box core to $coreExe"
}

if (-not $SkipSingBox) {
    if (Get-Command sing-box -ErrorAction SilentlyContinue) {
        Write-Step "sing-box already found"
    } else {
        Install-SingBoxCore
    }
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exe = Join-Path $InstallDir "sing-box-tui.exe"
if ((Test-Path $exe) -and -not $Force) {
    Write-Step "sing-box-tui already installed at $exe"
} else {
    $asset = Get-ReleaseAsset
    $zip = Join-Path $env:TEMP $asset.name
    $extract = Join-Path $env:TEMP ("sing-box-tui-install-" + [Guid]::NewGuid().ToString("N"))
    Write-Step "Downloading $($asset.name)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip
    New-Item -ItemType Directory -Force -Path $extract | Out-Null
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    $downloadedExe = Get-ChildItem -Path $extract -Recurse -Filter "sing-box-tui.exe" | Select-Object -First 1
    if (-not $downloadedExe) {
        throw "Downloaded archive did not contain sing-box-tui.exe"
    }
    Copy-Item -Path $downloadedExe.FullName -Destination $exe -Force
    Remove-Item -Recurse -Force $extract
    Remove-Item -Force $zip
    Write-Step "Installed sing-box-tui to $exe"
}

if (-not $NoPath) {
    Add-UserPath $InstallDir
    Write-Step "Added $InstallDir to the user PATH"
}

Write-Host ""
Write-Host "Run:"
Write-Host "  `"$exe`" run"
