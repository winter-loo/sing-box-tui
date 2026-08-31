# sing-box-tui

Terminal UI for managing sing-box selector nodes, node-quality assessments,
subscription refresh, bypass rules, and OS system proxy settings.

![Main screen](docs/assets/main-screen.svg)

## Quick start on Windows

Install the latest prebuilt Windows release:

```powershell
irm https://raw.githubusercontent.com/winter-loo/sing-box-tui/main/scripts/windows/install.ps1 | iex
```

If raw GitHub access is blocked, fetch the installer through the proxy URL:

```powershell
irm https://deeloo.cn/anywhere/https://raw.githubusercontent.com/winter-loo/sing-box-tui/main/scripts/windows/install.ps1 | iex
```

The installer downloads the latest `sing-box-tui` Windows x64 release with
multi-part parallel downloads when the server supports byte ranges. It installs
under `%LOCALAPPDATA%\sing-box-tui`, adds that directory to the user `PATH`, and
installs `sing-box` from the configured GitHub release when it is missing. If a
direct GitHub download fails, the installer retries through the configured
`https://deeloo.cn/anywhere` proxy.

From a local checkout, you can run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install.ps1 -AddToPath
```

Use `-DownloadParts 1` to disable parallel downloading. Disable the GitHub
proxy fallback with `-GitHubProxy ""`, override it with another prefix URL,
or use `-ForceGitHubProxy` to route all GitHub requests through it without a
direct attempt:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install.ps1 -DownloadParts 1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install.ps1 -GitHubProxy ""
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install.ps1 -GitHubProxy "https://example.com/anywhere"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install.ps1 -ForceGitHubProxy
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install.ps1 -SingBoxDir "D:\Tools\sing-box"
```

Start the TUI:

```powershell
sing-box-tui run
```

By default, `run` finds `sing-box` through `PATH`. To use a core installed
elsewhere or with a custom filename, specify its executable path:

```powershell
sing-box-tui run --sing-box "D:\Tools\sing-box\sing-box.exe"
```

On first launch, the TUI shows a setup wizard. Paste a sing-box subscription URL
to create `.suburl`, or press `s` to skip. Press `o` later to open TUI settings.

## Prebuilt releases

Tagged releases build these archives automatically:

- Windows x64: `sing-box-tui-<version>-x86_64-pc-windows-msvc.zip`
- Linux x64: `sing-box-tui-<version>-x86_64-unknown-linux-gnu.tar.gz`
- macOS Apple Silicon: `sing-box-tui-<version>-aarch64-apple-darwin.tar.gz`
- macOS Intel: `sing-box-tui-<version>-x86_64-apple-darwin.tar.gz`

## Internet Route Source Sync

`sync` logs into an Internet Route source website, downloads the sing-box subscription JSON, and merges route nodes into your local sing-box config.

Example:

```bash
cargo run -- sync \
  --provider https://3.airtcp.me \
  --account-file ./provider-account.txt \
  --config ./config.json \
  --subscription-output ./output/airtcp-singbox.json

cargo run -- sync \
  --provider https://3.airtcp.me \
  --account-file ./provider-account.txt \
  --config ./config.json \
  --output ./output/merged-config.json

cargo run -- sync \
  --provider https://3.airtcp.me \
  --account-file ./provider-account.txt \
  --config ./config.json \
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

`subscribe` fetches the URL with a `sing-box` user agent, extracts real node outbounds from the subscription JSON, filters source metadata entries, and merges the nodes into the selector/urltest groups from the template config.

Generated default configs omit remote `geoip-cn`, `geosite-*`, and
`AdGuardSDNSFilter` rule-sets by default so startup does not depend on fetching
GitHub-hosted `.srs` files. They also omit TUN mode by default. Add
`--include-geosite-rules` to `import`, `subscribe`, `subscriptions`, `sync`, or
`run` if you explicitly want those remote rules in a newly created default
config. Add `--include-tun-mode` if you explicitly want a TUN inbound in a newly
created default config. Existing configs keep whatever rule-sets and inbounds
they already contain.

To import one or more Internet Route source subscription URLs, copy the sing-box subscription
URL from the source website and save it in a local `.suburl` file. Each line
uses `source name = subscription url`:

```text
baobeiyun = https://example.com/api/subscribe?token=REDACTED
airtcp = https://spring.mailrelay.us/link/REDACTED?singbox=1
```

Then refresh those subscriptions into a config:

```bash
cargo run -- subscriptions \
  --input .suburl \
  --cache .suburl.cache.json \
  --config ./config.json \
  --output ./output/refreshed-config.json
```

Keep `.suburl` private because source subscription URLs usually contain
account tokens.

Source helper scripts under `scripts/` can extract subscription URLs from an
already-authenticated Chrome tab through CDP. When Chrome runs on Windows and
the script runs inside WSL, start a separate Windows Chrome profile with CDP
enabled:

```powershell
& "$env:ProgramFiles\Google\Chrome\Application\chrome.exe" `
  --user-data-dir="$env:TEMP\chrome-cdp-profile-9229" `
  --remote-debugging-port=9229 `
  --remote-allow-origins=* `
  --new-window "https://5.airtcp.me/user"
```

On WSL2, current Chrome builds may still bind CDP to Windows `127.0.0.1` only.
Use `--cdp-windows-relay` to start a temporary PowerShell TCP relay from the
WSL-visible Windows host to that loopback CDP port:

```bash
python3 scripts/get-airtcp-singbox-url.py --cdp-windows-relay --list-pages-only
python3 scripts/get-airtcp-singbox-url.py --cdp-windows-relay
python3 scripts/get-baipiao-singbox-url.py --cdp-windows-relay
python3 scripts/get-baobeiyun-singbox-url.py --cdp-windows-relay
```

If Chrome is already listening on a WSL-reachable Windows address, use
`--cdp-windows` without the relay. Both modes resolve the Windows host IP from
WSL, rewrite loopback debugger WebSocket URLs when needed, and bypass shell
proxy variables for CDP HTTP calls. If WSL chooses the wrong host, pass it
explicitly:

```bash
python3 scripts/get-airtcp-singbox-url.py \
  --cdp-windows-relay \
  --windows-host "$(ip route show default | awk '{ print $3; exit }')"
```

You can also set `SING_BOX_TUI_CDP_URL` to change the default CDP endpoint. Keep
the CDP browser profile temporary and close that Chrome instance after use; a
reachable CDP port can control the attached browser profile.

Set `WSL_CDP_LOG=1` to print CDP helper diagnostics such as host resolution,
URL rewrites, and relay start/stop events.

For scheduled or repeated refreshes, use the same `subscriptions` command. It
refreshes sources at most once per day by default and updates only server node
outbounds in an existing config:

```bash
cargo run -- subscriptions \
  --input .suburl \
  --cache .suburl.cache.json \
  --config ./config.json \
  --output ./output/refreshed-config.json
```

Use `--write` instead of `--output` to overwrite `--config` in place, and `--force` to fetch even when the cached subscription payload is still fresh. The command stores downloaded subscription JSON in `.suburl.cache.json` so skipped daily runs can still refresh node outbound definitions from cached source configs. DNS, inbounds, routes, selectors, experimental settings, and other non-node config sections are preserved.

When the TUI is running, it also starts a background subscription refresh worker if `.suburl` exists. The worker runs once on startup, then checks again every day. It refreshes node outbounds in the configured sing-box config path and keeps the TUI responsive while network fetches are running:

```bash
cargo run -- run --config ./config.json
```

Set `SING_BOX_CONFIG=/path/to/config.json` or pass `--config` to control the write target. Use `--no-subscription-refresh` to disable the TUI worker and `--force-subscription-refresh` to fetch on startup even when the cache is fresh. After the worker writes a new config, restart or reload sing-box for the new nodes to become active in the running service. If that refresh added, removed, or materially changed a node, node-quality persistence is paused in the current TUI so results from the still-running old config cannot be assigned to the new node identity. Reload sing-box and restart the TUI to resume it against the committed config.

Press `u` in the TUI to manually force-refresh subscription contents immediately. If a refresh is already running, the TUI keeps the existing worker and reports that the refresh is in progress.

Before overwriting the sing-box config, subscription refresh writes one fixed backup next to it: `<config filename>.sing-box-tui-subscription-backup`. Each refresh replaces that same backup file, so only one subscription-refresh backup is kept on disk.

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

The TUI keeps the original two-column layout when the selected selector has zero or one nested Internet Route selector. When the selected selector contains multiple child selector groups, it switches to a three-column layout:

```text
Selector Groups | Internet Routes | Nodes
```

Selecting a node inside an Internet Route group updates that child selector and then points the parent selector at that route group. This supports configs shaped like `手动选择 -> 宝贝云 -> node`.

## Installation

The platform installers install `sing-box-tui` and install the configured
`sing-box` core release when `sing-box` is not already on `PATH`. Set
`-SingBoxDir` on Windows or `--sing-box-dir` on macOS/Linux to install it in a
specific directory. When this option is set, the installer checks only that
directory for the `sing-box` executable instead of checking the global `PATH`.

Windows:

```powershell
scripts\windows\install.cmd
```

macOS/Linux:

```sh
scripts/install.sh
```

Useful Windows options:

```powershell
scripts\windows\install.cmd -Version v0.1.0
scripts\windows\install.cmd -SkipSingBox
scripts\windows\install.cmd -DownloadParts 1 -Force
scripts\windows\install.cmd -ForceGitHubProxy
scripts\windows\install.cmd -SingBoxDir "D:\Tools\sing-box"
```

Useful macOS/Linux options:

```sh
scripts/install.sh --check-only
scripts/install.sh --dry-run --force
scripts/install.sh --version v0.1.0 --force
scripts/install.sh --force-github-proxy
scripts/install.sh --github-proxy https://example.com/anywhere --force-github-proxy
scripts/install.sh --sing-box-dir "$HOME/.local/bin"
```

The Unix installer uses four parallel byte-range requests by default and falls
back to `https://deeloo.cn/anywhere` when GitHub is unavailable. If a network
or proxy does not handle ranges reliably, use a single request. The timeout
options control the total transfer time and how long a connection may make no
progress:

```sh
scripts/install.sh --download-parts 1
scripts/install.sh --download-timeout-sec 1800 --download-stall-timeout-sec 120
scripts/install.sh --github-proxy https://example.com/github-proxy
scripts/install.sh --github-proxy ""
```

Passing an empty GitHub proxy disables the fallback. Run
`scripts/install.sh --help` for all installer options and environment-variable
overrides.

## Requirements

- `sing-box` must expose `experimental.clash_api.external_controller`, usually `127.0.0.1:9992`
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
      "external_controller": "127.0.0.1:9992",
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
cargo run -- run --controller http://127.0.0.1:9992
```

Inside the TUI, use `/` to set a node-name filter such as `美国` or `美国,香港`, then press `a` to enable automatic selection for the selected selector group and active node-view panel. Prefix a filter term with `!` or `-` to exclude matching nodes, for example `美国,!倍率` keeps US nodes except names containing `倍率`, and `!香港` keeps all nodes except names containing `香港`. Automatic selection runs in a background worker controlled by the TUI over TCP JSON lines. Every 30 seconds the worker runs a three-attempt reachability assessment, ranks only nodes included by the active panel, and requires the same candidate to win two complete rounds. Within one reachability tier, balanced and low-latency policies require at least a 20% lower warm median, while throughput policy requires at least 20% higher sustained throughput. A switch is deferred while current-node connections grow by more than 64 KiB in the preceding 10 seconds. Candidate probes address concrete outbound tags and never mutate the live selector; only a confirmed automatic-selection decision switches it.

TUI node-quality history is stored next to the canonical active sing-box config. A config named `config.json` keeps the compatible sibling name `singbox.sqlite3`; another config such as `office.json` uses `office.json.sing-box-tui.sqlite3`, so configs in one directory do not share histories accidentally. `SING_BOX_TUI_DB` can override the path: an absolute value is used directly, while a relative value is resolved from the canonical active-config directory (not the process working directory). Empty values and `:memory:` are rejected for active configs. A custom-config database previously created in another working directory is not migrated automatically; use an absolute override if it should remain authoritative.

Schema v7 stores probe attempts, reachability assessments, sustained-quality results, usability-criterion runs, and node fingerprints; it has no legacy single-delay table or compatibility reader. The database stores the tag and hash, not outbound JSON or credentials. Subscription/import/provider writes reconcile history only after the active config is durably committed: unchanged tag-and-fingerprint pairs keep their facts, while removed outbounds and same-tag changed outbounds lose prior facts. An exact published v6 schema is migrated in place: only the obsolete single-delay table and its indexes are removed, while factual node-quality and usability data is preserved. Earlier pre-fact schemas are rebuilt. A malformed v6 or v7 schema, a future schema, or a non-SQLite file fails closed and is not replaced.

TUI runtime state is written to `./sing-box-tui.json` by default. Set `SING_BOX_TUI_CONFIG=/path/to/sing-box-tui.json` to use a different file. The state file records the last node-name filter, whether automatic selection is enabled, its target selector and stable node-view ID, explicit background permission for custom usability criteria, the current selected node for each selector group, and the last explicit TUN mode and China IP routing choices. On startup, the TUI re-applies saved selector choices when the saved node still exists in that selector. Confirmation streaks and traffic windows are intentionally rebuilt after restart, so pre-restart observations cannot complete a switch.

When auto-pick is enabled, the worker pid, TCP address, and token are recorded so `sing-box-tui background status` can query it and `sing-box-tui background stop` can stop it. Live TUI-to-worker interaction uses TCP while the registry file is only discovery data, not the live communication channel. Pressing `q` stops the worker together with the managed sing-box process; pressing `B` leaves sing-box, the worker, and active Private Access sessions running with their last applied settings. Starting the TUI again reconnects to the existing TCP-managed worker when auto-pick is enabled. Private Access sessions left by `B` are shown as `BACKGROUND` while the recorded profile pid is still alive.

The background control listener binds to `127.0.0.1:0` by default. Set `SING_BOX_TUI_BACKGROUND_BIND=HOST:PORT` to choose an address. Non-loopback addresses are rejected unless `SING_BOX_TUI_BACKGROUND_ALLOW_REMOTE=1` is also set, because the registry contains the control token.

TUI bypass entries are stored in that same state file and written to `sing-box-tui-bypass.json` next to the canonical active sing-box config by default. Set `SING_BOX_TUI_BYPASS_RULE_SET=/path/to/sing-box-tui-bypass.json` to use a different file; an absolute override is used directly and a relative override is resolved from the canonical active-config directory. Generated and merged configs reference the default adjacent file near the top of `route.rules`, routing matched domains/IPs/CIDRs to `direct` / `国内直连`. When using a custom override, keep the active config's local rule-set `path` pointed at that same resolved file. If an older live config does not yet reference the rule-set, regenerate/merge the config and restart or reload sing-box once; after that, the local rule-set file can be edited by the TUI and sing-box will reload it.

Generated and merged configs also route `100.64.0.0/10` direct so Tailscale and
other CGNAT overlay addresses do not go through the proxy.

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
SING_BOX_CONTROLLER=http://127.0.0.1:9992 cargo run -- run
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

- If `./config.json` does not exist, the importer creates a complete workspace-local config with sane defaults:
  - mixed inbound on `127.0.0.1:6780`
  - `selector` outbound `select`
  - `urltest` outbound `auto`
  - `direct` and `block`
  - `route.final = "select"`
  - `experimental.clash_api` on `127.0.0.1:9992`
  - remote `geoip-cn`, `geosite-*`, and `AdGuardSDNSFilter` rule-sets only when `--include-geosite-rules` is passed
  - TUN inbound only when `--include-tun-mode` is passed
- If `./config.json` exists, the importer reads it and merges the imported nodes into that config by default.
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
sing-box check -c ./config.json
sudo systemctl restart sing-box
```

After the config references the local rule-set, press `b` in the TUI to edit bypass entries. The TUI writes `sing-box-tui-bypass.json`; new or retried connections use the updated direct-bypass rules.

## Node quality and node-view panels

The Current selector panel always shows every selector member in selector order. Streaming is a
bundled custom usability probe and is visible by default. Press `U` on its tab to run an HTTPS
prefilter followed by a bounded 512 KiB transfer through an isolated runtime for each surviving
node. Nodes enter the panel only after that custom probe accepts them, and are ranked by throughput.
GitHub SSH and Agy Gemini are also bundled, but their tabs are hidden by default. A local manifest
can show either one without an external adapter, or hide/replace any bundled criterion by using the
same stable ID. The Agy probe resolves `agy` from `PATH`, or from
`SING_BOX_TUI_AGY_EXECUTABLE`. Each additional local usability-probe manifest contributes another
panel; a node may belong to several panels, and the active panel defines both visible candidates
and the automatic-selection boundary.

`T` runs three sequential quick probe attempts per node through the live controller, with different
nodes processed concurrently up to the configured cap. `t` performs that quick assessment and a
bounded 512 KiB sustained-quality probe for the highlighted node. `U` runs the active usability
criterion, including Streaming and any visible configured criterion. Sustained transfer and
executable or bundled usability criteria use isolated runtimes and never change the live selector.
`i` opens factual probe outcomes, reachability assessment, sustained quality, usability-criterion
results, expiry, and the latest automatic-selection explanation.

See [usability-probe manifests](docs/usability-probe-manifests.md) for registration, direct argument
execution, background permission, schedules, resource limits, result expiry, and the Agy example.
See the [release smoke test](docs/release-smoke-test.md) for bounded live validation.

For enterprise intranet access through an external Private Access service process,
including the Hillstone service, TUI settings, and troubleshooting, see
[docs/private-access-service.md](docs/private-access-service.md).

Run the TUI with a quick-node concurrency cap:

```bash
cargo run -- run --max-concurrency 8
```

## Clash API Inspection

Two read-only controller commands are available in addition to the TUI node-quality flow:

- `selectors`: returns JSON for all selector groups, or one group with `--selector NAME`
- `status`: returns controller version, current traffic counters, aggregate connection totals, and active connection metadata

Detached auto-pick worker commands are available separately:

- `background status`: returns JSON for the live detached headless auto-pick worker
- `background stop`: stops that detached worker and disables saved auto-pick

## Keys

- `Up` / `Down` or `j` / `k`: move
- `Tab`, `h`, `l`, `Left`, `Right`: switch pane
- `Space`: apply/switch to the currently highlighted proxy in the current selector group
- `b`: edit direct-bypass domains, IPs, and CIDRs; values are comma-separated and are written to the local sing-box rule-set
- `B`: exit the TUI while keeping the managed sing-box process, auto-pick background worker, and active Private Access sessions running
- `p`: on Windows/macOS/Linux, toggle the system proxy for the sing-box mixed inbound
- `\`: toggle the Internet Proxy TUN mode; adds or removes the sing-box `tun` inbound in the configured `config.json` and restarts the managed sing-box process. On macOS/Linux this needs `sudo` (the TUI prompts with `sudo -v`); on Windows it needs an Administrator session.
- `Enter`: unused for selection
- `Left` / `Right` while the candidate pane is focused: move between Current selector, Streaming, and custom usability-criterion panels
- `T`: run a three-attempt quick reachability assessment for the current panel/selector scope
- `t`: run a complete quick-plus-sustained assessment for the highlighted node
- `U`: manually run the active usability criterion, including the default Streaming probe
- `P`: grant or revoke independent background permission for the active usability criterion when its manifest permits scheduling
- `a`: toggle panel-aware automatic selection; the background worker requires two complete winning rounds, a same-tier 20% material improvement, and no active current-node transfer before switching
- `i`: show node-quality evidence and the latest automatic-selection explanation; use `j` / `k` to scroll
- `c`: show active sing-box connections, including inbound type, destination, outbound chain, and route rule; press `r` in this panel to refresh immediately
- `v`: immediately start configured background verification checks
- `o`: open TUI settings for quick assessment, sustained quality, automatic selection, and system proxy values
- `/`: change the node-name filter; comma-separated include values match any value, and `!` or `-` prefixes exclude values, for example `美国,香港,!倍率`
- `r`: refresh groups
- `?`: show the help modal; use `Up` / `Down` or `j` / `k` to browse it

The TUI deliberately leaves terminal mouse capture disabled. Drag with the mouse to select text using your terminal's native selection and copy behavior.
- `q`: quit

During a quick assessment, rows show a brighter probing state and then `3/3`, `2/3`, `1/3`, `0/3`, or `incomplete`. Only timeout and transport failure count against a node; controller failure, invalid measurement, and cancellation leave the assessment incomplete.

## System Proxy

The TUI can set the OS system proxy with `p`. On Windows, it updates the current
user's WinINET proxy:

```powershell
scripts\windows\set-system-proxy.cmd -Enable -Server 127.0.0.1:6780
```

When enabling the system proxy, TUI bypass entries are also written to the OS
proxy bypass list, alongside the default local/private and CGNAT overlay
network bypasses.

On macOS, it uses `networksetup` to update HTTP, HTTPS, and SOCKS proxies on
all enabled network services. To target specific macOS network services, set a
comma-separated service list:

```bash
SING_BOX_TUI_SYSTEM_PROXY_SERVICE="Wi-Fi,USB 10/100 LAN" cargo run -- run
```

On Linux, it uses `gsettings` to update the GNOME desktop proxy settings for
HTTP, HTTPS, and SOCKS:

```bash
gsettings set org.gnome.system.proxy mode manual
gsettings set org.gnome.system.proxy.http host 127.0.0.1
gsettings set org.gnome.system.proxy.http port 6780
gsettings set org.gnome.system.proxy.https host 127.0.0.1
gsettings set org.gnome.system.proxy.https port 6780
gsettings set org.gnome.system.proxy.socks host 127.0.0.1
gsettings set org.gnome.system.proxy.socks port 6780
```

The proxy server is detected from the configured sing-box JSON `mixed` inbound when possible. Override it with:

```powershell
$env:SING_BOX_TUI_SYSTEM_PROXY_SERVER = "127.0.0.1:6780"
```

To disable the Windows system proxy manually:

```powershell
scripts\windows\set-system-proxy.cmd -Disable
```

To disable the macOS system proxy manually for a service:

```bash
networksetup -setwebproxystate Wi-Fi off
networksetup -setsecurewebproxystate Wi-Fi off
networksetup -setsocksfirewallproxystate Wi-Fi off
```

To disable the Linux system proxy manually:

```bash
gsettings set org.gnome.system.proxy mode none
```

If you call the PowerShell script directly, pass a process-scoped execution policy override:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows\set-system-proxy.ps1 -Disable
```

## FAQ

### Why does the browser not use sing-box after I enable system proxy?

Check that the TUI status line shows system proxy enabled and that the detected
mixed inbound is correct. Press `o` to edit the system proxy server if your
sing-box mixed inbound is not `127.0.0.1:6780`.

### Why does `netsh winhttp show proxy` say no proxy is set on Windows?

On Windows, sing-box-tui updates the current user's WinINET proxy, which is the
proxy used by Windows Settings, Internet Options, and many desktop apps.
`netsh winhttp show proxy` reports the separate WinHTTP proxy used by some
services and command-line components, so it can still show direct access after
the system proxy is enabled.

To copy the current WinINET proxy into WinHTTP, run an elevated shell:

```powershell
netsh winhttp import proxy source=ie
```

To set or reset the WinHTTP proxy manually:

```powershell
netsh winhttp set proxy 127.0.0.1:6780
netsh winhttp reset proxy
```

Services such as Tailscale may not use the current user's WinINET proxy. If
Tailscale cannot reach the network after enabling the TUI system proxy, either
configure the WinHTTP proxy as above or configure an HTTP/HTTPS proxy for
Tailscale separately.

### What is the difference between bypass rules and system proxy bypass?

TUI bypass rules are written to `sing-box-tui-bypass.json` so sing-box can route
matching domains/IPs to direct. When system proxy is enabled, the same entries
are also written to the OS proxy bypass list so system-proxy-aware apps can skip
the local proxy for those targets.

### How do I restore the Windows proxy manually?

Run:

```powershell
scripts\windows\set-system-proxy.cmd -Disable
```

### What should I use: TUN mode or system proxy?

System proxy is easier to toggle and works for applications that respect OS
proxy settings. TUN mode captures more traffic but needs a sing-box config with
a TUN inbound and usually requires higher system permissions.

Press `\` in the TUI to toggle the Internet Proxy TUN mode. It adds or removes
the sing-box `tun` inbound (`tun-in`, `auto_route`/`strict_route`) in the
configured config and restarts the managed sing-box process. Toggling TUN on
requires `sudo` on macOS/Linux (the TUI runs `sudo -v` first) or an
Administrator session on Windows, because sing-box must create a network
interface and change routes. For manual config edits, see
[docs/tun-mode.md](docs/tun-mode.md).

### How do I toggle China IP routing?

Open TUI settings with `o` and edit the **China IP routing** field to `true` or
`false`. When enabling, the TUI first downloads the `geoip-cn`, `geosite-cn`,
`geosite-geolocation-cn`, and `geosite-geolocation-!cn` binary rule-sets through
the running proxy into `sing-box-tui-rulesets/` next to the config, then writes
them as local rule-sets so China IPs/domains go `direct` (`国内直连`) while
everything else follows the selector. The download runs through the proxy
because the rule-set source (`raw.githubusercontent.com`) is usually unreachable
directly, and a local rule-set means sing-box never depends on reaching it at
startup. Changing it restarts the managed sing-box process. The setting is
remembered in `sing-box-tui.json` and re-applied if a subscription refresh
regenerates the config without those rule-sets.

This is separate from `--include-geosite-rules`, which additionally bundles the
`AdGuardSDNSFilter` ad-block rule-set and only affects newly generated default
configs.

### Why does the first-run wizard appear?

It appears until setup is completed or skipped. Paste a subscription URL to
create `.suburl`, or press `s` to mark onboarding complete. You can still edit
settings later with `o`.
