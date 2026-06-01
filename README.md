# sing-box-tui

## Provider sync

`sync` logs into a provider website, downloads the sing-box subscription JSON, and merges provider nodes into your local sing-box config.

Example:

```bash
cargo run -- sync \
  --provider https://3.airtcp.me \
  --account-file ./provider-account.txt \
  --config /etc/sing-box/config.json \
  --subscription-output ./output/airtcp-singbox.json

cargo run -- sync \
  --provider https://3.airtcp.me \
  --account-file ./provider-account.txt \
  --config /etc/sing-box/config.json \
  --output ./output/merged-config.json

cargo run -- sync \
  --provider https://3.airtcp.me \
  --account-file ./provider-account.txt \
  --config /etc/sing-box/config.json \
  --write
```

`sync` is safe by default: it will not overwrite the live config unless you pass `--write`. Otherwise, use `--output` to write the merged config somewhere else.

If you already have a direct sing-box subscription URL, use `subscribe` instead of `sync`:

```bash
cargo run -- subscribe \
  --url 'https://h.bbydy.org/api/bby/client/subscribe?token=REDACTED' \
  --config ./config.json \
  --output ./output/merged-config.json \
  --replace-nodes
```

`subscribe` fetches the URL with a `sing-box` user agent, extracts real node outbounds from the subscription JSON, filters provider metadata entries, and merges the nodes into the selector/urltest groups from the template config.

Account file formats:

```text
email=your-email@example.com
password=your-password
```

or:

```text
your-email@example.com
your-password
```

Small terminal UI for switching `sing-box` selector outbounds through the Clash-compatible controller API.

The TUI keeps the original two-column layout when the selected selector has zero or one nested provider selector. When the selected selector contains multiple child selector groups, it switches to a three-column layout:

```text
Selector Groups | Providers | Nodes
```

Selecting a node inside a provider group updates that provider selector and then points the parent selector at the provider. This supports configs shaped like `手动选择 -> 宝贝云 -> node`.

## Requirements

- `sing-box` must expose `experimental.clash_api.external_controller`, usually `127.0.0.1:9090`
- The proxies you want to switch must be under at least one `selector` outbound
- Keep `experimental.cache_file.enabled = true` if you want `sing-box` to remember the last selection

Example config fragment:

```json
{
  "experimental": {
    "cache_file": {
      "enabled": true
    },
    "clash_api": {
      "external_controller": "127.0.0.1:9090",
      "secret": ""
    }
  }
}
```

## Run

```bash
cargo run -- run
```

Or point it at a different controller:

```bash
cargo run -- run --controller http://127.0.0.1:9090
```

Inside the TUI, use `/` to set a node-name filter such as `美国` or `美国,香港`, then press `a` to enable auto-pick for the selected selector group. Auto-pick benchmarks the filtered nodes every 30 seconds and switches to the best healthy node only when the current node is outside the filter, fails, or is above 600ms. It does not rewrite the sing-box `urltest` outbound; it switches the selector to a concrete node through the controller API.

TUI benchmark results are written to SQLite at `./singbox.sqlite3` by default. Set `SING_BOX_TUI_DB=/path/to/singbox.sqlite3` to use a different database. Rows are stored in `benchmark_results` with timestamp, selector, node, filter, latency in milliseconds, completion state, and benchmark kind (`group`, `single`, or `auto`).

TUI runtime state is written to `./sing-box-tui.json` by default. Set `SING_BOX_TUI_CONFIG=/path/to/sing-box-tui.json` to use a different file. The state file records the last benchmark filter, whether auto-pick is enabled, and the current selected node for each selector group.

TUI bypass entries are stored in that same state file and written to a sing-box source rule-set at `./sing-box-tui-bypass.json` by default. Set `SING_BOX_TUI_BYPASS_RULE_SET=/path/to/sing-box-tui-bypass.json` to use a different file. Generated and merged configs reference this local rule-set near the top of `route.rules`, routing matched domains/IPs/CIDRs to `direct` / `国内直连`. If an older live config does not yet reference the rule-set, regenerate/merge the config and restart or reload sing-box once; after that, the local rule-set file can be edited by the TUI and sing-box will reload it.

Generated and merged configs set selector/urltest `interrupt_exist_connections` to `false`, so switching nodes does not tear down existing connections. Existing connections keep their original outbound until they close or fail; new/retried connections use the current selection.

List selector groups through the Clash API:

```bash
cargo run -- selectors
cargo run -- selectors --selector select
```

Inspect live controller status, traffic, and active connections:

```bash
cargo run -- status
```

The TUI also shows active sing-box connection totals in the status area. Press `c` to open a live connection details panel showing the inbound type, target host/IP, outbound chain, and matched rule.

Environment variable still works too:

```bash
SING_BOX_CONTROLLER=http://127.0.0.1:9090 cargo run -- run
```

If the controller has a secret:

```bash
SING_BOX_SECRET='your-secret' cargo run
```

## Import Clash Nodes

Full `config.json` output:

```bash
cargo run -- import -i clash_proxies.txt -o config.json
```

Replace existing node outbounds instead of merging:

```bash
cargo run -- import -i clash_proxies.txt -o config.json --replace-nodes
```

Full-config behavior:

- If `/etc/sing-box/config.json` does not exist, the importer creates a complete config with sane defaults:
  - mixed inbound on `127.0.0.1:5780`
  - `selector` outbound `select`
  - `urltest` outbound `auto`
  - `direct` and `block`
  - `route.final = "select"`
  - `experimental.clash_api` on `127.0.0.1:9090`
- If `/etc/sing-box/config.json` exists, the importer reads it and merges the imported nodes into that config by default.
- With `--replace-nodes`, the importer removes existing node outbounds first, then inserts the newly imported nodes.
- Existing `select`, `auto`, `direct`, and `block` outbounds are reused when present.
- Imported node tags replace same-tag outbounds and are appended otherwise.

Use a different source config path if needed:

```bash
cargo run -- import -i clash_proxies.txt --config ./config.json -o merged-config.json
```

Validate the generated config:

```bash
sing-box check -c config.json
```

## Manual Bypass Migration

Use this when you want to update an older `sing-box` config by hand so the TUI can manage direct-bypass domains, IPs, and CIDRs.

Add this local source rule-set to `route.rule_set`:

```json
{
  "type": "local",
  "tag": "sing-box-tui-bypass",
  "format": "source",
  "path": "sing-box-tui-bypass.json"
}
```

Add this route rule near the top of `route.rules`, after any DNS hijack rule and before normal proxy/direct rules:

```json
{
  "rule_set": "sing-box-tui-bypass",
  "outbound": "direct"
}
```

If your direct outbound tag is `国内直连`, use that instead:

```json
{
  "rule_set": "sing-box-tui-bypass",
  "outbound": "国内直连"
}
```

Create the rule-set file at the configured `path`:

```json
{
  "version": 1,
  "rules": []
}
```

Example with entries:

```json
{
  "version": 1,
  "rules": [
    {
      "domain_suffix": ["example.com", "github.com"]
    },
    {
      "ip_cidr": ["1.1.1.1", "10.0.0.0/8"]
    }
  ]
}
```

Validate and restart once:

```bash
sing-box check -c /etc/sing-box/config.json
sudo systemctl restart sing-box
```

After the config references the local rule-set, press `B` in the TUI to edit bypass entries. The TUI writes `sing-box-tui-bypass.json`; new or retried connections use the updated direct-bypass rules.

## Benchmark Nodes

The former Python skill script is now built into the Rust app.

For a manual provider-subscription workflow, including fetching a sing-box subscription JSON, converting legacy config syntax for local testing, benchmarking every node through the Clash API, and verifying real traffic, see [docs/subscription-benchmark.md](docs/subscription-benchmark.md).

CLI examples:

```bash
cargo run -- benchmark
cargo run -- benchmark --max-concurrency 8
cargo run -- benchmark --selector select --match 美国 --switch
cargo run -- benchmark --match 美国 --switch --verify
cargo run -- benchmark --match 美国 --switch --verify --verify-discord
cargo run -- run --max-concurrency 8
```

If `--match` is omitted, benchmarking runs without a substring filter. If `--max-concurrency` is omitted, benchmarks use a default cap of 16 concurrent delay probes. The same limit applies to CLI benchmarking and TUI group benchmarks started with `b`.

JSON output includes:

- current selector target
- tested candidates
- per-node delay values
- best successful node
- whether a switch was applied
- final selected node
- optional verification summary

## Clash API Inspection

Two read-only controller commands are available in addition to the TUI and benchmarking flow:

- `selectors`: returns JSON for all selector groups, or one group with `--selector NAME`
- `status`: returns controller version, current traffic counters, aggregate connection totals, and active connection metadata

## Keys

- `Up` / `Down` or `j` / `k`: move
- `Tab`, `h`, `l`, `Left`, `Right`: switch pane
- `Space`: apply/switch to the currently highlighted proxy in the current selector group
- `B`: edit direct-bypass domains, IPs, and CIDRs; values are comma-separated and are written to the local sing-box rule-set
- `Enter`: unused for selection
- `b`: asynchronously benchmark all nodes in the current selector/group using the current filter
- `t`: asynchronously benchmark only the currently highlighted node (with a light same-node debounce to avoid spammy rapid retests)
- `s`: toggle node sort order between `SELECTOR ORDER` and `LATENCY ORDER`; latency order hides failed-tested nodes and sorts successful tested nodes by ascending latency
- `a`: toggle runtime auto-pick using the current filter; it benchmarks every 30 seconds and switches only when current latency is above 600ms, failed, or outside the filter
- `i`: show a SQLite-backed latency line chart for the highlighted node; x-axis is relative time in minutes or hours and y-axis is latency in ms. The chart refreshes from SQLite while open. Failed benchmark records are treated as gaps, so no point is drawn and the line breaks there.
- `z` / `Z`: while the latency chart is open, zoom in to the most recent values or zoom out to include less recent values
- `c`: show active sing-box connections, including inbound type, destination, outbound chain, and route rule; press `r` in this panel to refresh immediately
- `v`: run Google/GitHub verification checks
- `V`: run Google/GitHub/Discord verification checks
- `/`: change the benchmark substring filter; comma-separated values match any value, for example `美国,香港`
- `r`: refresh groups
- `q`: quit

During async benchmarks, node rows show a brighter pending state (`...` plus a spinner marker) while a test is in progress, then show measured latency or `fail` when the test completes.
