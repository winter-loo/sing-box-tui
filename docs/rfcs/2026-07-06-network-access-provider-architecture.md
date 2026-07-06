# RFC: Network Access Provider Architecture

Status: Draft
Date: 2026-07-06

## Summary

This RFC defines how `sing-box-tui` should integrate enterprise remote-access
VPN capabilities, starting with the current Hillstone SSL VPN implementation,
without forcing users to configure a second browser proxy or run a separate
official client.

The core decision is:

- Keep `sing-box` as the unified traffic entry point.
- Treat enterprise intranet access as a separate `Remote Access` capability.
- Run each remote-access implementation as an external provider process.
- Let the TUI orchestrate provider lifecycle, route installation, status, and
  errors.
- Avoid presenting the Hillstone protocol as a generic SSL VPN standard.

## Background

The project already manages normal sing-box proxy workflows:

- subscription import and refresh
- selector switching
- benchmarking and auto-pick
- direct bypass rules
- system proxy toggling
- connection inspection through the Clash-compatible API

We then explored a Hillstone Secure Connect deployment and found enough protocol
behavior to build a local client path:

- TLS control channel to the gateway.
- Username/password authentication.
- Gateway-pushed client IP, gateway IP, DNS, UDP port, and IPv4 route table.
- Session key negotiation for a UDP ESP-like data channel.
- A local HTTP bridge that forwards intranet HTTP traffic over the data channel.
- Route application into `config.json` so sing-box can rewrite matching intranet
  destinations to the local bridge.
- Graceful local disconnect that sends a logout instead of relying on the
  official client or gateway timeout.

This worked for intranet targets such as `http://10.1.126.5:8099/` when the
browser used the normal sing-box mixed inbound and sing-box routed pushed
intranet ranges to the local bridge.

## Terminology

The word `VPN` is useful for users, but too broad for the architecture.

Use these terms:

- `Proxy`: sing-box proxy behavior, including mixed inbound, HTTP/SOCKS proxy,
  selector outbounds, subscription nodes, and system proxy integration.
- `TUN`: a sing-box traffic capture mode that may feel like a VPN to users, but
  is still a sing-box inbound mode.
- `Remote Access`: enterprise intranet access that obtains private routes, DNS,
  and a remote-access tunnel from a gateway.
- `Remote Access VPN`: user-facing wording for technologies such as Hillstone,
  AnyConnect, GlobalProtect, FortiGate SSL VPN, OpenVPN, or WireGuard when they
  are used to reach private networks.
- `Network Access`: the top-level product area that contains proxy, TUN, system
  proxy, bypass, and remote-access capabilities.

Avoid these terms:

- `intranet SVN provider`: incorrect terminology.
- `GenericSslVpnProvider`: misleading because SSL VPN is not one interoperable
  protocol.
- `VpnProvider` as the top-level abstraction: too broad and confusing because
  sing-box TUN can also be described as VPN-like.

Recommended code naming:

- Module area: `network_access`
- Remote-access trait: `RemoteAccessProvider`
- External process adapter: `ExternalRemoteAccessProvider`
- Current implementation id: `hillstone`
- UI label: `Remote Access` / `内网接入`

The existing project also uses `provider` for subscription node providers. New
remote-access code should use the full phrase `remote access provider` in docs
and identifiers where ambiguity matters.

## Goals

- Provide one user-facing entry point in the TUI.
- Let users keep using the same system proxy or sing-box TUN entry point.
- Do not require the browser to manually use a second proxy.
- Support provider-specific remote-access protocols without coupling them to
  the TUI.
- Allow future providers to be added, upgraded, disabled, or removed without
  rewriting the TUI.
- Do not require secrets in config files; if a user explicitly chooses direct
  password config, keep it marked sensitive and out of logs.
- Keep provider crashes isolated from the TUI process.
- Make route changes explicit, inspectable, and reversible.

## Non-Goals

- Do not claim Hillstone is a generic SSL VPN protocol.
- Do not replace sing-box's proxy core.
- Do not require sing-box to implement every remote-access protocol natively.
- Do not control or depend on the official Hillstone client.
- Do not add a second browser proxy setup path.
- Do not require plaintext passwords in project config.

## Current Hillstone Behavior

The current Hillstone implementation is provider-specific. It should be treated
as a reverse-engineered client for one remote-access protocol family, not as a
standard SSL VPN implementation.

Observed phases:

1. Connect to the gateway over TLS.
2. Send authentication request.
3. Send client information.
4. Receive network setup frames:
   - client private IPv4 address and netmask
   - gateway private IPv4 address
   - UDP data port
   - DNS and WINS values
   - pushed IPv4 route table
5. Negotiate session keys.
6. Run a UDP data path for intranet packets.
7. Optionally expose a local HTTP bridge.
8. Apply pushed routes to sing-box config.
9. On local shutdown, send logout and release local ports.

Current sing-box integration strategy:

1. Provider receives pushed intranet CIDRs.
2. TUI/config layer writes route rules into `config.json`.
3. The route rules match destination CIDRs only; they must not restrict ports.
4. Matched traffic is routed direct with `override_address` and `override_port`
   pointing to the local remote-access bridge.
5. Browser and user applications continue using the normal sing-box entry point.

This avoids asking users to configure a separate proxy such as
`127.0.0.1:16780` in the browser.

## Architecture

### Process Model

Each remote-access implementation runs as a child process:

```text
sing-box-tui
  ├─ manages TUI, state, config, sing-box route updates, and system proxy
  ├─ spawns provider process
  ├─ sends JSON commands over stdio
  └─ reads JSON events over stdio

rap-hillstone
  ├─ implements Hillstone protocol
  ├─ authenticates to the gateway
  ├─ receives pushed routes and DNS
  ├─ runs local bridge
  └─ disconnects cleanly when asked
```

The provider process owns protocol details. The TUI owns product integration.

### Provider Discovery

Providers are discovered from a configured directory, for example:

```text
providers/
  hillstone/
    provider.json
    rap-hillstone
```

Manifest example:

```json
{
  "id": "hillstone",
  "name": "Hillstone Secure Connect",
  "kind": "remote_access",
  "protocol": "hillstone-secure-connect",
  "executable": "./rap-hillstone",
  "version": "0.1.0",
  "capabilities": {
    "pushed_routes": true,
    "pushed_dns": true,
    "local_http_bridge": true,
    "graceful_disconnect": true
  },
  "config_schema": {
    "server": { "type": "string", "required": true },
    "port": { "type": "integer", "default": 4433 },
    "username": { "type": "string", "required": true },
    "password": { "type": "string", "required": false, "sensitive": true },
    "password_env": { "type": "string", "required": false },
    "bridge_listen": { "type": "string", "default": "127.0.0.1:16780" },
    "tls_verify": { "type": "boolean", "default": true }
  }
}
```

Runtime profile example:

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
      "password_env": "SING_BOX_TUI_REMOTE_ACCESS_PASSWORD",
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

Profile order has no routing meaning. The profile `id` is a user-facing
connection slot and route ownership key. The manifest `id` is the provider
protocol implementation id used in stdio commands. Multiple profiles can share
the same manifest.

### Provider Protocol

Use newline-delimited JSON over stdio for the first implementation. It is easy
to debug, language-neutral, and works for Rust and non-Rust providers.

Command envelope:

```json
{
  "id": "cmd-1",
  "type": "connect",
  "provider": "hillstone",
  "config": {
    "server": "sslvpn.example.com",
    "port": 4433,
    "username": "user",
    "password": "optional-plaintext-password",
    "password_env": "SING_BOX_TUI_REMOTE_ACCESS_PASSWORD",
    "bridge_listen": "127.0.0.1:16780",
    "tls_verify": false
  }
}
```

Event envelope:

```json
{
  "type": "event",
  "event": "routes_pushed",
  "provider": "hillstone",
  "session_id": "local-session-id",
  "routes": [
    { "cidr": "10.1.0.0/16" },
    { "cidr": "10.255.0.0/24" },
    { "cidr": "10.253.0.0/24" }
  ],
  "dns": ["10.1.252.10", "114.114.114.114"],
  "bridge": {
    "kind": "http",
    "listen": "127.0.0.1:16780"
  }
}
```

Status event:

```json
{
  "type": "event",
  "event": "state_changed",
  "provider": "hillstone",
  "state": "connected",
  "message": "connected, routes pushed"
}
```

Disconnect command:

```json
{
  "id": "cmd-2",
  "type": "disconnect",
  "provider": "hillstone",
  "session_id": "local-session-id"
}
```

Disconnect event:

```json
{
  "type": "event",
  "event": "state_changed",
  "provider": "hillstone",
  "state": "disconnected",
  "message": "logout sent"
}
```

Error event:

```json
{
  "type": "event",
  "event": "error",
  "provider": "hillstone",
  "code": "auth_failed",
  "message": "authentication failed"
}
```

### State Model

Provider state should be represented independently from protocol details:

```text
Disabled
Disconnected
Connecting
Connected
Disconnecting
Error
```

`Connected` should include:

- provider id
- gateway address
- assigned client address when available
- pushed routes
- pushed DNS when available
- local bridge address when available
- connected duration
- last error if reconnecting or degraded

### Route Ownership

Routes installed by a remote-access provider must be tagged as owned by that
provider.

Route metadata should include:

- provider id
- session id or generation id
- route CIDRs
- local bridge address
- insertion timestamp

For `config.json`, provider-owned route rules should be inserted before broad
private/direct rules and before generic proxy routing. They should not match a
specific port unless the provider explicitly reports a port-limited service.

Current Hillstone route shape:

```json
{
  "ip_cidr": ["10.1.0.0/16", "10.255.0.0/24", "10.253.0.0/24"],
  "action": "route",
  "outbound": "国内直连",
  "override_address": "127.0.0.1",
  "override_port": 16780
}
```

The important behavior is destination-based routing. A request to
`10.1.126.5:8099` and a request to `10.1.126.5:10011` must both match the same
provider-pushed `10.1.0.0/16` route.

On disconnect, the TUI should stop the provider process and mark the route set
inactive. Whether it removes the rules from `config.json` immediately or leaves
them for the next connection should be a product choice:

- `remove_on_disconnect = true`: cleaner config, but requires sing-box reload
  or restart.
- `remove_on_disconnect = false`: faster reconnect, but stale route rules can
  point at a closed bridge and produce `502 Bad Gateway`.

Initial recommendation: keep routes in config while the feature is experimental,
but make stale-bridge errors visible in the TUI and provide a `Clear routes`
action.

### TUI Integration

Add a `Remote Access` area to the existing TUI instead of creating a separate
client UI.

Recommended status line examples:

```text
Proxy: sing-box running  System Proxy: enabled  Remote Access: disconnected
Proxy: sing-box running  System Proxy: enabled  Remote Access: Hillstone connecting
Proxy: sing-box running  System Proxy: enabled  Remote Access: Hillstone connected routes=3 bridge=127.0.0.1:16780
Proxy: sing-box running  System Proxy: enabled  Remote Access: Hillstone error auth_failed
```

Recommended actions:

- Open Remote Access panel.
- Select configured remote-access provider.
- Switch between configured provider profiles.
- Connect.
- Disconnect.
- Show pushed routes.
- Apply routes to config.
- Clear provider-owned routes.
- Show bridge listen address.
- Show last provider logs without secrets.

Recommended settings:

- provider id
- gateway host and port
- username
- password source
- bridge listen address
- TLS verification
- route apply mode
- remove routes on disconnect

Passwords should be provided through env vars, OS keychain integration, or a
masked prompt. They should not be persisted in `config.json`,
`sing-box-tui.json`, shell history, or logs.

### Unified Traffic Flow

The intended user flow:

1. User starts `sing-box-tui`.
2. TUI starts or controls sing-box as it does today.
3. User enables system proxy or TUN once.
4. User connects `Remote Access`.
5. Provider authenticates and starts a local bridge.
6. Provider sends pushed routes to the TUI.
7. TUI applies provider-owned route rules to sing-box config.
8. User opens intranet sites normally in the browser.

No browser-specific proxy should be required.

Traffic path for an intranet HTTP request:

```text
browser
  -> OS system proxy or sing-box TUN
  -> sing-box mixed inbound / route engine
  -> provider-owned intranet CIDR route
  -> local remote-access bridge
  -> provider UDP data channel
  -> enterprise intranet service
```

Traffic path for normal proxy traffic:

```text
browser
  -> OS system proxy or sing-box TUN
  -> sing-box selector outbound
  -> selected proxy node
```

## Extensibility

The external-process model is intentionally provider-agnostic.

Future providers can implement the same stdio protocol:

- Hillstone Secure Connect
- Cisco AnyConnect-compatible remote access
- Palo Alto GlobalProtect
- Fortinet SSL VPN
- OpenVPN
- WireGuard
- SSH-based internal forwarding
- Custom enterprise gateway protocols

Provider differences are isolated behind capabilities:

- pushed routes or static routes
- pushed DNS or static DNS
- L3 tunnel, HTTP bridge, SOCKS bridge, or process-local forwarding
- password, certificate, MFA, browser SSO, or device binding
- graceful logout or process termination only

The TUI should only depend on capabilities and events, not on protocol-specific
packet details.

## Security Considerations

- Never log passwords, session keys, cookies, MFA tokens, or raw auth payloads.
- Direct password config is allowed for local usability, but env/keychain based
  sources should remain available for users who do not want plaintext state.
- Redact session identifiers unless required for local debugging.
- Validate provider manifests before spawning executables.
- Prefer absolute executable paths after manifest resolution.
- Treat provider processes as less trusted than the main TUI.
- Restrict route application to CIDRs supplied by the provider session that
  owns the local bridge for that profile.
- Show route changes before applying them when possible.
- Do not silently disable TLS verification; if verification is disabled, surface
  that state in the UI.
- Do not automatically start remote access on boot until password storage and
  stale route cleanup are designed.

## Implementation Plan

### Phase 1: Internal Abstractions

- Add a `network_access` module.
- Define remote-access state, route, DNS, bridge, command, and event types.
- Add unit tests for JSON serialization and state transitions.
- Keep the current Hillstone CLI behavior working.

### Phase 2: External Provider Protocol

- Add provider manifest parsing.
- Add an external provider process adapter.
- Implement JSON-line command and event transport over stdio.
- Add tests with a fake provider process.

### Phase 3: Hillstone Provider Process

- Move reusable Hillstone code into library modules if needed.
- Add a `rap-hillstone` binary.
- Make it speak the remote-access provider protocol.
- Keep provider-specific protocol comments near the hard-won packet and route
  handling code.
- Ensure disconnect sends the Hillstone logout and stops the bridge.

### Phase 4: Route Management

- Convert pushed routes into provider-owned sing-box route rules.
- Preserve existing route ordering and direct-bypass behavior.
- Avoid port restrictions for pushed route CIDRs.
- Add `Clear routes` support for provider-owned routes.
- Validate generated config with `sing-box check` where available.

### Phase 5: TUI Integration

- Add a Remote Access panel.
- Add connect/disconnect actions.
- Show provider state, pushed routes, bridge address, and last error.
- Keep system proxy and sing-box selector workflows unchanged.
- Make stale bridge failures visible with actionable status text.

### Phase 6: Documentation

- Document user-facing terminology.
- Document provider manifest format.
- Document password handling.
- Document troubleshooting for:
  - auth failure
  - route not applied
  - stale bridge / `502 Bad Gateway`
  - TLS verification disabled
  - sing-box config not reloaded

## Open Questions

- Should provider-owned routes be removed from `config.json` on disconnect by
  default, or only marked inactive in TUI state?
- Should route application require an explicit confirmation the first time a new
  CIDR is pushed?
- Should provider logs be stored on disk or kept in memory only?
- Should DNS pushed by remote access be applied to sing-box DNS rules, system
  DNS, or displayed only in the first version?
- Should the first provider protocol support MFA/browser SSO, or should that be
  added after password-based Hillstone is stable?
- Should a shorter `vpn-provider-hillstone` compatibility alias be provided for
  users who expect the VPN term?

## Acceptance Criteria

- A user can connect and disconnect Hillstone remote access from the TUI.
- The TUI does not rely on the official Hillstone client.
- Browser traffic uses the same sing-box entry point as normal proxy traffic.
- Pushed intranet route CIDRs are applied without port restrictions.
- `10.1.126.5:8099`-style intranet targets route through the remote-access
  bridge when the provider is connected.
- Disconnect stops the local bridge and sends provider logout when supported.
- Provider-specific protocol failures do not crash the TUI.
- Secrets are redacted from logs and UI output.
- The architecture can add a second provider by implementing the same manifest
  and JSON-line process protocol.
