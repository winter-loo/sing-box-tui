# Remote Access Providers

`sing-box-tui` can run enterprise remote-access providers as external processes
and keep sing-box as the single user-facing traffic entry point.

The first provider is Hillstone Secure Connect. It is provider-specific and
should not be treated as a generic SSL VPN implementation.

## TUI Usage

Start the TUI:

```bash
sing-box-tui run --config ./config.json
```

Open settings with `o` and configure:

- `Remote access provider`
- `Remote access manifest path`
- `Remote access server`
- `Remote access port`
- `Remote access username`
- `Remote access password`
- `Remote access password env`
- `Remote access bridge listen`
- `Remote access TLS verify`

Press `V` to connect or disconnect Remote Access.

`Remote access provider` is only the focused profile for editing and for the
next connect/disconnect action. It is not a routing selector. Multiple profiles
can be connected at the same time, and sing-box route rules decide which
provider bridge receives a given intranet destination.

`Remote access password` can also be stored directly in `sing-box-tui.json` for
a simpler local workflow. The settings list masks the value as `<set>`, but the
file still contains plaintext, so use this only on a trusted machine.

If `Remote access password` is empty, the provider falls back to the configured
environment variable. For example:

```bash
export HILLSTONE_PASSWORD='...'
sing-box-tui run --config ./config.json
```

## Traffic Flow

Remote Access does not require a second browser proxy. The intended path is:

```text
browser or app
  -> OS system proxy or sing-box TUN
  -> sing-box route engine
  -> provider-owned intranet CIDR rule
  -> local remote-access bridge
  -> provider tunnel
  -> intranet service
```

When a provider pushes route CIDRs, the TUI writes provider-owned sing-box route
rules without port restrictions. If `office` pushes `10.1.0.0/16` and `lab`
pushes `10.2.0.0/16`, traffic to `10.1.x.x` goes to the `office` bridge and
traffic to `10.2.x.x` goes to the `lab` bridge.

## Provider Process

The Hillstone provider can be launched directly for protocol smoke tests:

```bash
printf '%s\n' '{"type":"status","id":"smoke","provider":"hillstone"}' \
  | rap-hillstone
```

The provider uses newline-delimited JSON over stdio. Human diagnostic logs are
written to stderr so stdout stays parseable by the TUI.

Each provider profile can point at a provider manifest. If `manifest_path` is
empty or `null`, the profile uses the built-in Hillstone provider. This allows
multiple Hillstone accounts or gateways without duplicating manifest files.

```json
{
  "remote_access_providers": [
    {
      "id": "office",
      "manifest_path": null,
      "server": "sslvpn.example.com",
      "port": 4433,
      "username": "user",
      "password": "optional-plaintext-password",
      "password_env": "HILLSTONE_PASSWORD",
      "bridge_listen": "127.0.0.1:16780",
      "tls_verify": false
    },
    {
      "id": "custom-provider",
      "manifest_path": "./providers/custom-remote-access.json",
      "server": "vpn.example.com",
      "port": 443,
      "username": "user",
      "password_env": "CUSTOM_REMOTE_ACCESS_PASSWORD",
      "bridge_listen": "127.0.0.1:18081",
      "tls_verify": true
    }
  ]
}
```

Profile order has no routing meaning. The first item is only the initial TUI
focus when the app starts. `remote_access_providers` is the only supported
Remote Access configuration surface.

For temporary testing, the TUI can also load one provider manifest from:

```bash
export SING_BOX_TUI_REMOTE_ACCESS_MANIFEST=/path/to/provider.json
```

If no manifest is configured, the built-in Hillstone manifest starts the current
`sing-box-tui` executable with the internal `remote-access-provider hillstone
--stdio` command.

Provider profile `id` is the user-facing connection slot and route ownership
key. The manifest `id` is the protocol implementation id used inside provider
commands. Route rules are owned by the profile id, so two profiles that both use
Hillstone can keep their route rules separate.

## Troubleshooting

- `auth_failed` or `AUTH returned status ...`: check username, direct password
  or password env, and whether the gateway allows another session.
- `502 Bad Gateway`: the local bridge may be disconnected or stale routes may
  still point sing-box at a closed bridge.
- Intranet IP does not route through the bridge: confirm pushed CIDRs were
  applied to `config.json` and reload or restart sing-box.
- TLS verification disabled: acceptable for exploratory testing only; enable it
  when the gateway certificate chain can be verified.
