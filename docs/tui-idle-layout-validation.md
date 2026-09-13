# Idle dashboard terminal layout validation

**2026-09-13 scope update:** Use existing core-wide traffic rates and totals, with no direct exclusion, per-node byte attribution, or separate probe accounting. Earlier proxy-only completeness gates below are historical findings and no longer block the refactor. See ADR 0003. Current editable Figma frames: 1025:2 (120×30), 1027:16 (80×24). No core changes.

2026-09-11. This is a standalone Ratatui fixture renderer, **not production integration**. It neither collects traffic nor implements persistence, navigation, shortcuts, or mouse behavior.

## Reproduce and inspect

```powershell
cargo run --locked --example idle_dashboard_layout_probe
cargo test --locked --example idle_dashboard_layout_probe
```

The renderer exports actual `TestBackend` cells to text, ANSI and SVG under `artifacts/idle-dashboard-layout/`. The checked PNG previews are rasterized from these SVGs using headless Edge, not hand-drawn approximations. SVG uses a 9×18 preview cell; Figma's design cell is 8×16, and the application always allocates terminal cells rather than pixels.

- [120×30 PNG](../artifacts/idle-dashboard-layout/idle-120x30-clean.png) / [SVG](../artifacts/idle-dashboard-layout/idle-120x30.svg) / [text](../artifacts/idle-dashboard-layout/idle-120x30.txt)
- [80×24 PNG](../artifacts/idle-dashboard-layout/idle-80x24-clean.png) / [SVG](../artifacts/idle-dashboard-layout/idle-80x24.svg) / [text](../artifacts/idle-dashboard-layout/idle-80x24.txt)
- [Figma source screenshot](../artifacts/idle-dashboard-layout/figma-dashboard-reference.png)

The current preview deliberately uses short, consistent Chinese labels, five connection rows, three aggregate time ticks, and no fixture or implementation explanations inside the product UI. A/B interval color alternation remains, but A/B explanatory text is removed; short node names identify boundaries. The preview export preserves cell positions, foreground colors and grapheme width. This fixture has one uniform background and no focus/modifier states. It does not validate a real Windows Terminal font, fallback glyphs, emoji shaping, monochrome fallback, or all terminal capabilities.

## Live Figma evidence and overrides

Read-only calls used **`mcp__figma__use_figma`** for handoff `1014:2`, Terminal Contract `874:2`, and dashboard `1:80`, plus **`mcp__figma__get_screenshot`** for `1:80`, in file `jGcpW9esdjzunUpuU3Aqu5`. The separate `mcp__codex_apps__figma_*` connector is not the source for this verification.

The live Terminal Contract explicitly defines 120×30 standard, 80×24 minimum, and a resize message with quit/help below that minimum. This corrects the earlier unverified suggestion of 64×22. Borders consume cells; CJK consumes two; truncation must preserve graphemes and reserve metric width first. Ordinary borders are subdued. The source dashboard still says “whole-machine” and uses metric colors for aggregate history: ADR 0003 supersedes those details. It also supersedes Connection History with Active connections. ADR 0002 defines hiding order and detail access.

## Verified layout budget

Coordinates start at zero; every bordered rectangle includes its border.

| 120×30 region | x | y | width | height |
| --- | ---: | ---: | ---: | ---: |
| Current route summary | 0 | 0 | 120 | 4 |
| Current node quality | 0 | 4 | 38 | 12 |
| Active connections | 0 | 16 | 38 | 12 |
| Aggregate chart container | 38 | 4 | 82 | 24 |
| Footer, without border | 0 | 28 | 120 | 2 |

Aggregate chart interiors reserve an eight-column y-label gutter. Both actual plot rectangles are 72 columns wide, with x=47..118. Latency occupies y=8..13 and throughput y=16..21. Boundary labels share the same x projection above both charts; common time labels sit at y=23. Units and minimal upload/download style hints occupy dedicated lines. The bottom status line only notes missing history and unavailable probe attribution. Explanations stay in this document rather than the product preview.

The implemented responsive policy is:

| Size | Visible regions |
| --- | --- |
| At least 120×30 | All regions |
| 96–119 columns and at least 30 rows | Hide connections; keep 30-column node panel |
| 80–95 columns or 24–29 rows | Hide connections, then node panel; aggregate full width |
| Below 80 columns or 24 rows | Resize message; quit/help contract shown |

Rendered cases: 120×30, 119×30, 96×30, 95×30, 120×29, 80×24, 79×24, 80×23, 20×4. Full and minimum PNGs were visually inspected: no observed clipped borders or overlapping labels; the 80×24 graphs are only three terminal rows each and support coarse trends, not precise value comparison.

## Data and drawing semantics exercised

Fixtures include A→B→A route intervals (cyan/yellow/cyan), with the second A representing a new interval; current-route probe latency; estimated-throughput presentation with download as Braille-connected lines and upload as dot scatter; node latency in pink and sparse measured node throughput in green; CJK destinations (combining graphemes and ZWJ emoji remain covered by truncation tests); and an unobserved -15m..-12m window. Values stay nonnegative. Latest summary values round the fixture's actual last points. Probe attribution is shown as unavailable (探测流量 —), not a fabricated precise subtotal. The accepted implementation will use existing sing-box statistics without changing sing-box and must label estimated or unavailable attribution honestly.

Charts are separate datasets at route boundaries and missing windows; no fabricated zero or line crossing the missing interval. Both charts use explicit plot rectangles with no built-in axis labels or legend. This avoids Ratatui automatically changing plot width according to different y-label widths. There is no native dashed `GraphType`; connected Braille lines versus scatter dots provide a practical style distinction while color retains its interval meaning.

Four tests check responsive hiding and minimum size, shared-projection node labels, interval colors in both plots, absence of spike-like boundary glyphs, and blank gap interiors in both actual buffers, and grapheme-safe truncation plus reserved metric columns. `cargo test --locked --example idle_dashboard_layout_probe` passes all four. The renderer run completes all nine sizes.

## Limits and next implementation seam

The current application `src/tui/view/dashboard.rs` is the operational candidate view. A production implementation should add a separate idle snapshot and renderer, consuming a page-independent history service. The fixture does not prove that `c`, `i`, or Ctrl+K handlers exist, return correctly, or obey idle-timer rules: footer hints express the agreed contract only.

No large-fixture downsampling, arbitrary rapid-switch label collision handling, scrolling history details, real collector data, or persistence is implemented here. Thirty minutes over 72 cells means about 25 seconds per horizontal cell: production must deliberately aggregate 2-second traffic samples while retaining gaps and route boundaries. Dense switches require collision management and accessible full interval labels. Cells have a single foreground color, so overlapping series or multiple intervals in one cell cannot preserve every color simultaneously. Windows Terminal and its configured font remain a required final manual check.

## Node value readability revision

The node panel retains both sparse histories. Each now has a concrete latest value, sample age, and a three-row plot with explicit zero and upper bound: latency **28 ms / 刚测**, range 0–120 ms; sustained probe **8.0 MiB/s / 2分钟前**, range 0–10 MiB/s. The shared -30m/current axis remains. The current 28 ms measurement is added to node history as well as the aggregate history; the older 50 ms sample remains visible. Probe throughput is a different measurement from the 3.9 MiB/s actual-traffic estimate in the summary. No additional benchmark is triggered. A focused test verifies the labels, scales, five latency samples and three throughput samples at both 120 and 96 columns.

[Updated node-detail preview](../artifacts/idle-dashboard-layout/idle-120x30-node-detail.png). Global charts and the 80×24 layout are unchanged.
