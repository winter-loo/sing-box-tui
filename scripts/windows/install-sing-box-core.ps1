[CmdletBinding()]
param(
    [string]$Repo = "winter-loo/sing-box",
    [string]$Version = "v1.13.13-winterloo.2",
    [string]$InstallDir = "$env:LOCALAPPDATA\sing-box-tui\core",
    [string]$Sha256 = "dcf5be84da3361eadd22efb23df5d5426826ad51b2a7d0c07f90d938da684ec9",
    [string]$GitHubProxy = "https://deeloo.cn/anywhere",
    [ValidateRange(1, 16)]
    [int]$DownloadParts = 4,
    [ValidateRange(1, 3600)]
    [int]$DownloadTimeoutSec = 600,
    [ValidateRange(1, 600)]
    [int]$DownloadStallTimeoutSec = 30,
    [switch]$Force,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

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

function Invoke-SingleDownload([string]$Url, [string]$OutFile) {
    Invoke-WebRequest -UseBasicParsing -Uri $Url -Headers @{ "User-Agent" = "sing-box-tui-onboard"; "Accept" = "application/octet-stream" } -OutFile $OutFile -TimeoutSec $DownloadTimeoutSec
}

function Test-ProgressAvailable {
    if ($ProgressPreference -eq "SilentlyContinue") {
        return $false
    }
    try {
        return -not [Console]::IsOutputRedirected
    } catch {
        return $false
    }
}

function Invoke-ParallelDownload([string]$Url, [string]$OutFile, [int]$Parts) {
    if ($Parts -le 1) {
        Invoke-SingleDownload $Url $OutFile
        return
    }

    function Start-DownloadPartJob($Part) {
        $Part.Attempts++
        $Part.Path = "$($Part.BasePath).attempt$($Part.Attempts)"
        $Part.LastDoneBytes = [int64]0
        $Part.LastProgressAt = Get-Date
        $Part.Completed = $false
        Remove-Item -Force $Part.Path -ErrorAction SilentlyContinue
        $Part.Job = Start-Job -ArgumentList $Url, $Part.Path, $Part.Start, $Part.End, $DownloadTimeoutSec, $DownloadStallTimeoutSec -ScriptBlock {
            param([string]$Url, [string]$PartFile, [int64]$Start, [int64]$End, [int]$TimeoutSec, [int]$StallTimeoutSec)
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            $request = [System.Net.HttpWebRequest]::Create($Url)
            $request.Method = "GET"
            $request.AllowAutoRedirect = $true
            $request.UserAgent = "sing-box-tui-onboard"
            $request.Accept = "application/octet-stream"
            $request.Timeout = $TimeoutSec * 1000
            $request.ReadWriteTimeout = $StallTimeoutSec * 1000
            $request.AddRange($Start, $End)
            $response = $request.GetResponse()
            try {
                if ($response.StatusCode -ne [System.Net.HttpStatusCode]::PartialContent) {
                    throw "Server did not honor range request"
                }
                $inputStream = $response.GetResponseStream()
                $outputStream = [System.IO.File]::Create($PartFile)
                try {
                    $buffer = New-Object byte[] 1048576
                    while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $outputStream.Write($buffer, 0, $read)
                    }
                } finally {
                    $outputStream.Close()
                    $inputStream.Close()
                }
            } finally {
                $response.Close()
            }
        }
    }

    function Restart-DownloadPart($Part, [string]$Reason, [int]$MaxAttempts) {
        if ($Part.Job) {
            Stop-Job -Job $Part.Job -ErrorAction SilentlyContinue
            Remove-Job -Job $Part.Job -Force -ErrorAction SilentlyContinue
            $Part.Job = $null
        }
        if ($Part.Attempts -ge $MaxAttempts) {
            throw "Part $($Part.Index + 1)/$actualParts failed after $($Part.Attempts) attempts: $Reason"
        }
        Write-Host "Part $($Part.Index + 1)/$actualParts stalled or failed ($Reason); retrying attempt $($Part.Attempts + 1) of $MaxAttempts"
        Start-DownloadPartJob $Part
    }

    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Method = "HEAD"
    $request.AllowAutoRedirect = $true
    $request.UserAgent = "sing-box-tui-onboard"
    $request.Accept = "application/octet-stream"
    $request.Timeout = $DownloadTimeoutSec * 1000
    $request.ReadWriteTimeout = $DownloadTimeoutSec * 1000
    $response = $request.GetResponse()
    try {
        $length = [int64]$response.ContentLength
        $acceptRanges = [string]$response.Headers["Accept-Ranges"]
    } finally {
        $response.Close()
    }

    if ($length -le 0) {
        throw "Server did not provide a content length for parallel download"
    }
    if ($acceptRanges -and $acceptRanges -notmatch "(?i)bytes") {
        throw "Server does not advertise byte range support"
    }

    $actualParts = [Math]::Min($Parts, [int]$length)
    $chunkSize = [int64][Math]::Ceiling($length / [double]$actualParts)
    $partFiles = @()
    $maxPartAttempts = 3

    try {
        for ($i = 0; $i -lt $actualParts; $i++) {
            $start = [int64]($i * $chunkSize)
            $end = [Math]::Min([int64]($start + $chunkSize - 1), [int64]($length - 1))
            if ($start -gt $end) {
                break
            }
            $partFile = "$OutFile.part$i"
            Remove-Item -Force $partFile -ErrorAction SilentlyContinue
            $part = [pscustomobject]@{
                Index = $i
                Path = $partFile
                BasePath = $partFile
                Start = $start
                End = $end
                Length = [int64]($end - $start + 1)
                Job = $null
                Attempts = 0
                LastDoneBytes = [int64]0
                LastProgressAt = Get-Date
                Completed = $false
            }
            $partFiles += $part
            Start-DownloadPartJob $part
        }

        $showProgress = Test-ProgressAvailable
        while (($partFiles | Where-Object { -not $_.Completed }).Count -gt 0) {
            $doneBytes = [int64]0
            foreach ($part in $partFiles) {
                $current = [int64]0
                if (Test-Path $part.Path) {
                    $current = [Math]::Min([int64](Get-Item $part.Path).Length, [int64]$part.Length)
                }
                $doneBytes += $current

                if ($part.Completed) {
                    if ($showProgress) {
                        Write-Progress -Id ($part.Index + 1) -ParentId 0 -Activity "Part $($part.Index + 1)" -Status "100% ($($part.Length) / $($part.Length) bytes)" -PercentComplete 100
                    }
                    continue
                }

                if ($part.Job.State -eq "Completed") {
                    try {
                        Receive-Job -Job $part.Job -ErrorAction Stop | Out-Null
                    } catch {
                        Restart-DownloadPart $part $_.Exception.Message $maxPartAttempts
                        continue
                    }
                    Remove-Job -Job $part.Job -Force -ErrorAction SilentlyContinue
                    $part.Job = $null
                    if (-not (Test-Path $part.Path)) {
                        Restart-DownloadPart $part "missing part file" $maxPartAttempts
                        continue
                    }
                    $actual = [int64](Get-Item $part.Path).Length
                    if ($actual -ne $part.Length) {
                        Restart-DownloadPart $part "expected $($part.Length) bytes, got $actual" $maxPartAttempts
                        continue
                    }
                    $part.LastDoneBytes = $part.Length
                    $part.Completed = $true
                    $current = $part.Length
                } elseif ($part.Job.State -ne "Running") {
                    $details = Receive-Job -Job $part.Job -ErrorAction SilentlyContinue
                    Restart-DownloadPart $part "job state $($part.Job.State): $details" $maxPartAttempts
                    continue
                } elseif ($current -gt $part.LastDoneBytes) {
                    $part.LastDoneBytes = $current
                    $part.LastProgressAt = Get-Date
                } elseif (((Get-Date) - $part.LastProgressAt).TotalSeconds -ge $DownloadStallTimeoutSec) {
                    Restart-DownloadPart $part "no progress for $DownloadStallTimeoutSec seconds" $maxPartAttempts
                    continue
                }

                if ($showProgress) {
                    $partPercent = [int][Math]::Min(100, [Math]::Floor(($current * 100.0) / [double]$part.Length))
                    Write-Progress -Id ($part.Index + 1) -ParentId 0 -Activity "Part $($part.Index + 1)" -Status "$partPercent% ($current / $($part.Length) bytes)" -PercentComplete $partPercent
                }
            }
            if ($showProgress) {
                $totalPercent = [int][Math]::Min(100, [Math]::Floor(($doneBytes * 100.0) / [double]$length))
                Write-Progress -Id 0 -Activity "Downloading with $actualParts parallel parts" -Status "$totalPercent% ($doneBytes / $length bytes)" -PercentComplete $totalPercent
            }
            Start-Sleep -Milliseconds 300
        }

        if ($showProgress) {
            foreach ($part in $partFiles) {
                Write-Progress -Id ($part.Index + 1) -ParentId 0 -Activity "Part $($part.Index + 1)" -Completed
            }
            Write-Progress -Id 0 -Activity "Downloading with $actualParts parallel parts" -Completed
        }

        foreach ($part in $partFiles) {
            if (-not (Test-Path $part.Path)) {
                throw "Missing downloaded part $($part.Path)"
            }
            $actual = (Get-Item $part.Path).Length
            if ($actual -ne $part.Length) {
                throw "Downloaded part size mismatch for $($part.Path): expected $($part.Length), got $actual"
            }
        }

        Remove-Item -Force $OutFile -ErrorAction SilentlyContinue
        $merged = [System.IO.File]::Create($OutFile)
        try {
            foreach ($part in $partFiles) {
                $inputStream = [System.IO.File]::OpenRead($part.Path)
                try {
                    $inputStream.CopyTo($merged)
                } finally {
                    $inputStream.Close()
                }
            }
        } finally {
            $merged.Close()
        }

        $actualTotal = (Get-Item $OutFile).Length
        if ($actualTotal -ne $length) {
            throw "Downloaded file size mismatch: expected $length, got $actualTotal"
        }
    } finally {
        foreach ($part in $partFiles) {
            if ($part.Job) {
                Stop-Job -Job $part.Job -ErrorAction SilentlyContinue
                Remove-Job -Job $part.Job -Force -ErrorAction SilentlyContinue
            }
            Remove-Item -Force "$($part.BasePath).attempt*" -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-DownloadUrl([string]$Url, [string]$OutFile) {
    if ($DownloadParts -gt 1) {
        Write-Host "Downloading with $DownloadParts parallel parts: $Url"
        try {
            Invoke-ParallelDownload $Url $OutFile $DownloadParts
            return
        } catch {
            Remove-Item -Force $OutFile -ErrorAction SilentlyContinue
            Write-Host "Parallel download unavailable; falling back to single request"
        }
    }

    Write-Host "Downloading with a single request: $Url"
    Invoke-SingleDownload $Url $OutFile
}

function Invoke-GitHubAssetDownload($Asset, [string]$OutFile) {
    $downloadUrl = $Asset.browser_download_url
    if (-not $downloadUrl) {
        $downloadUrl = $Asset.url
    }

    try {
        Invoke-DownloadUrl $downloadUrl $OutFile
        return
    } catch {
        if ([string]::IsNullOrWhiteSpace($GitHubProxy)) {
            throw
        }
        Remove-Item -Force $OutFile -ErrorAction SilentlyContinue
        $proxyUrl = Join-GitHubProxyUrl $Asset.url
        Write-Host "Direct GitHub download failed; retrying through $GitHubProxy"
        Invoke-DownloadUrl $proxyUrl $OutFile
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
