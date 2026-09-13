param(
    [string]$SingBox = 'C:\Users\Administrator\AppData\Local\sing-box-tui\core\sing-box.exe'
)
$ErrorActionPreference = 'Stop'
$experimentDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ('sing-box-telemetry-contract-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $experimentDirectory | Out-Null
$configPath = Join-Path $experimentDirectory 'config.json'
# `check` validates only: no listeners, proxy processes, routes, or DNS changes.
$configuration = @{
    log = @{ disabled = $true }
    outbounds = @(@{ type = 'direct'; tag = 'direct' })
    experimental = @{
        v2ray_api = @{
            listen = '127.0.0.1:0'
            stats = @{ enabled = $true; outbounds = @('direct') }
        }
    }
}
$configuration | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $configPath -Encoding utf8
$version = (& $SingBox version 2>&1 | Out-String).Trim()
$checkOutput = (& $SingBox check --config $configPath 2>&1 | Out-String).Trim()
$checkExitCode = $LASTEXITCODE
$result = [ordered]@{
    timestamp_utc = [DateTime]::UtcNow.ToString('o')
    executable = $SingBox
    executable_sha256 = (Get-FileHash -LiteralPath $SingBox -Algorithm SHA256).Hash
    version = $version
    config_path = $configPath
    check_exit_code = $checkExitCode
    check_output = $checkOutput
    started_proxy_processes = 0
}
$result | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $experimentDirectory 'result.json') -Encoding utf8
$result | ConvertTo-Json -Depth 5
Write-Output "Evidence directory: $experimentDirectory"
