# Usability probe manifests

Usability probe manifests add application-specific node-view tabs to the TUI. Discovery only
validates manifests and creates an untested tab; it never sends a request or starts a program.

By default, place JSON manifests in `usability-probes` beside the active sing-box configuration.
Set `SING_BOX_TUI_USABILITY_PROBE_DIR` to use a different directory. IDs are stable, lowercase
ASCII identifiers and must be unique. Labels are the tab names shown to users.

## Bundled probes and visibility

The executable always includes two custom probes; neither needs Python or a separately installed
adapter:

- `streaming` is visible by default and uses throughput ranking. It runs an HTTPS prefilter and
  then a bounded 512 KiB transfer through each surviving node's isolated runtime.
- `github-ssh` is hidden by default and uses low-latency ranking. It first checks GitHub HTTPS,
  then sends `CONNECT github.com:22` through the isolated runtime and requires a real `SSH-`
  protocol banner.
- `agy-gemini` is hidden by default and uses balanced ranking. It applies a hard two-second
  ordinary-connectivity prefilter through the isolated runtime, then runs a real
  `agy --agent gemini` request only for nodes that pass. Ordinary reachability is never treated as
  Agy Gemini usability.

A user manifest with one of those stable IDs replaces the bundled manifest. Use `builtin` to keep
the in-process implementation while changing presentation or scheduling. For example, this makes
the GitHub SSH tab visible:

```json
{
  "id": "github-ssh",
  "label": "GitHub SSH",
  "ranking": "low-latency",
  "builtin": "github-ssh",
  "visible": true
}
```

This hides Streaming while keeping it registered and configurable:

```json
{
  "id": "streaming",
  "label": "Streaming",
  "ranking": "throughput",
  "builtin": "streaming",
  "visible": false
}
```

This shows the bundled Agy Gemini tab:

```json
{
  "id": "agy-gemini",
  "label": "Agy Gemini",
  "ranking": "balanced",
  "builtin": "agy-gemini",
  "visible": true
}
```

The built-in Agy probe resolves the `agy` executable from `PATH`. Set
`SING_BOX_TUI_AGY_EXECUTABLE` to an absolute executable path when it is installed elsewhere. Its
authenticated CLI state remains owned by Agy; the TUI does not collect credentials.

`visible` defaults to `true` for user manifests. A manifest must declare exactly one source:
`url`, `executable`, or `builtin`. The supported `builtin` values are `streaming` and
`github-ssh`, and `agy-gemini`. Replacing a bundled ID with `url` or `executable` replaces its
implementation too.

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
{"type":"progress","message":"checking TCP 22","node":"Hong Kong 01"}
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

- Use Left and Right while the candidate pane is focused to navigate visible usability tabs.
- Press `U` on an active usability tab, including Streaming, to start its manual probe.
- Press `P` on an active usability tab to explicitly enable or disable its permitted background schedule for
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

## Agy Gemini built-in probe and legacy adapter

The Agy Gemini implementation is bundled in the executable and hidden by default; the `builtin`
manifest above is the shortest way to show it. It uses
`http://www.gstatic.com/generate_204` only for the hard two-second ordinary-connectivity screen.
Every node admitted to the panel must subsequently complete the real authenticated Agy Gemini
command through that node's isolated proxy.

The [Unix manifest](../examples/usability-probes/unix/agy-gemini.json) and
[Windows manifest](../examples/usability-probes/windows/agy-gemini.json) remain as examples of
replacing the built-in ID with the external Python adapter. Both explicitly make the tab visible,
use `balanced` network ranking, and declare no background schedule. Copy one only when that
external-program override is intentional.

On Unix, keep the manifest's executable path relative to `scripts/agy-gemini-node-probe.py` or
update it for the installed layout, and make the adapter executable if the checkout did not
preserve its executable bit. Windows does not interpret Python shebangs, so its manifest invokes
`python.exe` directly without a shell; replace both `C:\\Path\\To` placeholders with absolute
paths to the interpreter and checkout before registration.

The legacy adapter's `--tui-jsonl` mode keeps stdout reserved for the progressive protocol. It publishes
only the node tag, a successful result, and bounded timing; it never publishes the Agy response,
account, project, prompt, environment, proxy URL, configuration path, or manager diagnostic text.
Both implementations emit `usable:true` only after a zero-exit real
`agy --agent gemini --print ...` invocation. In the built-in probe, Agy's explicit
`User location is not supported for the API use` response rejects only the current node and the
scan continues. Authentication requirements, timeouts, spawn failures, isolated-runtime failures,
and unclassified non-zero exits still stop the assessment with `complete:false`; the Probe error
panel includes the bounded underlying Agy error, and the TUI preserves the previous complete panel.
The transient Agy startup message `not authenticated, trying silent auth` is not an authentication
failure by itself; the final process outcome and terminal API error decide the result.
The legacy external adapter conservatively treats every non-zero Agy exit as an incomplete run.

Automated coverage uses local fixture manager and Agy executables and redirects all proxy variables
to an unbound loopback port. It needs no account, quota, or public network access:

```sh
python3 -m unittest scripts.tests.test_agy_gemini_node_probe
```

`progress` records are optional, may appear before or between node results, and update the active
panel's progress box immediately. Programs should emit one before any potentially slow startup or
prefilter operation so a zero-result run remains visibly active. Progress messages are transient
and are not published as node facts. When `node` is present and belongs to the selector snapshot,
the active panel shows that node as pending. A later usable node result keeps it in the panel; a
rejected result removes it. Set `candidate` to `false` when a named preliminary check should update
the progress box without putting that node into the candidate panel; the field defaults to `true`.
Programs with multiple stages may also attach a `progress` object containing
`stage_one_completed`, `stage_one_total`, `stage_two_completed`, `stage_two_total`, `accepted`, and
optional `stage_one_label` / `stage_two_label` strings. The older GitHub-specific field names
`https_scanned`, `https_total`, `tcp_completed`, and `tcp_total` remain accepted as aliases. The
active panel renders those explicit counters instead of deriving a misleading denominator from the
selected selector's member count.
Executable probes receive the active selector's ordered node tags as a JSON array in
`SING_BOX_TUI_USABILITY_CANDIDATES`. Bundled probes use this scope so progress totals, pending rows,
accepted rows, and the selector backing the candidate panel always describe the same node set.

## GitHub SSH built-in probe and legacy adapter

The GitHub SSH implementation is now bundled in the executable and hidden by default; the
`builtin` manifest above is the shortest way to show it. The older
[Unix manifest](../examples/usability-probes/unix/github-ssh.json) and
[Windows manifest](../examples/usability-probes/windows/github-ssh.json) remain as examples of
replacing the built-in ID with the external `github-ssh-node-probe.py` adapter. Copy one only when
that external-program override is intentional, and replace the Windows path placeholders when
applicable.

For every node that passes the existing `https://github.com/` reachability prefilter, the adapter
starts from the isolated runtime's local HTTP proxy, sends `CONNECT github.com:22`, and waits for a
real `SSH-` protocol banner. It does not change the live selector and does not read or set proxy
environment variables. A timeout, rejected CONNECT, early close, or non-SSH response excludes only
that node. Manager and runtime infrastructure failures leave the run incomplete.

The offline contract test uses a local CONNECT fixture and needs no GitHub access:

```sh
python3 -m unittest scripts.tests.test_github_ssh_node_probe
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
