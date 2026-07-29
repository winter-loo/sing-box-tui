# Private Access Services

`sing-box-tui` can run enterprise private-access services as external processes
and keep sing-box as the single user-facing traffic entry point.

The first service is Hillstone Secure Connect. It is service-specific and
should not be treated as a generic SSL VPN implementation.

## TUI Usage

Start the TUI:

```bash
sing-box-tui run --config ./config.json
```

Open settings with `o` and configure:

- `Private Access service`
- `Private Access manifest path`
- `Private Access server`
- `Private Access port`
- `Private Access username`
- `Private Access password`
- `Private Access password env`
- `Private Access mode`
- `Private Access bridge listen`
- `Private Access TLS verify`

Press `V` to connect or disconnect Private Access.

`Private Access service` is only the focused profile for editing and for the
next connect/disconnect action. It is not a routing selector. Multiple profiles
can be connected at the same time, and sing-box route rules decide which
service bridge receives a given intranet destination.

`Private Access password` can also be stored directly in `sing-box-tui.json` for
a simpler local workflow. The settings list displays the plaintext value, and the
file also contains plaintext, so use this only on a trusted machine.

If `Private Access password` is empty, the service falls back to the configured
environment variable. For example:

```bash
export HILLSTONE_PASSWORD='...'
sing-box-tui run --config ./config.json
```

SonicWall profiles use the same precedence. When the gateway marks challenge
fields as `is-username` and `is-password`, the TUI prefills those two fields from
the profile. Generic password fields remain empty because they may contain a
dynamic password or one-time code. Interactive replies are never copied back to
`sing-box-tui.json`. Password and dynamic-code inputs are displayed as entered in
the authentication dialog, so the terminal should be treated as sensitive.

### SonicWall on macOS

The built-in SonicWall SMA 1000 / Connect Tunnel-compatible service uses TUN mode
on macOS. Add a profile whose `id` is `sonicwall`; that id selects the built-in
clean-room SonicWall service when no custom manifest is supplied:

```json
{
  "id": "sonicwall",
  "manifest_path": null,
  "mode": "tun",
  "server": "sslvpn.example.com",
  "port": 443,
  "username": "user",
  "password": "",
  "password_env": "",
  "bridge_listen": "",
  "tun_helper": [],
  "use_internet_proxy": false,
  "tls_verify": true
}
```

`use_internet_proxy` explicitly selects the SonicWall HTTPS transport. Its default
value is `false`, which uses a direct connection. Set it to `true` to use the
currently selected Internet proxy. The selected mode is strict: connection failures
do not trigger an automatic switch to the other transport.

Before connecting from the TUI, authorize the privileged helper in a terminal:

```bash
sudo -v
```

The normal TUI and SonicWall service remain unprivileged. The default helper
command uses `sudo -n` to create the macOS `utun` interface and install pushed
routes, so it fails with a clear message instead of prompting inside the service
protocol. Sites that deploy the binary centrally can configure `tun_helper` to
invoke a pre-authorized helper instead.

The authorized helper process is retained for the whole SonicWall login session.
If EVPN reconnects, the service resets and reconfigures the existing helper instead
of invoking `sudo` again. A changed gateway assignment therefore receives a fresh
TUN address, route, and DNS configuration without requiring another password.

On macOS the client retains the gateway-compatible Windows Connect Tunnel
protocol identity while evaluating supported OS-version and process
endpoint-policy checks against the local Mac. The compatibility identity is
required by gateways that reject an unverified Macintosh agent payload.
Windows-only registry checks remain unsupported and fail closed.

The privileged helper also installs temporary split-DNS files below
`/etc/resolver` for gateway-pushed domains. A more-specific resolver keeps the
public SonicWall gateway hostname on the Mac's primary physical DNS servers.
Resolver files carry the helper PID and are removed on disconnect only if their
contents are still owned by that helper.

The generated sing-box configuration routes public VPN gateway domains through
the existing Internet selector. Node measurement and selection remain the sole
responsibility of the normal Internet auto picker; the SonicWall service neither
benchmarks candidates nor changes the selected proxy. If that shared selection
stabilizes on a different path, the service re-establishes only its EVPN carrier
and reuses the authenticated session and authorized TUN helper.

## Traffic Flow

Private Access does not require a second browser proxy. In `bridge` mode, the
intended path is:

```text
browser or app
  -> OS system proxy or sing-box TUN
  -> sing-box route engine
  -> profile-owned intranet CIDR rule
  -> local private-access bridge
  -> service tunnel
  -> intranet service
```

When a service pushes route CIDRs, the TUI writes profile-owned sing-box route
rules without port restrictions. If `office` pushes `10.1.0.0/16` and `lab`
pushes `10.2.0.0/16`, traffic to `10.1.x.x` goes to the `office` bridge and
traffic to `10.2.x.x` goes to the `lab` bridge.

In `tun` mode, the service starts a small privileged helper and exchanges raw
IPv4 packets with it:

```text
browser, git, curl, ssh, or app
  -> OS pushed route
  -> helper-owned TUN interface
  -> service process
  -> service tunnel
  -> intranet service
```

The helper is intentionally generic: it owns TUN creation and OS route cleanup,
while the service owns protocol-specific authentication, encryption, and
gateway packet transport.

On Windows, the helper also installs temporary NRPT rules for gateway-pushed
hostnames and domain suffixes. Those namespaces use the tunnel DNS servers,
while unrelated names keep using the normal system DNS path. DNS server host
routes and NRPT rules are removed with the TUN session. If a pushed suffix also
contains the public SonicWall gateway hostname, an exact, higher-priority NRPT
override keeps that gateway on DNS servers from the physical default-route
interface while the remaining suffix continues to support dynamic intranet
names. The shared sing-box TUN baseline is written before sing-box starts, so a
successful Hillstone or
SonicWall TUN connection does not edit `config.json` or restart sing-box.
If abnormal termination leaves Windows network state behind, follow
[`windows-private-access-emergency-cleanup.md`](windows-private-access-emergency-cleanup.md)
and remove only entries that are positively identified as belonging to this program.

## Service Process

The Hillstone service can be launched directly for protocol smoke tests:

```bash
printf '%s\n' '{"type":"status","id":"smoke","service":"hillstone"}' \
  | pas-hillstone
```

The service uses newline-delimited JSON over stdio. Human diagnostic logs are
written to stderr so stdout stays parseable by the TUI.

When Hillstone runs through the TUI, its connection lifecycle and runtime
diagnostics are also persisted to `hillstone-private-access.log` in the working
directory. The log includes connection stages, network setup, periodic
bridge/TUN activity counters, keepalives, error chains, and the final session
exit reason. Passwords, password environment-variable values, session IDs, and
key material are not written to the file.

Each Private Access profile can point at a service manifest. If `manifest_path` is
empty or `null`, the profile uses the built-in Hillstone service. This allows
multiple Hillstone accounts or gateways without duplicating manifest files.

```json
{
  "private_access_profiles": [
    {
      "id": "office",
      "manifest_path": null,
      "mode": "bridge",
      "server": "sslvpn.example.com",
      "port": 4433,
      "username": "user",
      "password": "optional-plaintext-password",
      "password_env": "HILLSTONE_PASSWORD",
      "bridge_listen": "127.0.0.1:16780",
      "tls_verify": false
    },
    {
      "id": "office-tun",
      "manifest_path": null,
      "mode": "tun",
      "server": "sslvpn.example.com",
      "port": 4433,
      "username": "user",
      "password_env": "HILLSTONE_PASSWORD",
      "tun_helper": [
        "sudo",
        "-n",
        "/path/to/sing-box-tui",
        "private-access-tun-helper",
        "--stdio"
      ],
      "tls_verify": false
    },
    {
      "id": "custom-service",
      "manifest_path": "./services/custom-private-access.json",
      "mode": "bridge",
      "server": "vpn.example.com",
      "port": 443,
      "username": "user",
      "password_env": "CUSTOM_PRIVATE_ACCESS_PASSWORD",
      "bridge_listen": "127.0.0.1:18081",
      "tls_verify": true
    }
  ]
}
```

Profile order has no routing meaning. The first item is only the initial TUI
focus when the app starts. `private_access_profiles` is the only supported
Private Access configuration surface.

For temporary testing, the TUI can also load one service manifest from:

```bash
export SING_BOX_TUI_PRIVATE_ACCESS_MANIFEST=/path/to/service.json
```

If no manifest is configured, the built-in Hillstone manifest starts the current
`sing-box-tui` executable with the internal `private-access-service hillstone
--stdio` command.

In `mode=tun`, the built-in service starts the current executable with the
hidden `private-access-tun-helper --stdio` command. When the service is not
already running as root, the default command is wrapped as `sudo -n ...` so the
TUI never blocks on a password prompt. A product installer can replace this with
a LaunchDaemon or other pre-authorized helper by setting `tun_helper`.

Private Access profile `id` is the user-facing connection slot and route ownership
key. The manifest `id` is the protocol implementation id used inside service
commands. Route rules are owned by the profile id, so two profiles that both use
Hillstone can keep their route rules separate.

## Troubleshooting

- `auth_failed` or `AUTH returned status ...`: check username, direct password
  or password env, and whether the gateway allows another session.
- `502 Bad Gateway`: the local bridge may be disconnected or stale routes may
  still point sing-box at a closed bridge.
- Intranet IP does not route through the bridge: confirm pushed CIDRs were
  applied to `config.json` and reload or restart sing-box.
- `TUN helper failed before ready`: confirm the helper command has privilege to
  create a TUN interface and add OS routes. With the default `sudo -n` command,
  sudo credentials must already be cached or configured for non-interactive use.
- TLS verification disabled: acceptable for exploratory testing only; enable it
  when the gateway certificate chain can be verified.
