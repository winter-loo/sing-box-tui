# sing-box-tui

Terms that define how sing-box-tui owns proxy runtime and network-access behavior.

## Language

**Node dashboard**:
A passive monitoring view for the current route node, with separate histories for current-route node quality and core traffic. It is entered explicitly, is distinct from the operational node list, and contains no candidate-node list.
_Avoid_: Home page, node browser, node selection page

**Core traffic**:
Upload/download rates and cumulative bytes reported by the existing sing-box core, including direct traffic covered by its counters. The dashboard does not exclude direct traffic, attribute bytes to nodes, or separately account for probes. Coverage and resets follow the core; these are neither whole-machine counters nor subscription quota consumption. History continues across route and workspace changes; route colors mark switch times, not byte ownership.
_Avoid_: Proxy-only usage, per-node usage, subscription quota consumed, whole-machine usage

**Probe traffic**:
Traffic carried through Internet Proxy nodes for reachability, sustained-quality, or application-level usability probing. The dashboard does not separately count or add these bytes; existing core counters may not cover every probe path.
_Avoid_: Free traffic, user traffic, extra traffic

**Current route node**:
The Internet Proxy node selected by the current route's selector chain, independently of which candidate the user is browsing. It does not imply that existing connections have moved to that node.
_Avoid_: Focused node, browsed node, all-traffic node

**Route interval**:
A continuous period during which the current route node remains unchanged, ending when that node switches. Returning to a previously used node starts a new interval rather than resuming its earlier interval.
_Avoid_: Node identity, connection lifetime, probe attempt

**Current route probe latency**:
The measured probe delay through the node that was the current route node at the time of measurement. Its history spans route intervals and excludes background measurements of other candidate nodes; periods without measurements remain missing.
_Avoid_: Business request latency, average candidate latency, whole-machine latency

**Browsed node**:
An Internet Proxy candidate currently in focus for inspection. Browsing it does not change the current route node; switching requires explicit activation.
_Avoid_: Current route node, active node, connected node

**Managed sing-box process**:
A sing-box process started by the current `ManagedSingBox` instance and held in its explicit lifecycle ownership for one configuration. Existing, unrelated, user-owned, or previous-instance sing-box processes are never adopted, restarted, or stopped.
_Avoid_: Managed child, adopted process, sing-box service

**Controller readiness**:
The state in which sing-box's controller accepts requests; an operating-system process can be running without being controller-ready.
_Avoid_: Process alive, startup success

**Internet TUN transition**:
A recoverable change between enabled and disabled Internet traffic capture, including its persisted intent and the route state required to restore user configuration.
_Avoid_: TUN toggle, config edit, TUN job

**Private Access session**:
A live connection attempt for one configured intranet profile, including authentication state and the network resources granted by its remote gateway.
_Avoid_: VPN process, connector job, profile runtime

**Node reachability**:
Evidence that an Internet Proxy node can complete a proxy request during a particular probe attempt; it is not a judgment of overall node quality.
_Avoid_: Node health, node quality, working node

**Node startup quality**:
The success and timing characteristics of establishing a usable session from a cold node state.
_Avoid_: Latency, cold latency

**Node sustained quality**:
The stability and transfer characteristics of a node after a usable session has been established.
_Avoid_: Speed, bandwidth quality

**Node suitability**:
A judgment that a node fits a named usage profile, such as balanced, low-latency, or streaming, based on shared measurements interpreted by that profile.
_Avoid_: Best node, fastest node

**Node quality**:
The collection of reachability, startup, sustained, and historical measurements for a node; it is never represented by a single delay value.
_Avoid_: Delay, score, health

**Probe attempt**:
One bounded request through an Internet Proxy node that produces reachability and timing evidence without deciding whether the node is suitable.
_Avoid_: Test, ping, health check

**Reachability assessment**:
The four-level result derived from up to three sequential probe attempts: stable reachable, reachable, degraded, or unreachable for this assessment.
_Avoid_: Benchmark result, pass/fail, node status

**Sustained quality probe**:
A bounded transfer through one node that measures first-byte timing, completion time, and effective throughput after reachability has been established.
_Avoid_: Speed test, bandwidth test, deep test

**Usage profile**:
A named interpretation of shared node-quality measurements for a user goal: balanced, low-latency, or streaming.
_Avoid_: Mode, scoring mode, benchmark type

**Active node transfer**:
Recent meaningful byte growth on connections using the current Internet Proxy node; idle open connections are not active node transfers.
_Avoid_: Active connection, connected node, network activity

**Probe runtime**:
An isolated sing-box runtime that measures a candidate Internet Proxy node without changing the live selector or carrying user traffic.
_Avoid_: Test process, temporary proxy, benchmark instance

**Live outbound probe**:
A URL-only reachability attempt sent by the existing sing-box instance's Clash Delay Endpoint directly through a named outbound; it neither changes the live selector nor creates a probe runtime.
_Avoid_: Live selector probe, isolated probe, proxy request

**Probe outcome**:
The factual result of one probe attempt: reachable, timeout, transport failure, controller failure, invalid measurement, or cancelled; only timeout and transport failure are evidence against the node.
_Avoid_: Pass/fail, node status, error

**Incomplete assessment**:
A reachability assessment that cannot be derived because too few node-attributable probe outcomes were collected, typically after controller failure, invalid measurement, or cancellation.
_Avoid_: Failed node, unreachable, unknown error

**Node view panel**:
A named TUI view over Internet Proxy nodes. The default panel shows the members of the current selector; additional panels show nodes accepted by one built-in or user-authored usability criterion, and a node may appear in multiple panels.
_Avoid_: Selector, usage mode, node group

**Usability criterion**:
An application-level standard that decides whether a node belongs in a node view panel, such as bounded streaming transfer success or a real Agy Gemini request; it is independent of generic reachability evidence.
_Avoid_: Usage profile, latency threshold, selector rule

**Subscription node reconciliation**:
The per-node comparison performed after a successful subscription refresh: preserve measurements for unchanged nodes, discard measurements for removed or materially changed nodes, and initialize newly added nodes without history.
_Avoid_: Clear history, subscription reset, database rebuild

**Node configuration fingerprint**:
A non-secret hash of a node's canonical outbound configuration, used together with its tag to decide whether subscription reconciliation may preserve prior measurements.
_Avoid_: Node ID, server address, config copy

**Usability probe manifest**:
A user-owned declaration that gives one usability criterion a stable ID, panel label, either a lightweight URL or an executable with argument array, ranking policy, and optional background schedule and result lifetime.
_Avoid_: Plugin, shell command, selector config

**Usability probe result**:
A node-attributable application-level outcome published by a usability probe program to the TUI over JSON Lines; program or runtime failures leave the assessment incomplete rather than making nodes unusable.
_Avoid_: Runtime result, reachability result, node health
