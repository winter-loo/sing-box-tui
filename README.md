# sing-box-tui

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
cargo run
```

Or point it at a different controller:

```bash
SING_BOX_CONTROLLER=http://127.0.0.1:9090 cargo run
```

If the controller has a secret:

```bash
SING_BOX_SECRET='your-secret' cargo run
```

## Import Clash Nodes

Nodes-only JSON:

```bash
cargo run -- --import-from clash_proxies.txt --import-output imported_nodes.json
```

Full `config.json` output:

```bash
cargo run -- --import-from clash_proxies.txt --import-full-config --import-output config.json
```

Full-config behavior:

- If `/etc/sing-box/config.json` does not exist, the importer creates a complete config with sane defaults:
  - mixed inbound on `127.0.0.1:5780`
  - `selector` outbound `select`
  - `urltest` outbound `auto`
  - `direct` and `block`
  - `route.final = "select"`
  - `experimental.clash_api` on `127.0.0.1:9090`
- If `/etc/sing-box/config.json` exists, the importer reads it and merges the imported nodes into that config instead of replacing it.
- Existing `select`, `auto`, `direct`, and `block` outbounds are reused when present.
- Imported node tags replace same-tag outbounds and are appended otherwise.

Use a different source config path if needed:

```bash
cargo run -- --import-from clash_proxies.txt --import-full-config --import-config-path ./config.json --import-output merged-config.json
```

Validate the generated config:

```bash
sing-box check -c config.json
```

## Keys

- `Up` / `Down` or `j` / `k`: move
- `Tab`, `h`, `l`, `Left`, `Right`: switch pane
- `Enter`: apply the selected proxy to the selected selector group
- `r`: refresh groups
- `q`: quit
