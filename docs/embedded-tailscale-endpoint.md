# sing-box 内置 Tailscale endpoint 配置说明

本文记录 2026-08-25 对运行时 `config.json` 所做的内置 Tailscale 配置修改、修改原因、现场验证结果及回滚方式。

## 背景

原配置同时运行：

- sing-box TUN，负责普通互联网流量；
- 独立 Tailscale 客户端，负责 Tailnet 路由和系统 DNS。

两套 TUN 的 IP 路由可以共存，但独立 Tailscale DNS 的公网查询路径没有经过 sing-box 的远程 DNS，导致 `chatgpt.com`、`api.openai.com` 等域名得到错误地址。`pi` 最终表现为 WebSocket 连接失败和 `fetch failed`。

本次修改改用 sing-box 1.13.13 内置的 Tailscale endpoint，由 sing-box 在同一套路由和 DNS 策略中处理 Tailnet。外部 Tailscale 客户端随后通过 `tailscale down` 停止。

## 配置变更

### 1. 增加 Tailscale endpoint

在顶层增加：

```json
"endpoints": [
  {
    "type": "tailscale",
    "tag": "ts-ep",
    "state_directory": ".local/tailscale-embedded",
    "hostname": "macbook-deeloo-cn-1-sing-box",
    "accept_routes": true,
    "system_interface": false
  }
]
```

字段说明：

- `tag: "ts-ep"`：DNS 和路由规则引用的 endpoint 标识。
- `state_directory`：保存 Tailscale 登录状态。该目录包含敏感认证材料，已位于 Git 忽略的 `.local/` 下，不应提交。
- `hostname`：该内置 endpoint 在 Tailnet 中注册的独立节点名。
- `accept_routes: true`：接受其他 Tailnet 节点通告的子网路由。
- `system_interface: false`：使用 userspace endpoint，不再创建另一张系统 TUN；系统只保留 sing-box TUN。

`state_directory` 必须保持为配置目录下的相对路径。macOS privileged helper 会检查配置引用路径；使用绝对路径时，即使目标实际位于仓库内，也会被 helper 拒绝。

### 2. 增加内置 MagicDNS server

在 `dns.servers` 前部增加：

```json
{
  "type": "tailscale",
  "tag": "tailscale-dns",
  "endpoint": "ts-ep",
  "accept_default_resolvers": false
}
```

`accept_default_resolvers: false` 表示该服务器只负责 MagicDNS，不把普通公网查询交给 Tailscale 的默认 resolver。公网 DNS 继续使用原有的远程 DoT，避免再次得到污染结果。

### 3. 增加 DNS 分流规则

在现有 `dns.rules` 前增加：

```json
{
  "domain_suffix": [
    "taila6b1f7.ts.net"
  ],
  "server": "tailscale-dns"
},
{
  "domain_suffix": [
    "tailscale.com",
    "tailscale.io"
  ],
  "server": "remote"
}
```

第一条只把当前 Tailnet 的 MagicDNS 后缀交给内置 Tailscale DNS。第二条确保 Tailscale 控制平面和相关服务域名经原有远程 DNS 解析。

控制平面规则是必需的。只配置 MagicDNS server 时，`controlplane.tailscale.com` 会按设计得到 `NXDOMAIN`，endpoint 无法进入交互登录流程。

### 4. 增加 Tailnet 路由规则

在通用直连、代理和 GeoIP 规则之前增加：

```json
{
  "domain_suffix": [
    "taila6b1f7.ts.net"
  ],
  "action": "route",
  "outbound": "ts-ep"
},
{
  "ip_cidr": [
    "100.64.0.0/10",
    "fd7a:115c:a1e0::/48"
  ],
  "action": "route",
  "outbound": "ts-ep"
},
{
  "preferred_by": [
    "ts-ep"
  ],
  "action": "route",
  "outbound": "ts-ep"
}
```

三条规则分别覆盖：

- 当前 Tailnet 的 MagicDNS 域名；
- Tailscale IPv4 CGNAT 地址段和 Tailscale ULA IPv6 地址段；
- endpoint 从控制平面获得的 preferred routes，例如 peer allowed IP 和通告子网。

`preferred_by` 的值必须是 endpoint tag，即 `ts-ep`，并且必须使用数组。写成 `"tailscale"` 会被解释为引用一个名为 `tailscale` 的 outbound，启动时报 `outbound not found: tailscale`。

## 认证与运行状态

内置 endpoint 首次启动时注册为：

```text
macbook-deeloo-cn-1-sing-box
```

认证状态保存在：

```text
/Users/ldd/proj/rust/sing-box-tui/.local/tailscale-embedded
```

正式切换后，独立 Tailscale 客户端已停止：

```bash
tailscale down
```

不要同时删除 `.local/tailscale-embedded`；删除它会使内置 endpoint 丢失身份并要求重新认证。

## 验证结果

使用当前配置和已认证 state 完成了以下验证：

```bash
/Library/PrivilegedHelperTools/com.winterloo.sing-box-tui.sing-box check \
  -c /Users/ldd/proj/rust/sing-box-tui/config.json
```

- 内置 Tailnet IP `100.121.67.118:22` 返回 `SSH-2.0-OpenSSH_10.4`。
- MagicDNS `archlinux.taila6b1f7.ts.net:22` 返回相同 SSH banner。
- 无代理环境变量访问 `https://chatgpt.com/backend-api/models` 返回 HTTP 403，证明 DNS、TCP 和 TLS 已连通。
- 无代理环境变量访问 `https://api.openai.com/` 返回 HTTP 421，证明 OpenAI HTTPS 链路已连通。
- 系统解析 `chatgpt.com` 恢复为 Cloudflare 地址 `104.18.32.47` 和 `172.64.155.209`。
- `pi` 不再报告 WebSocket/`fetch failed`，而是收到明确的认证失效响应；后者需要在 `pi` 中重新登录，与网络配置无关。

## 回滚

切换前配置保存在仓库根目录：

```text
config.pre-embedded-tailscale.json
```

回滚命令：

```bash
cd /Users/ldd/proj/rust/sing-box-tui
cp config.pre-embedded-tailscale.json config.json
printf '%s\n' \
  '{"action":"restart","config":"/Users/ldd/proj/rust/sing-box-tui/config.json"}' \
  | nc -U /var/run/sing-box-tui-helper.sock
tailscale up
```

回滚后应重新执行公网 HTTPS、Tailnet IP 和 MagicDNS 验证。

## 维护注意事项

- `config.json` 是被 `.gitignore` 排除的运行时文件，本说明文档不会使该配置自动应用到其他机器。
- 订阅导入或完整配置重建可能覆盖顶层 `endpoints` 以及新增的 DNS/route 规则。更新订阅后应检查这些片段是否仍然存在，并重新运行 `sing-box check`。
- 当前 Tailnet 后缀 `taila6b1f7.ts.net` 是显式配置。如果 Tailnet DNS 后缀变化，需要同步更新 DNS 与 route 两处 `domain_suffix`。
- `.local/tailscale-embedded` 和 `config.pre-embedded-tailscale.json` 都可能包含敏感信息或节点配置，不应提交到 Git。
- 当前方案基于 sing-box `1.13.13-winterloo.2` 且二进制包含 `with_tailscale` build tag；升级后应重新核对官方 endpoint 和 DNS schema。
