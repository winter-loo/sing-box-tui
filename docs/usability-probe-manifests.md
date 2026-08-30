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
  "background": true,
  "interval_seconds": 900,
  "ttl_seconds": 3600,
  "timeout_seconds": 60,
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

`background` defaults to `false`. When true, `interval_seconds` may override the safe 900-second
default schedule. This is only manifest permission: the TUI still will not schedule the criterion
until the user selects that custom tab and presses `P`. The explicit authorization is stored for
the stable manifest ID and selected selector. Enabling auto-pick with `a` does not authorize a
custom probe or consume its application quota. Press `P` again to revoke the authorization.

`ttl_seconds` is optional. Without it, a complete result follows each unchanged node
configuration's lifetime. With it, the expiry timestamp is frozen when the complete run is
published, so later manifest edits cannot revive expired evidence. Expired facts remain visible
in detail but cannot enter custom-panel or automatic-selection candidates. `timeout_seconds`
bounds the whole program and defaults to 600 seconds.

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

Invalid JSON Lines, authentication failure, unexpected exit, timeout, cancellation, and runtime
failure all make the new run incomplete. A prior complete result is retained only while unexpired,
and the newer failed attempt remains visible in panel/detail state.

Stdout is limited to 64 KiB per line and 4 MiB total. The TUI continuously drains stderr but
retains only a bounded 16 KiB diagnostic prefix. These bounds keep a faulty probe from growing the
TUI's memory without limit. Detail, summary, stderr, and manifest diagnostic text is normalized to
a printable single line before it is stored or rendered, so control characters cannot alter the
terminal display.

## TUI behavior

- Use Left and Right while the candidate pane is focused to navigate built-in and custom tabs.
- Press `U` on a custom tab to start its manual probe.
- Press `P` on a custom tab to explicitly enable or disable its permitted background schedule for
  the selected selector. This permission is independent from auto-pick.
- Progressive results and terminal status appear in the status area.
- A complete custom tab shows only usable results that are also members of the selector snapshot
  used for the run. Results for other selectors are never published into the tab.
- Press `i` on any Current selector node to view all available custom-criterion facts, including
  rejected nodes that correctly do not appear as custom-tab candidates. Use `j`/`k` to scroll a
  long evidence list.

Invalid manifests are skipped and reported with bounded, actionable diagnostics during startup.
Press `?`, then select each invalid-manifest row to inspect every reported path and reason. Fix the
reported files and restart the TUI to rediscover them.

## Agy Gemini example panel

The [Unix manifest](../examples/usability-probes/unix/agy-gemini.json) and
[Windows manifest](../examples/usability-probes/windows/agy-gemini.json) wire the existing Agy
adapter into a custom **Agy Gemini** panel. Both use `balanced` network ranking and declare no
background schedule, so they run only when the user presses `U`. Copy only the manifest for the
current platform into the active `usability-probes` directory.

On Unix, keep the manifest's executable path relative to `scripts/agy-gemini-node-probe.py` or
update it for the installed layout, and make the adapter executable if the checkout did not
preserve its executable bit. Windows does not interpret Python shebangs, so its manifest invokes
`python.exe` directly without a shell; replace both `C:\\Path\\To` placeholders with absolute
paths to the interpreter and checkout before registration.

The adapter's `--tui-jsonl` mode keeps stdout reserved for the progressive protocol. It publishes
only the node tag, a successful result, and bounded timing; it never publishes the Agy response,
account, project, prompt, environment, proxy URL, configuration path, or manager diagnostic text.
Only a zero-exit real `agy --agent gemini --print ...` invocation emits `usable:true`. Authentication
requirements, non-zero process exits, timeouts, spawn failures, and isolated-runtime failures stop
the assessment with `complete:false`; they do not manufacture `usable:false` facts. Consequently,
the TUI preserves the previous complete panel instead of blaming a node for an infrastructure or
account failure.

Automated coverage uses local fixture manager and Agy executables and redirects all proxy variables
to an unbound loopback port. It needs no account, quota, or public network access:

```sh
python3 -m unittest scripts.tests.test_agy_gemini_node_probe
```

### Optional authenticated manual smoke

This smoke test consumes a real authenticated Agy Gemini request and may use account quota. Run it
only after confirming the intended account and quota policy. It uses `node-runtime-manager`, so the
request travels through an isolated probe runtime and does not switch the live selector:

```sh
./scripts/agy-gemini-node-probe.py \
  --manager ./target/debug/sing-box-tui \
  --agy /absolute/path/to/agy \
  --config /absolute/path/to/config.json \
  --sing-box /absolute/path/to/sing-box \
  --limit 1
```

First run one interactive authenticated `agy --agent gemini` request if the CLI requests login.
The standalone `--limit 1` command intentionally keeps its human-readable report mode; remove the
limit and let the TUI launch `--tui-jsonl` only when a complete panel assessment is desired. Review
the isolated result file, then delete it if its node tags are sensitive in your environment.
