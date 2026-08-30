# Usability probe manifests

Usability probe manifests add application-specific node-view tabs to the TUI. Discovery only
validates manifests and creates an untested tab; it never sends a request or starts a program.

By default, place JSON manifests in `usability-probes` beside the active sing-box configuration.
Set `SING_BOX_TUI_USABILITY_PROBE_DIR` to use a different directory. IDs are stable, lowercase
ASCII identifiers and must be unique. Labels are the tab names shown to users.

## URL probes

```json
{
  "id": "github-web",
  "label": "GitHub Web",
  "ranking": "low-latency",
  "url": "https://github.com/"
}
```

`ranking` is one of `balanced`, `low-latency`, or `throughput`. A URL manifest must contain an
HTTPS URL and must not contain `executable` or `args`. Current sing-box Delay Endpoint handlers
silently replace plain HTTP targets with their built-in default test URL, so the TUI rejects
`http://` manifests rather than reporting a result for the wrong target.

When the user presses `U` on this tab, the TUI calls the running sing-box Clash
`/proxies/{outbound}/delay` endpoint once for every named outbound in the current selector. The
named outbound is passed directly; the live selector is not changed. Any valid HTTP response from
the target URL is usable, regardless of its status code. Delay Endpoint 503 and 504 responses mean
that individual node is not usable. The client waits longer than the target deadline for Clash to
produce that response; a stalled/unreachable controller, authentication failure, missing route,
or other unexpected status leaves the whole run incomplete and preserves the previous complete
panel. URL probes do not start a child program or create a node runtime.

## Executable probes

```json
{
  "id": "agy-gemini",
  "label": "Agy Gemini",
  "ranking": "balanced",
  "executable": "./agy-gemini-probe",
  "args": ["--profile", "default"]
}
```

An executable manifest requires an explicit `args` array, including when it is empty, and must not
contain `url`. Relative executable paths are resolved from the manifest directory. The TUI starts
the executable directly with its argument array; it never invokes a shell. Metacharacters in an
argument therefore remain literal data.

The probe program owns application-specific requests and pass/fail decisions. If it needs isolated
per-node proxies, the program starts and controls `sing-box-tui node-runtime-manager --stdio`
itself. The TUI does not create that runtime on the program's behalf.

The program writes progressive JSON Lines records to stdout:

```json
{"type":"node_result","node":"Hong Kong 01","usable":true,"detail":"request accepted"}
{"type":"node_result","node":"US 02","usable":false,"detail":"application rejected"}
{"type":"summary","complete":true,"message":"all candidates assessed"}
```

Each selector node may appear at most once. Exactly one `summary` record terminates the protocol,
and no node records may follow it. A complete summary is required before results replace the
panel's prior complete run. An incomplete run and malformed output retain their diagnostic history
but do not replace the last complete panel.

Stdout is limited to 64 KiB per line and 4 MiB total. The TUI continuously drains stderr but
retains only a bounded 16 KiB diagnostic prefix. These bounds keep a faulty probe from growing the
TUI's memory without limit. Detail, summary, stderr, and manifest diagnostic text is normalized to
a printable single line before it is stored or rendered, so control characters cannot alter the
terminal display.

## TUI behavior

- Use Left and Right while the candidate pane is focused to navigate built-in and custom tabs.
- Press `U` on a custom tab to start its manual probe.
- Progressive results and terminal status appear in the status area.
- A complete custom tab shows only usable results that are also members of the selector snapshot
  used for the run. Results for other selectors are never published into the tab.
- Press `i` on any Current selector node to view all available custom-criterion facts, including
  rejected nodes that correctly do not appear as custom-tab candidates. Use `j`/`k` to scroll a
  long evidence list.

Invalid manifests are skipped and reported with bounded, actionable diagnostics during startup.
Press `?`, then select each invalid-manifest row to inspect every reported path and reason. Fix the
reported files and restart the TUI to rediscover them.
