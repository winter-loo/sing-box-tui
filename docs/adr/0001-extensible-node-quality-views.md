---
status: accepted
---

# Use reconciled node facts and extensible usability views

Node selection will use append-only quality facts and application-level usability results instead of treating one delay value as node health. The TUI keeps the current selector's complete member list as its default node view and adds one derived panel for each built-in or user-authored usability criterion; these panels filter the current selector but never create or modify sing-box selectors.

After the new storage schema is first introduced, the existing SQLite database is deleted and recreated without migration or compatibility reads. Later successful subscription refreshes reconcile nodes individually using the node tag plus a non-secret canonical outbound-configuration fingerprint: unchanged nodes retain measurements, removed or materially changed nodes lose them, and new nodes start without history. Failed refreshes do not alter stored results.

Generic quick probes and URL-only usability criteria reuse the existing sing-box instance's Clash Delay Endpoint for the named outbound. They do not start a probe runtime and do not change the live selector. A probe runtime is reserved for work that needs a candidate-bound HTTP/SOCKS data channel, such as reading a bounded response body, measuring sustained throughput, or executing an application-level request.

User-authored criteria are registered through manifests containing either a lightweight URL or an executable and argument array rather than shell text. URL-only criteria use the live Delay Endpoint and accept any valid HTTP response. The TUI may launch a declared program when richer application semantics are required, while the program continues to own and drive `node-runtime-manager`; the program publishes node results and progress to the TUI as JSON Lines, with diagnostics on stderr. The runtime manager remains application-agnostic and never receives or interprets usability decisions. Custom probes do not run automatically unless their manifest permits background execution and the user enables it.

The active node view defines the automatic-selection candidate set. A panel may choose balanced, low-latency, or throughput ranking, while switching still requires the agreed reachability gate, repeated wins, material improvement, and active-transfer protection. Probe infrastructure failures produce an incomplete assessment and preserve unexpired prior results instead of classifying untested nodes as unusable.

The TUI keeps the existing selector list on the left and presents node-view panels as tabs above the selected selector's candidate list on the right. This layout was selected from the three alternatives captured on the `prototype/node-quality-panels` branch at commit `d724213`. Full node-quality evidence remains available through the existing `i` interaction; narrow terminals do not reserve a permanent third column for details.

This supersedes the result-ownership restriction in `docs/node-usability-probe-design.md`: probe programs still own application-specific probing and decisions, but may now publish those decisions to the TUI for display, persistence, and selection. It does not change the isolation, traversal, or cleanup contract of `node-runtime-manager`.
