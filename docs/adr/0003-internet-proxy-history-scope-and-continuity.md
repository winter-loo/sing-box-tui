---
status: accepted
---

# Keep core traffic history independent of workspace navigation

## Revision 2026-09-13: use core counters without the three filters

The user explicitly chose to keep the existing sing-box core unchanged and then dropped direct exclusion, per-node byte attribution, and separate probe accounting. Use the core's existing upload/download rates and cumulative counters. This supersedes both the original proxy-only accounting requirement and the intermediate proposal to reconstruct estimated proxy usage from active connections.

Label the chart Core traffic. Include whatever the core reports, including direct traffic covered by its counters. Do not label this whole-machine traffic, proxy-only usage, or subscription quota consumed. Preserve the core's counter lifetime/reset semantics; do not turn a counter reset into a negative traffic sample or a synthetic spike. Persisting recent chart samples does not make the displayed runtime total an all-time total.

Remove the separate probe-traffic field entirely. Do not add corrections for probes the core does not count. Keep usable latency and existing sustained-quality measurements; removing per-node byte attribution does not remove node-quality charts. Route interval colors indicate switch times only, never ownership of core traffic. This work must not change, rebuild, replace, or extend sing-box for telemetry.

The user accepted the 120×30 and 80×24 region layouts but requested less visual clutter. Keep the layout, remove fixture explanations and redundant legends from the product view, shorten labels, use sparse axes and connection rows, and keep implementation limitations in details/documentation rather than filling the dashboard with them.

## History and chart contract

The idle dashboard retains an active-connections list and per-node quality history and adds core traffic history. The current editable Figma designs are frames 1025:2 (120×30) and 1027:16 (80×24); handoff 1014:2 records the updated contract. These design fixtures supersede the old dashboard 1:80 and do not imply production integration.

Label the connections area Active connections and retain active snapshots rather than introducing records of ended connections. The 30-minute history persistence in this decision applies to metric series, not connection records. This resolves the handoff's ambiguous Connection History wording; the existing c panel remains an active-connections view.

Aggregate history continues across node, provider, and workspace changes. Its latency series records probe delay through the node that was current when measured, not business-request response time or background probes of other candidates. Candidate probe results belong to their respective node histories. Aggregate traffic can include connections still using previous nodes; a route boundary is not evidence that all existing connections migrated.

Show aggregate latency and throughput in two vertically aligned charts with a shared time axis. Alternate two colors by route interval, rather than assigning colors to node identities; every node switch starts a new interval, including a return to a previously used node. Mark boundaries with node names. Show upload and download separately in the throughput chart, distinguished by line or point style while colors retain their interval meaning. Per-node quality charts retain the agreed pink-latency and green-throughput palette. This replaces the handoff's metric-based colors for aggregate charts so color has only one meaning in each chart.

Persist history and restore the most recent 30 minutes when the TUI restarts. Periods without collected data, including unobserved time while the TUI is closed, remain gaps: do not fabricate zeros or connect across missing periods. Persisted monitoring history avoids losing recent diagnostic evidence when restarting the interface; this decision does not introduce an always-running collector. These are agreed requirements for the upcoming refactor, not implemented behavior.

Collect actual upload and download traffic every 2 seconds, following the current connection refresh cadence. Probe current-route latency every 10 seconds with a lightweight request, reusing an existing equivalent probe when available. Continue collection across all operational pages, independently of whether the idle dashboard is visible. Aggregate throughput uses actual traffic; drawing history must not trigger additional download benchmarks.

Per-node quality history shows existing sustained-quality probe results as measured throughput, distinct from actual traffic usage in the aggregate charts. Missing probe results remain gaps: do not trigger extra benchmarks to populate the chart or extend an old result into a continuous series. A measured result describes that probe's transfer performance, not a guaranteed node capacity.
