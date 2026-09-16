# Idle dashboard terminal layout validation

Updated 2026-09-16 for the production Idle Dashboard renderer in
`src/tui/view/idle_dashboard.rs`. The live Figma handoff is `1014:2`; the
editable reference frames are `1025:2` (120×30) and `1027:16` (80×24) in file
`jGcpW9esdjzunUpuU3Aqu5`.

Figma uses an 8×16 design grid, but the application never allocates pixels.
Reference dimensions and proportions are converted to terminal columns and
rows, then recomputed from the current `Rect` on every draw.

## Production checks

```powershell
cargo test --locked test_render_idle_dashboard_
cargo check --locked
```

The buffer tests cover the reference sizes, an intermediate breakpoint,
representative larger Windows Terminal sizes, a wide/short non-standard
aspect ratio, and the below-minimum guard.

## 120×30 reference budget

Coordinates start at zero. Header and footer each reserve two rows: one for
content and one subdued horizontal boundary. The monitoring body has one-cell
outer gutters and one-cell gaps between sibling panels.

| Region | x | y | width | height | Boundary |
| --- | ---: | ---: | ---: | ---: | --- |
| Breadcrumb header | 0 | 0 | 120 | 2 | bottom separator |
| Current-node quality | 1 | 2 | 37 | 15 | no box |
| Inter-panel gap | 38 | 2 | 1 | 26 | none |
| Active connections | 1 | 18 | 37 | 10 | full subdued box |
| Global history | 39 | 2 | 80 | 26 | no box |
| Footer | 0 | 28 | 120 | 2 | top separator |

This preserves the Figma proportions: 1-cell outer gutter, 37-cell left rail,
1-cell gap, and 80-cell global-history surface. Current-node quality and global
history use the surface color without decorative boxes. Active connections
keeps its border because the design treats it as a discrete list.

## Responsive policy

Visibility is decided from the usable terminal-cell budget after header,
footer, and outer gutters are reserved.

| Available terminal | Visible monitoring regions |
| --- | --- |
| 120×30 reference and larger readable layouts | quality, connections, global history |
| At least 94 usable columns and 20 usable rows | quality and global history; connections hidden first |
| Narrower supported viewports, including 80×24 | global history only |
| Below 80 columns or 24 rows | resize notice with quit/help |

At 96×30 the body is `x=1..94`: a 30-cell quality surface, one-cell gap, and
63-cell global-history surface. At 80×24 the global surface is `x=1..78`,
`y=2..21`. A 120×29 or 150×24 viewport retains quality and global history but
hides connections, because both remaining regions still meet their cell
budgets.

To avoid awkwardly stretched plots, monitoring content is capped at 144
columns and 30 rows and centered inside the available body. Tests pin 132×36
to `130×30` at `(1,3)` and 160×45 to `144×30` at `(8,7)`. Panel widths still
derive from the actual capped cell width; no Figma pixel coordinate is used at
runtime.

## Chart and footer contract

Node quality retains current latency and sustained-probe values, units, sample
ages, upper/zero bounds, and independent `-30m`/current axes. Global history
retains route labels, explicit latency and core-traffic units, upload/download
style hints, upper/zero bounds, and `-30m`/`-15m`/current axes for both plots.

Latency and traffic samples remain separate datasets across route changes and
collection gaps. The 120×30 buffer fixture places samples twenty minutes apart
and asserts that neither global plot draws Braille cells through the missing
interval. Node-quality sparse samples have an equivalent gap assertion.
Current-route collection follows the applied selector chain independently of
the browsed provider, and node-quality history matches both selector and node
name so equal node labels from different providers cannot be merged.

Footer shortcuts are selected from full, compact, and key-only variants only
after reserving the complete right-aligned global status plus live rates. Both
sides retain a one-cell outer inset, and the status is never replaced by rates
alone at supported reference sizes.

The standalone `examples/idle_dashboard_layout_probe.rs` remains a historical
artifact renderer. Production acceptance and geometry are defined by the
renderer and buffer tests above.
