# Sing-box TUN Mode

This note documents the process for enabling TUN mode in a sing-box config.

## Toggling TUN from the TUI

When the TUI manages sing-box (`sing-box-tui run`), press `\` to toggle the
Internet Proxy TUN mode without editing the config by hand:

- It adds or removes a `tun` inbound (`tag: "tun-in"` with `auto_route`,
  `strict_route`, and `stack: mixed`) in the configured `config.json`.
- While TUN is enabled, it sets `route.auto_detect_interface` to `true` so
  sing-box's own node and DNS connections leave through the physical default
  interface instead of looping back into the TUN. Disabling restores the exact
  value (including a previously absent field) that existed before enabling.
- It owns only the `tun-in` inbound. A differently tagged custom TUN is left
  untouched, and enabling is rejected rather than creating a conflicting
  second TUN inbound. The `tun-in` tag is reserved globally; enabling is also
  rejected if a non-TUN inbound already uses that tag, because sing-box inbound
  tags must be unique.
- It restarts the managed sing-box process so the change takes effect.
- TUN requires elevated permissions. On macOS, the installer installs a root-owned
  LaunchDaemon helper and the ordinary-user TUI asks that helper to restart or stop
  sing-box over an authenticated Unix socket. Administrator authorization is needed
  once during installation, not for every toggle. A source build without the helper
  falls back to `sudo -v` followed by `sudo -n`. Linux uses the sudo path and Windows
  requires an Administrator session.

### macOS privileged helper

The installer places a root-owned copy of the executable at
`/Library/PrivilegedHelperTools/com.winterloo.sing-box-tui.helper` and installs
`/Library/LaunchDaemons/com.winterloo.sing-box-tui.helper.plist`. The daemon accepts
connections only from the UID that installed it. Its protocol exposes only restart,
stop, and status operations; it does not accept shell commands. Executable and config
paths are canonicalized and checked for safe ownership and permissions before use.
The helper runs only the root-owned sing-box copy installed alongside it; clients cannot
select an executable or log path.

To deliberately keep the legacy sudo behavior, install with `--no-macos-helper`.
To remove an installed helper:

```sh
sudo launchctl bootout system/com.winterloo.sing-box-tui.helper
sudo rm /Library/LaunchDaemons/com.winterloo.sing-box-tui.helper.plist
sudo rm /Library/PrivilegedHelperTools/com.winterloo.sing-box-tui.helper
sudo rm /Library/PrivilegedHelperTools/com.winterloo.sing-box-tui.sing-box
sudo rm -f /var/run/sing-box-tui-helper.sock /var/run/sing-box-tui-helper.pid
sudo rm -f /var/log/sing-box-tui-helper.log /var/log/sing-box-tui-managed.log
```

The first run after upgrading may request sudo once to retire a root sing-box process
started by an older TUI. Subsequent TUN toggles use the daemon without sudo prompts.

The TUI status bar shows whether TUN is currently enabled (`\` key). This is a
different mechanism from the Private Access TUN data-plane helper, which is a
separate root-run helper process for enterprise intranet access.

The sections below cover the manual, config-file route.

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

Newly generated default configs omit TUN by default. When generating a config
with `import`, `subscribe`, `subscriptions`, `sync`, or the `run` background
subscription refresh, pass `--include-tun-mode` to include the TUN inbound
shown below.

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

Keep the rest of the config, including `dns`, `outbounds`, route rules, and
`experimental`, unchanged unless you are intentionally changing routing behavior.
When `auto_route` is enabled, the top-level route object must also select a
physical egress interface. The portable choice used by this project is:

```json
{
  "route": {
    "auto_detect_interface": true
  }
}
```

Without this setting (or the advanced alternatives `route.default_interface`
and per-outbound `bind_interface`), the connection from sing-box to every proxy
node can be captured by its own TUN route, making all nodes appear unavailable.

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
