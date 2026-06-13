[CmdletBinding(DefaultParameterSetName = "Enable")]
param(
    [Parameter(ParameterSetName = "Enable")]
    [switch]$Enable,

    [Parameter(ParameterSetName = "Disable")]
    [switch]$Disable,

    [string]$Server = "127.0.0.1:6780",

    [string]$Override = "localhost;127.*;10.*;172.16.*;172.17.*;172.18.*;172.19.*;172.20.*;172.21.*;172.22.*;172.23.*;172.24.*;172.25.*;172.26.*;172.27.*;172.28.*;172.29.*;172.30.*;172.31.*;192.168.*;<local>"
)

$ErrorActionPreference = "Stop"

if (-not $Enable -and -not $Disable) {
    $Enable = $true
}

$internetSettings = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings"

function Update-WinInetProxySettings {
    if (-not ("WinInetProxyRefresh" -as [type])) {
        Add-Type @"
using System;
using System.Runtime.InteropServices;

public static class WinInetProxyRefresh {
    [DllImport("wininet.dll", SetLastError = true)]
    public static extern bool InternetSetOption(IntPtr hInternet, int dwOption, IntPtr lpBuffer, int dwBufferLength);
}
"@
    }

    [void][WinInetProxyRefresh]::InternetSetOption([IntPtr]::Zero, 39, [IntPtr]::Zero, 0)
    [void][WinInetProxyRefresh]::InternetSetOption([IntPtr]::Zero, 37, [IntPtr]::Zero, 0)
}

if ($Enable) {
    if ($Server -notmatch "^[^:\s]+:\d{1,5}$") {
        throw "Proxy server must use host:port format, got '$Server'"
    }

    New-ItemProperty -Path $internetSettings -Name ProxyEnable -PropertyType DWord -Value 1 -Force | Out-Null
    New-ItemProperty -Path $internetSettings -Name ProxyServer -PropertyType String -Value $Server -Force | Out-Null
    New-ItemProperty -Path $internetSettings -Name ProxyOverride -PropertyType String -Value $Override -Force | Out-Null
    Update-WinInetProxySettings
    Write-Output "Enabled Windows system proxy: $Server"
} else {
    New-ItemProperty -Path $internetSettings -Name ProxyEnable -PropertyType DWord -Value 0 -Force | Out-Null
    Update-WinInetProxySettings
    Write-Output "Disabled Windows system proxy"
}
