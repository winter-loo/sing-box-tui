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

## Keys

- `Up` / `Down` or `j` / `k`: move
- `Tab`, `h`, `l`, `Left`, `Right`: switch pane
- `Enter`: apply the selected proxy to the selected selector group
- `r`: refresh groups
- `q`: quit
