# Internet Proxy telemetry contract experiment

**2026-09-13 scope update:** Use existing core-wide traffic rates and totals, with no direct exclusion, per-node byte attribution, or separate probe accounting. Earlier proxy-only completeness gates below are historical findings and no longer block the refactor. See ADR 0003. Current editable Figma frames: 1025:2 (120×30), 1027:16 (80×24). No core changes.

Run date: 2026-09-11 (Asia/Shanghai). This is an actual isolated-core experiment, not a live-network benchmark. It evaluates the accepted ADR 0003 requirements; it does not relax complete accounting to connection-snapshot estimates.

## Installed core capability

Executable: `C:\Users\Administrator\AppData\Local\sing-box-tui\core\sing-box.exe`.

- Version: `1.13.13-winterloo.2`; Go `1.25.11`, Windows amd64.
- Revision: `c65bc7e2b743e991bab4ae428abf6c894abd917d`.
- SHA-256: `DCF5BE84DA3361EADD22EFB23DF5D5426826AD51B2A7D0C07F90D938DA684EC9`.
- Build tags include `with_clash_api`, but not `with_v2ray_api`.
- Running `check` against a separate minimal V2Ray statistics configuration returned exit 1: `create v2ray-server: v2ray api is not included in this build, rebuild with -tags with_v2ray_api`.

Reproduce with `./scripts/test-telemetry-stats-capability.ps1`. Evidence directory: `C:\Users\Administrator\AppData\Local\Temp\sing-box-telemetry-contract-15c494eb7241478a8fa08594ba1a07bc`. This check started no proxy process.

[Official V2Ray API documentation](https://sing-box.sagernet.org/configuration/experimental/v2ray-api/) describes configurable inbound/outbound/user statistics and explicitly says the API is not included by default. The installed build's rejection establishes the local capability gate. No replacement core was downloaded or used.

## Loopback Clash API experiment

Reproduce with `python scripts/test-telemetry-clash-contract.py`. Python standard library only. The script creates two local SOCKS nodes, local HTTP destinations, a private mixed inbound and controller, and its own temporary config. SOCKS forwarding rejects every destination except `127.0.0.1`. Only the recorded child process is terminated. No live configuration, process, TUN, DNS, system proxy, or trust-store changes occur.

Two completed runs used owned PIDs 24696 and 14952. Both were terminated and reaped; exit 1 is the result of Windows process termination, not a spontaneous test failure. Evidence directories contain exact configs, logs, and JSON snapshots:

- `C:\Users\Administrator\AppData\Local\Temp\sing-box-clash-contract-iotuezd3`
- `C:\Users\Administrator\AppData\Local\Temp\sing-box-clash-contract-lm04_tpl`

The following exact numbers are from the first completed run; the second reproduced all final totals and the Delay coverage counterexample.

| Observation | Download total | Upload total | Active snapshots / interpretation |
| --- | ---: | ---: | --- |
| Fresh process | 0 | 0 | Empty |
| Short proxy request completed | 4230 | 92 | Empty: entire short connection absent from the active snapshot |
| Short direct request completed | 8460 | 184 | Empty: direct contributes equally to global totals |
| Failed local HTTPS Delay completed | 8460 | 184 | Empty, despite 1847 bytes relayed by local SOCKS during that attempt |
| Old node before selector switch | 17811 | 280 | node-a connection downloaded 9351 bytes |
| Same connection after switching to node-b | 20883 | 280 | Same ID and node-a chain, now 12423 bytes |
| New short request completed | 27161 | 372 | Old node-a connection still present, now 14471 bytes |
| All requests completed | 78361 | 372 | Empty: global total retains the old connection's remaining tail |

The long HTTP response was 65671 bytes including response headers. Its last observed per-connection value was 14471, leaving 51200 bytes absent from a collector that only differences those snapshots. The global final total is exactly `4230 * 3 + 65671 = 78361`, including the direct request. Global totals therefore retain these routed short connections and tails, but do not provide their node/direct attribution once the connections disappear.

The Delay experiment deliberately uses an HTTPS URL pointing to the plain local HTTP server. The TLS handshake fails, yielding HTTP 503 from the Delay API. Local SOCKS relayed bytes grew from 4322 to 6169, but Clash totals did not change. This proves a **failed probe with actual proxy-channel traffic is omitted**; it is not a successful HTTPS latency measurement. Success-path Delay accounting, TLS trust, UDP, protocol encapsulation, and detour chains were not experimentally established.

An earlier draft used an HTTP Delay URL and got 503. The matching core source rewrites HTTP URLs to the default target; the local SOCKS allowlist rejected that non-loopback target before any destination connection. The checked-in script uses HTTPS and avoids that rewrite. No external traffic was forwarded by the experiment's SOCKS nodes.

Each completed run is a separate isolated process and starts at zero. Its final totals were read before termination and persisted by the experiment, illustrating the required final-read boundary. Production `node-runtime-manager` integration, simultaneous runtime aggregation, graceful final draining, and abrupt-crash loss were not implemented or tested here.

## Matching source evidence

Read-only source inspection used `git -C D:/proj/sing-box show v1.13.13-winterloo.2:<path>` rather than the checkout's different HEAD. The tag resolves to the installed binary revision above.

- `experimental/clashapi/proxies.go:205`, `getProxyDelay`: directly calls `urltest.URLTest(ctx, url, proxy)` after its HTTP URL rewrite.
- `common/urltest/urltest.go:75,95`, `URLTest`: directly calls `detour.DialContext`, then performs a HEAD request. This is distinct from the routed inbound connection path.
- `route/route.go:153`: the routed connection path invokes `tracker.RoutedConnection`.
- `experimental/v2rayapi/stats.go:63,66,95`, `RoutedConnection` / `RoutedPacketConnection`: counters are attached to routed connections and use `matchOutbound.Tag()`; this is not proof of leaf-node attribution through every selector or detour.

The line numbers above refer to that exact tag's file contents, not the current checkout. The resulting source inference is that adding the missing build tag is necessary to use V2Ray statistics, but **does not by itself establish complete Delay coverage or correct leaf-node accounting**. Separately, the runtime experiment directly disproves complete failed-probe coverage by the existing Clash totals.

## Implementation consequence

**Intermediate decision (superseded by the 2026-09-13 scope update above):** keep sing-box unchanged and use available telemetry. The complete-accounting failures below remain valid experimental findings, but are no longer a delivery gate. Follow the revised ADR 0003: display attributable observed/estimated proxy usage, keep unsupported probe breakdown unavailable, and do not present raw direct-inclusive totals as proxy-only usage. The core-extension proposal below was not accepted and must not be implemented as part of this task.

The installed core cannot satisfy complete Internet Proxy accounting and separately identified probe traffic using its current exposed telemetry alone. Do not label 2-second connection deltas as complete actual usage. Shorter polling or a stream of active snapshots cannot guarantee observing every short connection or its final bytes.

Next implementation step: decide whether to extend the existing custom core with a unified cumulative accounting interface, covering routed user connections and direct-outbound Delay requests, with a nonduplicating Internet Proxy classification and probe-purpose dimension. Preserve the current live Delay contract in ADR 0001. Enabling V2Ray support can supply part of this interface, but the missing paths and attribution semantics still require verification or changes. A sidecar can account for data channels actually routed through it; it cannot account for live Delay traffic that bypasses it.

After that source contract exists, collect each runtime's monotonic counters every 2 seconds, with runtime epoch identities and final draining; aggregate probe traffic once as a subset. Persist metric intervals independently of workspace/provider/node changes, preserve gaps over unobserved periods, and keep active connection snapshots separate. Test successful and failed probes, short TCP/UDP flows, direct exclusion, selector/detour attribution without double counting, old-node transfers, runtime shutdown/restart, and simultaneous isolated runtimes before calling the total complete.

The remaining product precision decision is the byte boundary: application/proxy-channel bytes versus on-wire protocol overhead and retransmissions. The observed HTTP counts include HTTP headers, and are not evidence of whole-link or billing-byte accuracy.
