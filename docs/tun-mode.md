# Sing-box TUN Mode

This note documents the process for enabling TUN mode in a sing-box config.

## What TUN Mode Changes

The current `./config.json` may only expose a local proxy listener such as:

```json
{
  "listen": "::",
  "listen_port": 6780,
  "set_system_proxy": false,
  "type": "mixed"
}
```

That is not TUN mode. It only accepts traffic from apps or system proxy settings
that explicitly point to the mixed proxy port.

TUN mode adds a virtual network interface and routes system traffic into
sing-box automatically. It requires elevated permissions because sing-box must
create an interface and modify routes.

## 1. Validate The Current Config

Before editing, check whether the config is valid:

```powershell
sing-box check -c .\config.json
```

If this fails, fix or regenerate `config.json` before adding TUN mode. sing-box
will not start from an invalid JSON config.

## 2. Add A TUN Inbound

Add a second inbound object under the top-level `inbounds` array.

Example:

```json
{
  "type": "tun",
  "tag": "tun-in",
  "address": [
    "172.19.0.1/30",
    "2001:470:f9da:fdfa::1/64"
  ],
  "mtu": 9000,
  "auto_route": true,
  "strict_route": true,
  "stack": "mixed",
  "endpoint_independent_nat": true
}
```

If the existing config has only one `mixed` inbound, the result should look like:

```json
{
  "inbounds": [
    {
      "listen": "::",
      "listen_port": 6780,
      "set_system_proxy": false,
      "type": "mixed"
    },
    {
      "type": "tun",
      "tag": "tun-in",
      "address": [
        "172.19.0.1/30",
        "2001:470:f9da:fdfa::1/64"
      ],
      "mtu": 9000,
      "auto_route": true,
      "strict_route": true,
      "stack": "mixed",
      "endpoint_independent_nat": true
    }
  ]
}
```

Keep the rest of the config, including `dns`, `outbounds`, `route`, and
`experimental`, unchanged unless you are intentionally changing routing behavior.

## 3. Linux Optional Setting

On Linux, a manually enabled TUN inbound can also use:

```json
"auto_redirect": true
```

Use it only on Linux. Do not add it for Windows or macOS unless your sing-box
version and platform support it.

## 4. Validate Again

After editing:

```powershell
sing-box check -c .\config.json
```

Do not restart sing-box until this passes.

## 5. Restart Sing-box

If running manually:

```powershell
sing-box run -c .\config.json
```

On Windows, start PowerShell or the service as Administrator.

If running as a Windows service, restart the service after writing the config:

```powershell
Restart-Service sing-box
```

If running on Linux with systemd:

```bash
sudo systemctl restart sing-box
```

If running on macOS through a service manager, restart the corresponding service
or stop the old process and start sing-box again with the updated config.

## 6. Confirm TUN Is Active

Check the sing-box log first. TUN startup failures usually mention permission,
route, interface, or address conflicts.

You can also inspect active connections through the Clash API or this TUI. TUN
connections usually show an inbound kind similar to:

```text
tun/tun-in
```

## Notes

- Changing `config.json` alone is not enough. sing-box must be restarted or
  reloaded before the TUN inbound becomes active.
- TUN mode and the existing mixed proxy inbound can coexist.
- TUN mode needs Administrator/root permissions.
- If network access breaks after enabling TUN mode, revert the TUN inbound,
  validate the config, and restart sing-box.
