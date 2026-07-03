[CmdletBinding()]
param(
    [string]$Repo = "winter-loo/sing-box",
    [string]$Version = "v1.13.13-winterloo.2",
    [string]$InstallDir = "$env:LOCALAPPDATA\sing-box-tui\core",
    [string]$Sha256 = "dcf5be84da3361eadd22efb23df5d5426826ad51b2a7d0c07f90d938da684ec9",
    [string]$GitHubProxy = "https://deeloo.cn/anywhere",
    [switch]$Force,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

function Join-GitHubProxyUrl([string]$Url) {
    if ([string]::IsNullOrWhiteSpace($GitHubProxy)) {
        return $Url
    }
    return $GitHubProxy.TrimEnd("/") + "/" + $Url
}

function Invoke-GitHubApi([string]$Url) {
    try {
        return Invoke-RestMethod -Uri $Url -Headers @{ "User-Agent" = "sing-box-tui-onboard" }
    } catch {
        if ([string]::IsNullOrWhiteSpace($GitHubProxy)) {
            throw
        }
        Write-Host "GitHub is not directly accessible; retrying through $GitHubProxy"
        return Invoke-RestMethod -Uri (Join-GitHubProxyUrl $Url) -Headers @{ "User-Agent" = "sing-box-tui-onboard" }
    }
}

function Invoke-GitHubAssetDownload($Asset, [string]$OutFile) {
    try {
        Invoke-WebRequest -Uri $Asset.url -Headers @{ "User-Agent" = "sing-box-tui-onboard"; "Accept" = "application/octet-stream" } -OutFile $OutFile
    } catch {
        if ([string]::IsNullOrWhiteSpace($GitHubProxy)) {
            throw
        }
        Remove-Item -Force $OutFile -ErrorAction SilentlyContinue
        Write-Host "GitHub download is not directly accessible; retrying through $GitHubProxy"
        Invoke-WebRequest -Uri (Join-GitHubProxyUrl $Asset.url) -Headers @{ "User-Agent" = "sing-box-tui-onboard"; "Accept" = "application/octet-stream" } -OutFile $OutFile
    }
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

$coreExe = Join-Path $InstallDir "sing-box.exe"
if ((Test-Path $coreExe) -and -not $Force) {
    Write-Host "sing-box already installed at $coreExe"
    Add-UserPath $InstallDir
    exit 0
}

$releaseUrl = "https://api.github.com/repos/$Repo/releases/tags/$Version"
if ($DryRun) {
    Write-Host "Install sing-box from $releaseUrl to $coreExe"
    exit 0
}

$release = Invoke-GitHubApi $releaseUrl
$asset = $release.assets | Where-Object {
    $_.name -match "windows-amd64" -and $_.name.EndsWith(".exe")
} | Select-Object -First 1
if (-not $asset) {
    throw "No Windows amd64 sing-box exe asset found in $Repo release '$Version'"
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$download = Join-Path $env:TEMP $asset.name
Write-Host "Downloading $($asset.name)"
Invoke-GitHubAssetDownload $asset $download

if ($Sha256) {
    $actual = (Get-FileHash -Path $download -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256.ToLowerInvariant()) {
        Remove-Item -Force $download
        throw "sing-box SHA256 mismatch. Expected $Sha256 but got $actual"
    }
}

Copy-Item -Path $download -Destination $coreExe -Force
Remove-Item -Force $download
Add-UserPath $InstallDir
Write-Host "sing-box installed to $coreExe"
