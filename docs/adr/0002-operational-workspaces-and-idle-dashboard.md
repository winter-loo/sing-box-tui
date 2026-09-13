---
status: accepted
---

# Separate operational workspaces from the idle dashboard

The TUI separates operational workspaces from a passive monitoring destination. The dashboard follows the current route node rather than the browsed candidate, so inspecting a candidate cannot change either the live route or the subject of route monitoring. This records the agreed navigation boundaries for the upcoming refactor, not completed implementation.

The Figma implementation handoff at https://www.figma.com/design/jGcpW9esdjzunUpuU3Aqu5?node-id=1014-2 defines the operational entry model: the Internet workspace opens its node list, and returning launches restore the persisted proxy workspace. If the remembered workspace is removed, enter the sole remaining configured workspace; if none remains, enter configuration onboarding. Restoring a page must not connect a VPN or switch an Internet Proxy node.

After 30 seconds without user input on ordinary browsing pages, enter the idle dashboard. Pause this transition during settings editing, subscription input, and VPN authentication, including waiting for a code. Restart the timer after leaving those interactions. Background probes do not block the transition.

The dashboard is a directly operable destination, not a screen saver with a swallowed wake-up key. Its shortcuts execute immediately, unbound keys do not navigate, and closing its dialogs returns to the dashboard. The navigation menu opens operational pages; arbitrary input does not restore the previous page. This avoids introducing a second interpretation of the same shortcut solely because an idle timer elapsed.

Use 120 columns by 30 rows as the full dashboard design target. When terminal space cannot accommodate all content, prioritize the current-route summary and aggregate latency and upload/download charts. Hide the active-connections list first, then the per-node quality chart as necessary; c and i continue to provide their corresponding details. Exact layout breakpoints must be determined from terminal-cell budgets and readability checks rather than copied from Figma pixel coordinates.

Figma Terminal Contract frame 874:2 defines the minimum supported size as 80 columns by 24 rows. Below either limit, display a resize notice and retain quit/help instead of squeezing unreadable charts into the available cells.

Private Access profile navigation and connection actions retain existing code behavior. Changing the focused profile does not disconnect another profile. The V action operates on the focused profile: connected or connecting requests disconnection; disconnecting waits; disabled, disconnected, or error requests connection. The session layer does not impose a cross-profile single-connection gate or automatically disconnect another profile before connecting the focused one. Existing backend validation, resource constraints, and background-session ownership checks still apply; this is not a guarantee that arbitrary VPN combinations can operate concurrently. Do not introduce a new connection-conflict dialog as part of this UI refactor.

The replacement of the permanent selector sidebar described in ADR 0001 follows the Figma handoff; ADR 0001's node-view membership and automatic-selection contracts remain in force.
