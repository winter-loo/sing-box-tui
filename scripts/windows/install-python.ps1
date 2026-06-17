param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CliArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Version = ""
$Force = $false
$CheckOnly = $false
$DryRun = $false

function Show-Usage {
    Write-Output "Usage: scripts\install-python.cmd [--version VERSION] [--force] [--check-only] [--dry-run]"
    Write-Output ""
    Write-Output "Installs Python with winget. Without --version, the latest Python.Python.3.x package id is selected."
    Write-Output "Examples:"
    Write-Output "  scripts\install-python.cmd"
    Write-Output "  scripts\install-python.cmd --version 3.12"
    Write-Output "  scripts\install-python.cmd --dry-run --force"
}

$i = 0
while ($i -lt $CliArgs.Count) {
    $arg = $CliArgs[$i]
    switch ($arg) {
        { $_ -eq "--help" -or $_ -eq "-h" } {
            Show-Usage
            exit 0
        }
        "--version" {
            if ($i + 1 -ge $CliArgs.Count) {
                throw "--version requires a value"
            }
            $Version = $CliArgs[$i + 1]
            $i += 2
            continue
        }
        "--force" {
            $Force = $true
            $i += 1
            continue
        }
        "--check-only" {
            $CheckOnly = $true
            $i += 1
            continue
        }
        "--dry-run" {
            $DryRun = $true
            $i += 1
            continue
        }
        default {
            throw "unknown argument: $arg"
        }
    }
}

function Get-ExistingPython {
    $candidates = @(
        @{ Command = "py"; Args = @("-3", "--version") },
        @{ Command = "python"; Args = @("--version") },
        @{ Command = "python3"; Args = @("--version") }
    )

    foreach ($candidate in $candidates) {
        $command = Get-Command $candidate.Command -ErrorAction SilentlyContinue
        if (-not $command) {
            continue
        }

        $output = & $candidate.Command @($candidate.Args) 2>&1
        if ($LASTEXITCODE -eq 0 -and ($output -join "`n") -match "Python 3\.") {
            return $candidate.Command
        }
    }

    return $null
}

function Resolve-LatestPythonPackageId {
    $searchOutput = winget search --id Python.Python.3 --source winget 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "winget search failed: $($searchOutput -join "`n")"
    }

    $ids = [regex]::Matches(($searchOutput -join "`n"), "Python\.Python\.3\.\d+") |
        ForEach-Object { $_.Value } |
        Sort-Object -Unique

    if (-not $ids) {
        throw "could not find any Python.Python.3.x package ids from winget"
    }

    return $ids |
        Sort-Object { [version]($_ -replace "^Python\.Python\.", "") } -Descending |
        Select-Object -First 1
}

function Resolve-VersionedInstall {
    param([string]$RequestedVersion)

    if ($RequestedVersion -match "^Python\.Python\.3\.\d+$") {
        return @{
            PackageId = $RequestedVersion
            PackageVersion = ""
        }
    }

    if ($RequestedVersion -match "^\d+\.\d+$") {
        return @{
            PackageId = "Python.Python.$RequestedVersion"
            PackageVersion = ""
        }
    }

    if ($RequestedVersion -match "^(\d+\.\d+)\.\d+$") {
        return @{
            PackageId = "Python.Python.$($Matches[1])"
            PackageVersion = $RequestedVersion
        }
    }

    throw "unsupported --version value '$RequestedVersion'. Use 3.12, 3.12.10, or Python.Python.3.12."
}

$existingPython = Get-ExistingPython
if ($existingPython -and -not $Force) {
    Write-Output "Python 3 already found: $existingPython"
    exit 0
}

if ($CheckOnly) {
    Write-Output "Python 3 not found"
    exit 1
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
    throw "winget was not found. Install App Installer from Microsoft Store, then rerun this script."
}

if ($Version) {
    $install = Resolve-VersionedInstall -RequestedVersion $Version
    $packageId = $install.PackageId
    $packageVersion = $install.PackageVersion
} else {
    $packageId = Resolve-LatestPythonPackageId
    $packageVersion = ""
}

$wingetArgs = @(
    "install",
    "-e",
    "--id",
    $packageId,
    "--source",
    "winget",
    "--accept-package-agreements",
    "--accept-source-agreements"
)

if ($packageVersion) {
    $wingetArgs += @("--version", $packageVersion)
}

if ($DryRun) {
    Write-Output ("winget " + ($wingetArgs -join " "))
    exit 0
}

Write-Output "Installing Python with winget package id: $packageId"
& winget @wingetArgs
exit $LASTEXITCODE
