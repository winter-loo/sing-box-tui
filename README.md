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

List selector groups through the Clash API:

```bash
cargo run -- selectors
cargo run -- selectors --selector select
```

Inspect live controller status, traffic, and active connections:

```bash
cargo run -- status
```

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
- With `--import-replace-nodes`, the importer removes existing node outbounds first, then inserts the newly imported nodes.
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

## Benchmark Nodes

The former Python skill script is now built into the Rust app.

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
- `Enter`: unused for selection
- `b`: asynchronously benchmark all nodes in the current selector/group using the current filter
- `t`: asynchronously benchmark only the currently highlighted node (with a light same-node debounce to avoid spammy rapid retests)
- `s`: toggle the visible benchmark view mode between `FILTER VIEW` and `LATENCY SORT`; the active mode is shown in the pane titles/status, and latency sort hides failed-tested nodes while sorting successful tested nodes by ascending latency
- `v`: run Google/GitHub verification checks
- `V`: run Google/GitHub/Discord verification checks
- `/`: change the benchmark substring filter
- `r`: refresh groups
- `q`: quit

During async benchmarks, node rows show a brighter pending state (`...` plus a spinner marker) while a test is in progress, then show measured latency or `fail` when the test completes.
