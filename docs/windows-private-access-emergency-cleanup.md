# Windows Private Access 应急清理手册

本文用于 Hillstone 或 SonicWall TUN 会话异常退出，且 `sing-box-tui` 无法自行恢复
Windows 网络状态时的人工清理。适用于本项目写入的以下运行时状态：

- 命令行包含 `private-access-tun-helper` 的辅助进程；
- `Comment` 为 `sing-box-tui private access pid=<PID>` 的 NRPT 规则；
- Private Access 专用 TUN 接口上的运行时 IPv4 路由；
- Private Access 专用 TUN 接口上的 DNS 服务器配置；
- WinINet 系统代理中可能残留的临时域名 bypass。

所有命令都应在“以管理员身份运行”的 PowerShell 中执行。先执行查看命令，确认目标
确实属于 Private Access，再执行删除命令。

> **禁止批量清空系统配置。** 不要执行
> `Get-DnsClientNrptRule | Remove-DnsClientNrptRule`、
> `Get-NetRoute | Remove-NetRoute`、`netsh int ip reset` 或 Windows“网络重置”。
> 这些操作会同时删除公司策略、其他 VPN 和用户自己的网络配置。

## 1. 保存现场并退出相关程序

先正常退出 TUI，并关闭仍在运行的 Hillstone/SonicWall 会话。保存下面命令的输出，便于
排查和确认清理范围：

```powershell
Get-Date
Get-CimInstance Win32_Process |
  Where-Object { $_.CommandLine -match '(?i)private-access-(tun-helper|service)' } |
  Select-Object ProcessId, Name, CommandLine

Get-DnsClientNrptRule |
  Where-Object { $_.Comment -match '^sing-box-tui private access pid=\d+$' } |
  Select-Object Name, Namespace, NameServers, DisplayName, Comment

Get-NetAdapter -IncludeHidden |
  Select-Object ifIndex, Name, InterfaceDescription, Status
```

日志中可查到 Hillstone 最近使用过的接口、客户端地址和下发路由：

```powershell
Select-String -Path .\hillstone-private-access.log `
  -Pattern 'helper_interface:|client_ipv4:|installed_routes:' |
  Select-Object -Last 12
```

不要把完整日志公开发送给第三方。虽然程序会隐藏密钥和会话 ID，日志仍可能包含公司
网关、内网地址等信息。

## 2. 清理残留的 Private Access helper

再次列出 helper。只有命令行明确包含 `private-access-tun-helper` 的进程才属于本步骤：

```powershell
$PrivateAccessHelpers = @(
  Get-CimInstance Win32_Process |
    Where-Object { $_.CommandLine -match '(?i)\bprivate-access-tun-helper\b' }
)
$PrivateAccessHelpers | Select-Object ProcessId, Name, CommandLine
```

确认列表无误后，逐个填写并停止明确确认过的 PID。停止 helper 会立即断开对应的
Private Access 会话：

```powershell
$ConfirmedHelperPids = @(1234, 5678) # 替换为上一步确认过的 PID
$ConfirmedHelperPids | ForEach-Object {
  Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue
}
Start-Sleep -Seconds 2
```

如果第一步还发现命令行包含 `private-access-service` 的孤立服务进程，先检查其完整
命令行和 PID；仅在确认它不是仍需保留的会话后，用同样方法停止。

## 3. 精确清理本程序创建的 NRPT 规则

程序创建的 NRPT 规则具有固定 Comment 标记。先只查看：

```powershell
$TuiNrptRules = @(
  Get-DnsClientNrptRule |
    Where-Object { $_.Comment -match '^sing-box-tui private access pid=\d+$' }
)
$TuiNrptRules |
  Format-Table Name, Namespace, NameServers, Comment -AutoSize
```

确认输出中的规则都属于本程序后，按规则的唯一 `Name` 删除：

```powershell
foreach ($Rule in $TuiNrptRules) {
  Remove-DnsClientNrptRule -Name $Rule.Name -Force -Confirm:$false
}
Clear-DnsClientCache
```

验证没有本程序的 NRPT 残留：

```powershell
@(
  Get-DnsClientNrptRule |
    Where-Object { $_.Comment -match '^sing-box-tui private access pid=\d+$' }
).Count
```

预期结果为 `0`。没有上述 Comment 标记的 NRPT 规则可能来自域策略、公司 VPN 或其他
软件，不应删除。重启 Windows 不一定会删除 NRPT，因此必须完成本步骤。

## 4. 检查并清理 TUN 路由和接口 DNS

正常情况下，停止 helper 后 TUN 接口及其路由会随设备句柄关闭而消失。只有接口或路由
仍然存在时才需要人工处理。

先查找候选接口，但不要仅凭名称自动删除。接口常见名称为 `tun0` 或 `tun<数字>`，仍需
结合日志中的 `helper_interface`、`client_ipv4` 和接口地址确认：

```powershell
Get-NetAdapter -IncludeHidden |
  Where-Object { $_.Name -match '^tun\d*$' } |
  Select-Object ifIndex, Name, InterfaceDescription, Status

Get-NetIPAddress -AddressFamily IPv4 |
  Sort-Object InterfaceIndex |
  Format-Table InterfaceIndex, InterfaceAlias, IPAddress, PrefixLength -AutoSize
```

确认某个接口确实是本次 Private Access 专用接口后，手工填写它的索引：

```powershell
$PrivateAccessIfIndex = 42 # 替换为已经确认的接口索引

Get-NetRoute -InterfaceIndex $PrivateAccessIfIndex -AddressFamily IPv4 |
  Sort-Object DestinationPrefix |
  Format-Table DestinationPrefix, NextHop, RouteMetric, Protocol, State -AutoSize

Get-DnsClientServerAddress -InterfaceIndex $PrivateAccessIfIndex -AddressFamily IPv4
```

程序通过 Windows IP Helper API 创建的路由，其 `Protocol` 为 `NetMgmt`。在已经确认这是
Private Access 专用接口的前提下，可以只删除该接口上的 `NetMgmt` 路由：

```powershell
$PrivateAccessRoutes = @(
  Get-NetRoute -InterfaceIndex $PrivateAccessIfIndex -AddressFamily IPv4 |
    Where-Object { $_.Protocol -eq 'NetMgmt' }
)
$PrivateAccessRoutes |
  Format-Table DestinationPrefix, NextHop, RouteMetric, Protocol -AutoSize

foreach ($Route in $PrivateAccessRoutes) {
  Remove-NetRoute -InputObject $Route -Confirm:$false
}
```

然后仅重置这个已确认接口的 DNS：

```powershell
Set-DnsClientServerAddress `
  -InterfaceIndex $PrivateAccessIfIndex `
  -ResetServerAddresses
Clear-DnsClientCache
```

不要删除其他接口上的 `NetMgmt` 路由。如果 TUN 接口仍存在但已无法使用，优先重启
Windows，让临时接口和运行时路由由系统释放；不要手工卸载 Wintun、WireGuard 或其他
网络驱动，因为它们可能被别的 VPN 共用。

## 5. 检查系统代理 bypass

动态域名在 TUN 存活期间会临时加入当前用户的 WinINet `ProxyOverride`。异常终止时，
这些域名可能留在系统代理设置中。先查看，不要整项清空：

```powershell
$InternetSettings = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
Get-ItemProperty -Path $InternetSettings |
  Select-Object ProxyEnable, ProxyServer, ProxyOverride
```

优先在 TUI 恢复后关闭再开启一次 System Proxy，让程序按当前活动会话重建 bypass。
如果 TUI 完全无法运行，请打开：

`Windows 设置 -> 网络和 Internet -> 代理`

可以临时关闭“使用代理服务器”，或只删除已经确认来自已断开 Private Access 会话的
域名。不要删除 `localhost`、`127.*`、`<local>` 以及用户在
`sing-box-tui.json` 的 `bypass_entries` 中明确配置的条目。

## 6. 最终验证

```powershell
# 不应再有孤立 helper
Get-CimInstance Win32_Process |
  Where-Object { $_.CommandLine -match '(?i)\bprivate-access-tun-helper\b' } |
  Select-Object ProcessId, Name, CommandLine

# 应输出 0
@(
  Get-DnsClientNrptRule |
    Where-Object { $_.Comment -match '^sing-box-tui private access pid=\d+$' }
).Count

# 若接口仍存在，确认不再有程序创建的 NetMgmt 路由
Get-NetRoute -InterfaceIndex $PrivateAccessIfIndex -AddressFamily IPv4 `
  -ErrorAction SilentlyContinue |
  Where-Object { $_.Protocol -eq 'NetMgmt' }

Clear-DnsClientCache
Resolve-DnsName www.microsoft.com
Test-NetConnection www.microsoft.com -Port 443
```

如果公共 DNS 和网络访问仍不正常：

1. 确认系统代理是否仍指向一个已经停止的本地端口；
2. 确认没有误删公司域策略下发的 NRPT；
3. 重启 Windows，以清除临时 TUN 接口和非持久路由；
4. 重启后再次检查本程序标记的 NRPT，因为 NRPT 可能跨重启保留；
5. 保留第一步的输出和两个 Private Access 日志，再交给维护人员处理。

## 清理范围说明

- NRPT 可以通过本程序的固定 Comment 精确识别，可以自动筛选后逐条删除。
- TUN 路由本身没有独立标签，只能在确认过的专用接口上按 `Protocol=NetMgmt` 清理。
- DNS 服务器设置只能对确认过的 Private Access 接口执行重置。
- ProxyOverride 没有逐条来源标签，因此只能对已知动态域名人工清理，不能整项覆盖。
