# Global Figma conformance audit (#43)

Audited 2026-09-24 against the live Figma product page in file
`jGcpW9esdjzunUpuU3Aqu5` and its Implementation Handoff `1014:2`.
All 25 visible product frames listed below were inspected. Hidden legacy layers
and illustrative values are not runtime requirements. The 960×480 frames are
structural references for a 120×30 terminal; `1027:16` is the 80×24 dashboard
reference. ADR 0002 governs viewport priority, and ADR 0003 governs core traffic
and current route history semantics.

## Shipped view to source frame map

| Shipped view or state | Current Figma frame | Runtime renderer |
| --- | --- | --- |
| Internet node list | `8:2` | `src/tui/view/dashboard.rs` |
| Internet probe running | `73:277` | `src/tui/view/dashboard.rs` |
| Internet provider picker | `796:7` | `src/tui/view/provider_modal.rs` |
| Private Access disconnected | `595:2` | `src/tui/view/dashboard.rs` |
| Private Access connecting | `596:2` | `src/tui/view/dashboard.rs`, `private_access.rs` |
| Private Access authentication | `596:226` | `src/tui/view/private_access.rs` |
| Authentication required code error | `596:348` | `src/tui/view/private_access.rs` |
| Authentication with a filled code | `596:409` | `src/tui/view/private_access.rs` |
| Authentication submitted | `596:110` | `src/tui/view/private_access.rs` |
| Private Access connected | `73:2` | `src/tui/view/dashboard.rs` |
| Connected, DNS expanded | `599:2` | `src/tui/view/dashboard.rs` |
| Connected, routes expanded | `599:62` | `src/tui/view/dashboard.rs` |
| Connected, both expanded | `602:102` | `src/tui/view/dashboard.rs` |
| Private Access disconnecting | `596:168` | `src/tui/view/dashboard.rs`, `private_access.rs` |
| Private Access error | `133:2` | `src/tui/view/dashboard.rs` |
| Private Access profile picker | `998:2` | `src/tui/view/provider_modal.rs` |
| Node quality detail | `600:883` | `src/tui/view/node_quality_detail.rs` |
| Active connections | `600:592` | `src/tui/view/connections.rs` |
| Settings | `600:2` | `src/tui/view/settings.rs` |
| Help | `600:288` | `src/tui/view/help.rs` |
| Command palette | `635:2` | `src/tui/view/command_palette.rs` |
| Node dashboard, full | `1025:2` | `src/tui/view/node_dashboard.rs` |
| Node dashboard, compact | `1027:16` | `src/tui/view/node_dashboard.rs` |
| First launch workspace choice | `953:3` | Design reference; no matching shipped page |
| Returning launch notice | `953:25` | Design reference; runtime restores the saved workspace directly |
| Subscription URL first-run setup | No current frame | `src/tui/view/onboarding.rs` |
| Unsupported terminal resize notice | Terminal contract `874:2` | `src/tui/ds/widgets.rs` |

## Viewport and terminal checks

`src/tui/global_conformance_tests.rs` renders the actual app through
`TestBackend` at 80×24, 96×27, 120×30, 160×30, 120×44, 107×35, and
106×32. The cases cover Internet, Private Access, the node dashboard,
Connections, Node Quality, Settings, Help, Command Palette, both provider
pickers, Private Access lifecycle states and expansions, progress,
authentication with a CJK field label, and the subscription wizard. The
assertions check breadcrumb and dialog controls, the inspection-dialog
border and close-control rows, status in the bottom-right area, the rightmost
live rate, and semantic footer colors. Existing focused tests
check responsive panel geometry, scroll behavior, charts, and grapheme-safe
truncation.

On 2026-09-24, the latest debug executable built with `cargo build --locked`
was started in a separate Windows Terminal window. It used an isolated
configuration with no inbounds, a `direct`/`block` selector, controller port
19092, and two example-only Private Access profiles. No VPN was connected.
`GetConsoleScreenBufferInfo` measured the actual terminal at **120×30** and
**80×24** cells. The screenshots below are DPI-aware captures of the real
Windows Terminal window, including its rendered colors and bottom row.

| Actual size | Captured views |
| --- | --- |
| 120×30 | [Internet](tui-captures/issue43/120x30-internet.png), [Private Access disconnected](tui-captures/issue43/120x30-private-disconnected.png), [profile picker](tui-captures/issue43/120x30-private-picker.png), [node dashboard](tui-captures/issue43/120x30-node-dashboard.png), [Connections](tui-captures/issue43/120x30-connections.png), [Node Quality](tui-captures/issue43/120x30-quality.png), [Settings](tui-captures/issue43/120x30-settings.png), [Help with Tab selected](tui-captures/issue43/120x30-help-tab.png), [Command Palette](tui-captures/issue43/120x30-palette.png) |
| 80×24 | [Internet](tui-captures/issue43/80x24-internet.png), [Private Access disconnected](tui-captures/issue43/80x24-private-disconnected.png), [profile picker](tui-captures/issue43/80x24-private-picker.png), [compact node dashboard](tui-captures/issue43/80x24-node-dashboard.png), [Node Quality from the dashboard](tui-captures/issue43/80x24-dashboard-quality.png), [Connections](tui-captures/issue43/80x24-connections.png), [Help](tui-captures/issue43/80x24-help.png), [Command Palette](tui-captures/issue43/80x24-palette.png) |

The captured views keep borders, close controls, breadcrumb, and network
status within the viewport. The bottom-right status retains its semantic
colors and the contextual left footer no longer exposes inactive page
shortcuts. The older 106×32 running release process had ProcDump output over
rows 9–17; it was left untouched. The watchdog now discards the child
process's standard streams, but a new dump was not triggered for this audit.
The isolated run had no real gateway, active connections, or quality history,
so connected/authentication/progress states and populated tables/charts remain
covered by `TestBackend` rather than live terminal captures.

## Decisions and residual differences

- Connections and Node Quality now reserve the bottom status row, matching
  their Figma frames. Their close and scroll controls remain inside the dialog.
  At 80×24, Node Quality shortens evidence labels so the newest failure value
  stays readable within its one-row evidence viewport. Short labels also keep
  the initial evidence value visible without scrolling.
- Internet, Private Access, and the node dashboard share muted `GLOBAL NET`,
  state-colored network status, and transfer-colored rates. Focus uses the
  shared cyan token; ordinary boundaries stay subdued, while dialog borders
  use the focus token.
- Workspace switching is an explicit Command Palette action. Tab changes
  Internet pane focus, so the old `[Tab] Intranet` footer and palette badges
  were removed. Unconfigured Private Access shows one settings hint instead
  of a duplicate. Modal footers display only actions that work in that modal.
- The terminal uses whole cells: borders occupy a row and column; CJK and
  emoji use display width and grapheme-safe clipping. Full layouts retain
  outer gutters and panel hierarchy; compact layouts prioritize the current
  route and global charts, then expose omitted detail via `c` and `i`.
- The first-launch workspace choice and returning notice in `953:3` and
  `953:25` are design references, not shipped pages. The shipped first-run
  subscription URL wizard has no current Figma frame. Startup currently
  restores the saved workspace or enters the configured fallback without a
  notice. These are known product differences, not inferred implementations.
- Authentication fields are supplied by the gateway. The sample credentials,
  demo code, and explanatory mock text in the Figma states are intentionally
  absent from the production dialog. The runtime shows progress events and
  errors rather than copying the frame's simulated session transcript.
- Actual node names, measurements, connection rows, routes, and network
  status replace Figma sample data. Missing history remains a gap and no
  quality measurement is presented as traffic usage.
