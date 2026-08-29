# sing-box-tui node usability probe design

## Current user intent

Build a generic node usability probing facility for user-authored usability probe programs. The user program owns all application-specific probing and pass/fail decisions. The lower-level runtime facility must not ask for, receive, persist, or aggregate `true`/`false` probe results.

ADR-0001 later extends the surrounding TUI architecture: a probe program may publish its application-specific results to the TUI for a derived node-view panel. That does not put result interpretation or persistence into the node runtime manager described here.

No production implementation has been authorized yet. The architecture below was confirmed through a completed grilling session. Do not begin implementation until the user explicitly asks.

## Lightweight URL probing stays outside this manager

The TUI does not create a node runtime when a criterion only supplies an HTTP or HTTPS URL and defines usability as receiving any valid HTTP response. It calls the existing sing-box instance's Clash `/proxies/{node}/delay` endpoint for the named outbound. That internal request does not change the live selector and is sufficient for three-attempt generic reachability and URL-only usability panels.

Create a node runtime only when the caller needs a fixed candidate-bound HTTP/SOCKS proxy for response-body validation, bounded transfer measurement, or an arbitrary application request. The manager retains its own Delay Endpoint prefilter because it is also a standalone facility used by programs that may run without the TUI or its recent reachability facts.

## Final terminology and control relationship

- **Usability probe program**: a user-authored program in any language. It starts and controls the manager, performs arbitrary node usability probes, and owns all results.
- **Node runtime manager**: the generic child process implemented as `sing-box-tui node-runtime-manager --stdio`.
- **Node runtime**: one isolated traversal lane formerly called a worker. It owns an initialization URL, one temporary sing-box process, one fixed local mixed-proxy endpoint, and one independent node cursor.

Control is inverted from the earlier plugin proposal:

```text
usability probe program
  -> starts node runtime manager once
  -> initializes manager
  -> creates one or more node runtimes
  -> repeatedly calls next(runtime_id)
  -> performs its own probes and aggregation
```

The manager never starts the usability probe program.

## Transport interface

The manager is a long-running child process controlled with JSON Lines RPC over its stdin/stdout. Manager stdout is protocol-only; diagnostics go to stderr. Requests have IDs so different node runtimes can have outstanding operations concurrently and responses may arrive out of order. No protocol-version field is required.

The minimal operations are:

```text
initialize
create_runtime
next
close_runtime
```

`close_runtime(runtime_id)` cancels any in-flight `next`, terminates the isolated sing-box, and is idempotent. stdin EOF or parent termination closes all runtimes. The usability probe program's own stdout/stderr remain unrestricted because it owns the manager's pipes.

## Manager initialization

`initialize` occurs exactly once before any other operation. It accepts either both of these paths or neither:

```json
{
  "config_path": "D:\\path\\config.json",
  "sing_box_executable": "D:\\path\\sing-box.exe",
  "max_runtimes": 4
}
```

- If both paths are supplied, validate and use them.
- Supplying only one path is an error.
- If both are omitted, reuse the verified real config and executable paths of the current `sing-box-tui` instance.
- With no active instance, require explicit paths.
- With multiple active instances, return `ambiguous_runtime_environment` rather than guessing; candidates may include PID and config path but no secrets.
- Reuse only the real config path, real executable path, and config-directory-relative file semantics. Do not copy another process's environment variables.
- Clear inherited proxy environment variables for isolated sing-box children.
- Read the source config once. One manager process owns one immutable config snapshot; config changes require restarting the manager.
- Default `max_runtimes` is the project's current concurrency setting when available, otherwise 4. Exceeding the limit returns `runtime_limit_reached` without queuing.

## Node enumeration

Usability probe programs do not specify selectors. Every node runtime independently traverses all nodes reachable from every `type: "selector"` outbound in the config.

- Process selectors in config order and their members in declared order.
- Recursively expand selector/urltest references to concrete leaf outbounds.
- Exclude `direct`, `block`, DNS-only, and other non-proxy/internal outbounds.
- Deduplicate concrete leaves globally by tag within each node runtime.
- Record every selector to which a returned node directly or indirectly belongs.
- Different node runtimes each traverse the full node set. If users initialize multiple runtimes for the same URL and duplicate work, that is caller misuse; multiple runtimes are intended for different URLs.

The current config inspected during design had four selectors and concrete `hysteria2`, `trojan`, `vless`, and `vmess` leaves, plus `direct` and `block` entries that must be excluded.

## Node runtime lifecycle

`create_runtime` accepts an HTTP or HTTPS initialization URL and optional `connectivity_timeout_ms`, whose default is 2000 ms. It does not accept headers, cookies, authentication, or business predicates.

Creation:

1. Allocate private dynamic mixed/controller ports.
2. Generate an isolated config from the manager's immutable snapshot.
3. Remove all original inbounds, including TUN, and never change the system proxy.
4. Preserve concrete outbounds and required DNS/certificate/endpoint/config-relative dependencies.
5. Replace business route rules so all traffic entering the runtime proxy is forced through the selected node.
6. Start and verify one temporary sing-box instance.
7. Keep its internal selector on an internal `block` outbound until a successful `next`.
8. Return `runtime_id`, the fixed loopback HTTP/SOCKS5 mixed-proxy URLs, and `total_candidates`.

Each runtime is independent and owns its own complete cursor. Different runtimes can scan concurrently. Within one runtime, at most one `next` may be outstanding.

## next semantics and connectivity prefilter

`next(runtime_id)` is lazy and serial inside that runtime:

1. Switch to internal `block` and force-close all old connections.
2. Starting at the next candidate, call isolated sing-box's Clash `/proxies/{node}/delay` with the runtime URL and timeout.
3. Treat any valid HTTP response as network reachable regardless of status. The HEAD-based delay probe is only a transport prefilter; it never decides application usability.
4. Skip timeout/transport/switch-invalid candidates internally.
5. On the first reachable candidate, select it on the runtime selector and return it.
6. If no candidates remain, return `end: true`, final counts, and close the runtime automatically.

Calling `next(runtime_id)` again implicitly releases the prior node. It is a hard switching point: old HTTP clients, connection pools, browser connections, and QUIC sessions must not be reused. The fixed proxy address remains the same until the runtime closes.

A successful non-terminal response contains only a minimal node description and progress:

```json
{
  "end": false,
  "node": {
    "tag": "Hong Kong 01",
    "selectors": ["group-a"],
    "ordinal": 17
  },
  "proxy": {
    "http": "http://127.0.0.1:23145",
    "socks5": "socks5://127.0.0.1:23145"
  },
  "scanned": 17,
  "reachable": 4
}
```

Do not return full outbound JSON, server addresses, credentials, keys, or failed-node details. Expose only aggregate scanned/reachable counts. A selected node has no lease timeout and remains active until `next`, `close_runtime`, manager shutdown, or infrastructure failure.

Single-node connectivity failure is an internal skip. If the runtime's sing-box process/controller/temp configuration fails as a whole, return structured `runtime_failed`, poison that runtime, and do not silently restart or resume it.

## Isolation and cleanup invariants

- Never change the live selector, live config, TUN state, or system proxy.
- Bind runtime proxy/controller listeners to loopback only.
- Store derived configs/logs in private temporary directories; they may contain source-config secrets and must be reliably removed.
- Use bounded diagnostics and never emit config credentials.
- Ensure manager exit, pipe EOF, cancellation, and usability-probe-program crashes terminate every child sing-box.
- Preserve relative config dependencies as if loaded from the source config directory.
- Support Windows, macOS, and Linux without TUN or elevation.

## Usability probe program support

The formal interface is the language-neutral JSON Lines protocol. Provide a thin Python example client showing manager startup, initialization, multiple independent runtimes, concurrent `next` calls, proxy use, and cleanup. Do not commit to maintained language SDKs in the first version.

## Superseded designs

The following earlier ideas are explicitly obsolete:

- `probe(context) -> bool` callbacks;
- loading an in-process plugin/DLL;
- launching one external process per node;
- the manager launching a long-running plugin;
- sending `true`/`false` or classifications back to the manager;
- manager-owned result persistence or `usable_nodes` aggregation (TUI-owned view persistence is allowed by ADR-0001);
- requiring callers to select selector names;
- a globally shared node queue across runtimes.

## Established evidence and references

- Existing Clash `/proxies/{node}/delay` proves transport/HTTP exchange only and can report a regional-block page as reachable. This limitation is intentional for the manager's coarse prefilter.
- Earlier live-selector `agy` tests were not isolated and can affect unrelated traffic; do not reuse that mechanism.
- Feasibility study: `D:\proj\sing-box-tui\docs\gemini-node-probe-feasibility.md`.
- Throwaway prototype: `D:\proj\sing-box-tui\scripts\gemini-node-probe-prototype.py`; experimental only.
- Existing latency reference: `src\controller.rs`, especially `BenchmarkRequest`, `spawn_benchmark_task`, and `measure_delay`.
- Existing benchmark persistence in `src\benchmark_workflow.rs` and `src\storage.rs` is not a dependency of the node runtime manager. TUI-owned quality and usability-view persistence is specified separately by ADR-0001.

## Next action

Implementation is exposed as:

```text
sing-box-tui node-runtime-manager --stdio
```

See `scripts/node-usability-probe-example.py` for a Python example of the language-neutral protocol. It intentionally stops after retrieving one reachable node per runtime; callers supply their own usability probes and result handling.
