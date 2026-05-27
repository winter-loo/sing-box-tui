# Subscription Node Extraction And Benchmarking

This note documents the manual workflow used to extract sing-box nodes from a provider subscription URL and benchmark them through the local Clash-compatible API.

Do not commit subscription tokens. Use an environment variable or a local shell variable instead.

## 1. Fetch The Subscription

Some providers return different content based on the user agent. If a plain `curl` response is empty, retry with a sing-box user agent:

```bash
SUB_URL='https://h.bbydy.org/api/bby/client/subscribe?token=REDACTED'

curl -sS -L \
  -A 'sing-box' \
  -D /tmp/singbox-sub.headers \
  -o /tmp/singbox-sub.body \
  "$SUB_URL"
```

Inspect the response:

```bash
wc -c /tmp/singbox-sub.body
sed -n '1,80p' /tmp/singbox-sub.headers
jq type /tmp/singbox-sub.body
```

If the body is JSON, list outbound tags and types:

```bash
jq -r '.outbounds[]? | [.tag,.type] | @tsv' /tmp/singbox-sub.body
```

Real nodes are usually outbound types such as `vless`, `vmess`, `trojan`, `hysteria2`, or `shadowsocks`. Ignore selector/control/info entries such as:

```text
selector
urltest
direct
剩余流量：...
套餐到期：...
TG群：...
邀请好友返佣...
```

## 2. Validate Or Convert The Config

This repo now has a direct command for generating a merged config from a subscription URL:

```bash
cargo run -- subscribe \
  --url "$SUB_URL" \
  --config ./config.json \
  --output /tmp/merged-config.json \
  --replace-nodes

sing-box check -c /tmp/merged-config.json
```

Use the manual steps below when debugging provider responses or benchmarking individual nodes.

First try the subscription config directly:

```bash
sing-box check -c /tmp/singbox-sub.body
```

If installed sing-box rejects legacy inbound or DNS syntax, keep the provider outbounds but replace the runtime-facing parts with a minimal local test config:

```bash
jq '
  del(.dns, .route.rule_set)
  | .inbounds = [
      {
        "type": "mixed",
        "tag": "mixed-in",
        "listen": "127.0.0.1",
        "listen_port": 2334
      }
    ]
  | .route = {"final":"节点选择"}
  | .experimental.clash_api.external_controller = "127.0.0.1:9090"
' /tmp/singbox-sub.body > /tmp/singbox-sub-test.json

sing-box check -c /tmp/singbox-sub-test.json
```

Adjust `节点选择` if the subscription uses a different selector tag.

## 3. Start sing-box For Testing

Make sure ports `2334` and `9090` are free:

```bash
ss -ltnp | rg ':(2334|9090)\b' || true
```

Start sing-box with the temporary config:

```bash
sing-box run -c /tmp/singbox-sub-test.json
```

The temporary config exposes:

- mixed proxy: `127.0.0.1:2334`
- Clash API: `127.0.0.1:9090`

## 4. Inspect Selectors Through The Clash API

Query all proxies:

```bash
curl -sS http://127.0.0.1:9090/proxies | jq '.proxies | keys'
```

Inspect a selector:

```bash
curl -sS http://127.0.0.1:9090/proxies \
  | jq '.proxies["节点选择"] | {now, all_count:(.all|length), all}'
```

## 5. Benchmark Nodes

The Clash API delay endpoint benchmarks one node at a time:

```bash
NODE='L1|日本04|直连|流媒体|2x'
ENCODED_NODE=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$NODE")

curl -sS \
  "http://127.0.0.1:9090/proxies/$ENCODED_NODE/delay?timeout=5000&url=https://www.gstatic.com/generate_204"
```

For a full benchmark, run concurrent probes and sort successful results by latency:

```bash
python3 - <<'PY'
import concurrent.futures
import json
import time
import urllib.parse
import urllib.request

base = "http://127.0.0.1:9090"
selector = "节点选择"
exclude_prefixes = ("剩余流量", "套餐到期")
exclude_contains = ("直连地址", "TG群", "邀请好友", "请更换客户端")

proxies = json.load(urllib.request.urlopen(base + "/proxies"))["proxies"]
selector_data = proxies[selector]

candidates = []
for name in selector_data.get("all", []):
    proxy = proxies.get(name, {})
    proxy_type = str(proxy.get("type", "")).lower()
    if proxy_type in {"selector", "urltest", "direct"}:
        continue
    if name in {"direct", selector}:
        continue
    if name.startswith(exclude_prefixes):
        continue
    if any(part in name for part in exclude_contains):
        continue
    candidates.append(name)

def test(name):
    encoded = urllib.parse.quote(name, safe="")
    url = f"{base}/proxies/{encoded}/delay?timeout=5000&url=https://www.gstatic.com/generate_204"
    started = time.time()
    try:
        with urllib.request.urlopen(url, timeout=7) as response:
            data = json.load(response)
        delay = data.get("delay")
        return {
            "name": name,
            "delay": delay if isinstance(delay, int) else None,
            "error": data.get("message") if not isinstance(delay, int) else None,
            "elapsed_ms": round((time.time() - started) * 1000),
        }
    except Exception as exc:
        return {
            "name": name,
            "delay": None,
            "error": str(exc),
            "elapsed_ms": round((time.time() - started) * 1000),
        }

with concurrent.futures.ThreadPoolExecutor(max_workers=16) as executor:
    rows = list(executor.map(test, candidates))

ok = sorted(
    [row for row in rows if isinstance(row.get("delay"), int) and row["delay"] >= 0],
    key=lambda row: row["delay"],
)
failed = [row for row in rows if row not in ok]

print(json.dumps({
    "tested": len(rows),
    "ok": len(ok),
    "failed": len(failed),
    "top20": ok[:20],
    "failed_names": [row["name"] for row in failed],
}, ensure_ascii=False, indent=2))
PY
```

## 6. Switch To The Best Node

Use the selector endpoint to switch the active node:

```bash
BEST='L1|日本04|直连|流媒体|2x'
SELECTOR='节点选择'
ENCODED_SELECTOR=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$SELECTOR")

curl -sS -X PUT \
  "http://127.0.0.1:9090/proxies/$ENCODED_SELECTOR" \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"$BEST\"}"
```

## 7. Verify Real Traffic

Latency alone is not enough. Verify at least one real request through the local mixed proxy:

```bash
curl -x http://127.0.0.1:2334 -4 -I -sS --max-time 12 https://www.google.com | sed -n '1,5p'
curl -x http://127.0.0.1:2334 -I -sS --max-time 12 https://github.com | sed -n '1,5p'
```

A usable node should return successful HTTP headers, for example `HTTP/2 200`.

## Notes

- `https://www.gstatic.com/generate_204` is used for lightweight delay checks.
- Keep subscription URLs out of committed files and shell history when possible.
- Stop the temporary `sing-box run` process after benchmarking if it is not the live service.
