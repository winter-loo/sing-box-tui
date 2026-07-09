# RFC: Private Access TUN Data Plane

Status: Draft
Date: 2026-07-06

## Summary

The current Private Access implementation uses a local HTTP bridge. It is useful
for browser traffic that enters sing-box through the normal mixed inbound, but
it cannot cover arbitrary command-line traffic such as `git pull` against an
intranet HTTP Git server.

This RFC proposes a profile-owned TUN data plane for Private Access services.
The first target is the existing Hillstone service. The goal is to make Remote
Access behave like the official macOS client: the service receives gateway
pushed routes, creates a local tunnel interface, installs OS routes for those
CIDRs, and forwards IP packets over the service protocol.

## Problem

The bridge data plane works only when traffic can be rewritten to a local HTTP
proxy endpoint:

```text
browser/app -> sing-box mixed inbound -> route override -> 127.0.0.1:16780
```

This has important limits:

- CLI tools that connect directly to `10.x.x.x` do not use the bridge unless
  they are explicitly configured to use the sing-box proxy.
- Non-HTTP protocols cannot be represented by the HTTP bridge.
- Requests with only `Host: 127.0.0.1:16780` do not contain the original
  intranet target, so the bridge cannot infer where to forward them.
- The Private Access service cannot act like a real intranet interface.

We need a packet data plane:

```text
app -> OS route -> utun -> Private Access service -> gateway UDP/ESP -> intranet
```

## Goals

- Keep the existing bridge mode working.
- Add a service data-plane mode: `bridge` or `tun`.
- Let each Private Access profile select its data plane independently.
- Make the TUI status show which mode is active.
- Use gateway-pushed CIDRs as the source of truth for intranet routes.
- Support arbitrary TCP/UDP/ICMP traffic once TUN mode is complete.
- Disconnect cleanly and remove OS routes/interface state.
- Keep protocol-specific packet handling inside the service process.

## Non-Goals

- Do not force all users onto TUN immediately.
- Do not remove the HTTP bridge until TUN is stable.
- Do not merge service protocol internals into the TUI.
- Do not claim Hillstone is a generic SSL VPN protocol.
- Do not make sing-box implement Hillstone ESP directly.

## Configuration

Each Private Access profile gains a `mode` field:

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
    }
  ]
}
```

`bridge` is the default because it is the only fully implemented data plane
today. `tun` is the target mode for full intranet access.

Service manifests should advertise the accepted values:

```json
{
  "config_schema": {
    "mode": {
      "type": "string",
      "enum": ["bridge", "tun"],
      "default": "bridge"
    }
  }
}
```

TUN mode can also accept an optional helper command:

```json
{
  "mode": "tun",
  "tun_helper": [
    "sudo",
    "-n",
    "/path/to/sing-box-tui",
    "private-access-tun-helper",
    "--stdio"
  ]
}
```

If omitted, the built-in service uses the current executable and wraps it with
`sudo -n` when the service process is not already privileged. The nonblocking
sudo mode is intentional: the TUI should report a privilege/setup error instead
of hanging inside a password prompt.

## Service Responsibilities

In `bridge` mode, the service:

1. Authenticates to the gateway.
2. Receives routes, DNS, client IP, gateway IP, and session keys.
3. Starts the local HTTP bridge.
4. Emits pushed routes and bridge details to the TUI.
5. The TUI writes sing-box route override rules to `config.json`.

In `tun` mode, the service should:

1. Authenticate and negotiate the same network setup.
2. Create a macOS `utun` interface.
3. Assign the pushed client IPv4 address and netmask.
4. Install OS routes for the pushed intranet CIDRs through the utun interface.
5. Forward packets read from utun into the service data channel.
6. Write inbound packets from the service data channel back to utun.
7. Remove routes and close utun on disconnect.

The TUI should not parse or transform raw packets. It should only orchestrate
service lifecycle and display state.

## macOS TUN Design

The service starts a privileged TUN helper instead of opening utun directly.
The helper uses `tun-rs` to create/configure the interface and exchanges plain
IPv4 packets with the service over newline-delimited JSON. Packet payloads are
base64 encoded so the stdio transport remains line oriented.

The helper is deliberately protocol-neutral:

```text
service -> helper: start(client IP, gateway IP, prefix, pushed routes)
helper   -> service: ready(interface)
helper   -> service: packet(base64 IPv4)
service -> helper: packet(base64 IPv4)
service -> helper: stop
```

The service still owns Hillstone authentication, session keys, ESP
encapsulation, and gateway UDP transport. The helper owns only privileged local
network state: TUN creation, pushed OS routes, route cleanup, and TUN packet
read/write.

Privilege model:

- The TUI and service process can remain unprivileged.
- The helper must have permission to create/configure TUN and add routes.
- The default helper launch uses `sudo -n` when needed, which fails clearly if
  sudo is not already authorized.
- A product installer can replace `sudo -n` with a LaunchDaemon, privileged
  helper tool, or Network Extension wrapper later.

## Hillstone Packet Loop

The existing Hillstone code already negotiates the pieces required for a packet
data plane:

- client private IPv4 address and netmask
- gateway private IPv4 address
- UDP ESP gateway endpoint
- outbound SPI
- session id
- encryption/authentication algorithms
- pushed route table

The TUN packet loop should be added below this negotiation point:

```text
utun read IPv4 packet
  -> encapsulate using Hillstone ESP/session state
  -> send UDP packet to gateway

UDP packet from gateway
  -> validate/decrypt/decapsulate
  -> write IPv4 packet to utun
```

The bridge implementation should remain available while this packet loop is
being hardened.

## TUI Behavior

The TUI should expose these settings:

- Private Access service profile
- Private Access data plane mode: `bridge` or `tun`
- Server, port, username, password/password env, TLS verification
- Bridge listen address only matters in `bridge` mode

The summary line should include the active mode:

```text
private access: [>office CONNECTED] mode=bridge routes=3 bridge=127.0.0.1:16780
private access: [>office CONNECTED] mode=tun routes=3
```

When `tun` is selected before the implementation is complete, the service must
fail explicitly instead of silently falling back to bridge mode.

## Implementation Plan

1. Add `mode` to profile state, TUI settings, service command config, examples,
   and the built-in Hillstone manifest.
2. Add a service-side mode branch. `bridge` keeps the current behavior. `tun`
   initially returns a clear `not_implemented` error.
3. Introduce a small privileged TUN helper behind a service-only module.
4. Move Hillstone post-`NEW_KEY` session material into a reusable runtime state
   so both bridge and tun can use it.
5. Implement outbound utun packet read and UDP/ESP encapsulation.
6. Implement inbound UDP/ESP decapsulation and utun write.
7. Add route installation and cleanup for pushed CIDRs.
8. Add TUI tests for mode persistence, settings validation, and status text.
9. Add integration smoke tests gated behind macOS/privilege availability.

## Current Landing

The current implementation has completed the first usable TUN data-plane
landing:

- `mode=bridge` keeps the existing HTTP bridge path.
- `mode=tun` runs the normal Hillstone control plane through AUTH, SET_ROUTE,
  KEY_DONE, and NEW_KEY.
- After NEW_KEY, the service validates ESP session material and starts the
  hidden `private-access-tun-helper --stdio` helper.
- The helper uses `tun-rs` to create/configure the TUN interface and install
  gateway-pushed intranet routes with a guard that removes them on exit.
- The service and helper exchange plain IPv4 packets over JSON-lines stdio.
- The service runs a nonblocking loop that reads helper-emitted IPv4 packets,
  encapsulates them as Hillstone ESP over UDP, receives UDP ESP packets,
  decapsulates them, and sends IPv4 packets back to the helper.

This placement matters: starting the helper before Hillstone authentication
would test the wrong thing and could create local system state even when the
private-access gateway rejects the login. The current boundary proves both sides
are ready before packet forwarding starts. The route guard also matters because
any failed TUN experiment should not leave stale OS routes that blackhole
intranet traffic.

## Open Questions

- Whether DNS should be installed at OS scope in TUN mode or remain a displayed
  service detail first.
- Whether route installation should move from shelling out to a structured
  route-management crate on macOS.
- How to coordinate when sing-box itself is already running in TUN mode.
- Whether multiple simultaneous TUN private-access profiles should be allowed or
  blocked until route conflict handling exists.
