# Baobeiyun Sing-box Subscription URL Extraction

This note documents how the Baobeiyun (`宝贝云`) sing-box subscription URL was extracted from an already-authenticated Chrome tab through Chrome DevTools Protocol (CDP).

Do not commit real subscription tokens. The examples below use `<SUBSCRIBE_TOKEN>` where the live token appeared.

## Result Format

The Sing-box button produced a custom protocol import URL:

```text
sing-box://import-remote-profile?url=https%3A%2F%2Fe.bbydy.org%2Fapi%2Fbby%2Fclient%2Fsubscribe%3Ftoken%3D<SUBSCRIBE_TOKEN>#%E5%AE%9D%E8%B4%9D%E4%BA%91
```

The decoded subscription URL inside the `url=` query parameter is:

```text
https://e.bbydy.org/api/bby/client/subscribe?token=<SUBSCRIBE_TOKEN>
```

The URL fragment decodes to `宝贝云`, which is the remote profile name used by the Sing-box import link.

## Browser Setup

The normal Chrome instance was already running without `--remote-debugging-port`, so enabling CDP on that live process was not possible without restarting it.

Instead, the current Chrome user data directory was copied to a temporary independent profile directory. Runtime lock files and cache-heavy directories were excluded so the copied profile could start separately:

```bash
SRC="$HOME/Library/Application Support/Google/Chrome"
DEST="/tmp/chrome-cdp-profile-9229-YYYYMMDD-HHMMSS"

mkdir -p "$DEST"
rsync -a \
  --exclude='Singleton*' \
  --exclude='Crashpad' \
  --exclude='*/Cache/*' \
  --exclude='*/Code Cache/*' \
  --exclude='*/GPUCache/*' \
  --exclude='*/GrShaderCache/*' \
  --exclude='*/ShaderCache/*' \
  --exclude='*/Service Worker/CacheStorage/*' \
  "$SRC/" "$DEST/"

find "$DEST" -maxdepth 1 -name 'Singleton*' -delete
```

Chrome was then launched as a separate macOS app instance with CDP enabled on port `9229`:

```bash
open -n -a "Google Chrome" --args \
  --user-data-dir="$DEST" \
  --profile-directory="Default" \
  --remote-debugging-address=127.0.0.1 \
  --remote-debugging-port=9229 \
  --remote-allow-origins='*' \
  --no-first-run \
  --no-default-browser-check \
  --new-window about:blank
```

The CDP endpoint was verified with:

```bash
curl -fsS http://127.0.0.1:9229/json/version
```

## Find The Baobeiyun Tab

The open tabs were listed through CDP:

```bash
curl -fsS http://127.0.0.1:9229/json/list
```

The target tab was the page target with:

```text
title: 宝贝云
url:   https://user2.bby012.com/#/dashboard
type:  page
```

The `webSocketDebuggerUrl` from that target was then used for all page runtime inspection.

## Inspect The Dashboard

Using `Runtime.evaluate`, the page DOM and storage were inspected from the Baobeiyun tab.

Important observations:

- The dashboard was already authenticated.
- `localStorage.authorization` contained the JWT used by the frontend API calls.
- The visible page text contained `一键订阅` (`one-click subscription`).
- The loaded resources included:
  - `https://api.345110.xyz/api/v1/user/getSubscribe`
  - `https://api3.345119.xyz/api/v1/user/getSubscribe`
  - `https://3.115.134.89/api/v1/user/getSubscribe`
  - `/images/icon/Sing-box.png`

The storage/API inspection was done in the page runtime instead of with plain `curl`, because the browser already had the correct origin/session context. Directly fetching some frontend assets with `curl` was also unreliable because of TLS host/certificate mismatch behavior on `user2.bby012.com`.

## Query The Subscription API

From inside the authenticated page runtime, the frontend API was queried with the JWT from `localStorage.authorization`:

```javascript
const auth = localStorage.getItem("authorization");

const endpoints = [
  "https://api.345110.xyz/api/v1/user/getSubscribe",
  "https://api3.345119.xyz/api/v1/user/getSubscribe",
  "https://3.115.134.89/api/v1/user/getSubscribe",
];

for (const url of endpoints) {
  const res = await fetch(url, { headers: { authorization: auth } });
  console.log(url, await res.json());
}
```

Those API responses contained the account subscription token and provider-generated `subscribe_url` values. The returned hosts differed by API endpoint, for example:

```text
https://c.bbydy.org/api/bby/client/subscribe?token=<SUBSCRIBE_TOKEN>
https://f.bbydy.org/api/bby/client/subscribe?token=<SUBSCRIBE_TOKEN>
https://h.bbydy.org/api/bby/client/subscribe?token=<SUBSCRIBE_TOKEN>
```

However, these API responses alone did not identify the exact Sing-box import URL. The dashboard's Sing-box button generated a different host (`e.bbydy.org`), so the UI click handler still had to be captured.

## Locate The One-click Subscription UI

The dashboard contained a shortcut card:

```text
一键订阅
快速将节点导入对应客户端进行使用
```

Clicking that card displayed a modal with these options:

```text
复制订阅地址
扫描二维码订阅
导入到 Hiddify
导入到 Sing-box
导入到 ClashX
```

The relevant DOM classes were:

```text
.v2board-shortcuts-item   # dashboard shortcut items
.oneClickSubscribe...     # modal body
.sing-box                 # "导入到 Sing-box" item
```

The shortcut was opened from the page runtime with:

```javascript
const clean = (s) => (s || "").replace(/\s+/g, " ").trim();

const shortcut = Array.from(document.querySelectorAll(".v2board-shortcuts-item"))
  .find((el) => clean(el.innerText).startsWith("一键订阅"));

shortcut.dispatchEvent(new MouseEvent("click", {
  bubbles: true,
  cancelable: true,
  view: window,
}));
```

After that, `.sing-box` was visible in the modal.

## Capture The Sing-box Navigation

The `.sing-box` modal item did not expose the final URL as an `href` attribute. Its React click handler initiated a navigation to a custom protocol URL.

To capture that without guessing, CDP `Page` events were enabled, then the `.sing-box` element was clicked:

```javascript
const el = document.querySelector(".sing-box");
const r = el.getBoundingClientRect();

for (const type of ["mousedown", "mouseup", "click"]) {
  el.dispatchEvent(new MouseEvent(type, {
    bubbles: true,
    cancelable: true,
    view: window,
    clientX: r.left + r.width / 2,
    clientY: r.top + r.height / 2,
  }));
}
```

CDP emitted `Page.frameScheduledNavigation` and `Page.frameRequestedNavigation` events containing the generated URL:

```text
sing-box://import-remote-profile?url=https%3A%2F%2Fe.bbydy.org%2Fapi%2Fbby%2Fclient%2Fsubscribe%3Ftoken%3D<SUBSCRIBE_TOKEN>#%E5%AE%9D%E8%B4%9D%E4%BA%91
```

That event was the source of truth for the Sing-box-specific subscription URL.

## Decode The URL

The custom protocol URL contains the actual remote subscription URL in the `url=` query parameter.

It can be decoded with Python:

```bash
python3 - <<'PY'
from urllib.parse import parse_qs, unquote, urlparse

sing_box_url = (
    "sing-box://import-remote-profile?"
    "url=https%3A%2F%2Fe.bbydy.org%2Fapi%2Fbby%2Fclient%2Fsubscribe%3Ftoken%3D<SUBSCRIBE_TOKEN>"
    "#%E5%AE%9D%E8%B4%9D%E4%BA%91"
)

parsed = urlparse(sing_box_url)
sub_url = parse_qs(parsed.query)["url"][0]
profile_name = unquote(parsed.fragment)

print(sub_url)
print(profile_name)
PY
```

Expected output:

```text
https://e.bbydy.org/api/bby/client/subscribe?token=<SUBSCRIBE_TOKEN>
宝贝云
```

## Summary

The URL was not taken from static page HTML. It was recovered by:

1. Launching an independent copied Chrome profile with CDP on port `9229`.
2. Finding the authenticated Baobeiyun dashboard tab through `/json/list`.
3. Inspecting page storage and API calls to confirm the subscription token source.
4. Opening the `一键订阅` modal.
5. Clicking the modal's `导入到 Sing-box` item through the page runtime.
6. Reading the generated `sing-box://...` URL from CDP `Page.frameScheduledNavigation` / `Page.frameRequestedNavigation`.
7. Decoding the `url=` query parameter to get the final HTTP subscription URL.
