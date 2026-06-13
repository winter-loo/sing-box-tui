# macOS System Proxy To Sing-box

This note documents how the macOS system proxy was changed from the AirTCP client port to the sing-box mixed inbound port.

## Context

The sing-box service was already running with a mixed inbound on port `6780` and a Clash API controller on port `9992`.

The live config showed the mixed inbound:

```bash
jq '.inbounds[] | select(.type == "mixed")' /usr/local/etc/sing-box/config.json
```

Expected shape:

```json
{
  "listen": "::",
  "listen_port": 6780,
  "tag": "mixed-in",
  "type": "mixed"
}
```

The listener was also visible at the socket level:

```bash
netstat -anv | rg '\\.6780\\s'
```

That showed `*.6780` in `LISTEN`, owned by the running sing-box process.

## Check The Current macOS Proxy

First inspect the effective proxy settings:

```bash
scutil --proxy
```

Before the change, macOS was still pointing to the AirTCP client:

```text
HTTPEnable : 1
HTTPProxy : 127.0.0.1
HTTPPort : 5780
HTTPSEnable : 1
HTTPSProxy : 127.0.0.1
HTTPSPort : 5780
SOCKSEnable : 1
SOCKSProxy : 127.0.0.1
SOCKSPort : 5780
```

Then list the network services:

```bash
networksetup -listallnetworkservices
```

The active service was confirmed with:

```bash
scutil --nwi
```

The active interface was `en0`, which corresponds to the `Wi-Fi` network service.

For that service, each proxy setting also showed AirTCP's port:

```bash
networksetup -getwebproxy Wi-Fi
networksetup -getsecurewebproxy Wi-Fi
networksetup -getsocksfirewallproxy Wi-Fi
```

Each one returned:

```text
Enabled: Yes
Server: 127.0.0.1
Port: 5780
Authenticated Proxy Enabled: 0
```

## Change Wi-Fi To Sing-box Port 6780

The macOS system proxy was switched to sing-box with these commands:

```bash
networksetup -setwebproxy Wi-Fi 127.0.0.1 6780
networksetup -setsecurewebproxy Wi-Fi 127.0.0.1 6780
networksetup -setsocksfirewallproxy Wi-Fi 127.0.0.1 6780
```

Meaning:

- `-setwebproxy`: sets the HTTP system proxy.
- `-setsecurewebproxy`: sets the HTTPS system proxy.
- `-setsocksfirewallproxy`: sets the SOCKS system proxy.
- `Wi-Fi`: the macOS network service being changed.
- `127.0.0.1 6780`: the sing-box local mixed inbound.

If the service name contains spaces, quote it:

```bash
networksetup -setwebproxy "USB 10/100 LAN" 127.0.0.1 6780
```

## Verify The Change

After the change, `scutil --proxy` showed:

```text
HTTPEnable : 1
HTTPProxy : 127.0.0.1
HTTPPort : 6780
HTTPSEnable : 1
HTTPSProxy : 127.0.0.1
HTTPSPort : 6780
SOCKSEnable : 1
SOCKSProxy : 127.0.0.1
SOCKSPort : 6780
```

Then verify Google through sing-box's local mixed proxy:

```bash
curl -sS -I --max-time 12 -x http://127.0.0.1:6780 https://www.google.com
```

The request returned `HTTP/2 200`.

The lightweight Google connectivity check also worked:

```bash
curl -sS --max-time 12 \
  -x http://127.0.0.1:6780 \
  https://www.google.com/generate_204 \
  -o /dev/null \
  -w 'code=%{http_code} connect=%{time_connect} appconnect=%{time_appconnect} total=%{time_total}\n'
```

Expected result:

```text
code=204
```

## Why This Fixed Browser Traffic

When AirTCP's official client was controlling the macOS system proxy, apps such as Chrome were pointed at:

```text
127.0.0.1:5780
```

That is AirTCP's local client port.

After switching macOS to:

```text
127.0.0.1:6780
```

system-proxy-aware apps send their HTTP, HTTPS, and SOCKS traffic to sing-box instead. That lets sing-box route the traffic through the selected provider group and node.

This is different from relying only on TUN mode. With a system proxy, the browser passes the destination hostname to sing-box, so sing-box can resolve and route it itself.

## Rollback Or Disable

To point macOS back to AirTCP:

```bash
networksetup -setwebproxy Wi-Fi 127.0.0.1 5780
networksetup -setsecurewebproxy Wi-Fi 127.0.0.1 5780
networksetup -setsocksfirewallproxy Wi-Fi 127.0.0.1 5780
```

To disable the system proxy for Wi-Fi:

```bash
networksetup -setwebproxystate Wi-Fi off
networksetup -setsecurewebproxystate Wi-Fi off
networksetup -setsocksfirewallproxystate Wi-Fi off
```

Then verify again:

```bash
scutil --proxy
```
