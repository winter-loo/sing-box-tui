# AirTCP Sing-box Subscription URL Extraction

This note documents how the AirTCP sing-box subscription URL was extracted from an already-authenticated Chrome tab through Chrome DevTools Protocol (CDP).

Do not commit live subscription tokens or private link identifiers in documentation. The examples below use `<AIRTCP_LINK_TOKEN>` where the live link path appeared.

## Result Format

The AirTCP page exposes a direct sing-box subscription URL:

```text
https://spring.mailrelay.us/link/<AIRTCP_LINK_TOKEN>?singbox=1
```

The page's one-click Sing-box import helper wraps that URL in a custom protocol link:

```text
sing-box://import-remote-profile?url=https%3A%2F%2Fspring.mailrelay.us%2Flink%2F<AIRTCP_LINK_TOKEN>%3Fsingbox%3D1#AirTCP
```

The `url=` query parameter is the URL-encoded subscription URL. The fragment is the profile name, `AirTCP`.

## Browser Setup

The extraction was done from the independent Chrome instance already running with CDP enabled:

```text
http://127.0.0.1:9229/json/version
```

That browser had been started with a copied Chrome profile and:

```text
--remote-debugging-address=127.0.0.1
--remote-debugging-port=9229
```

The copied profile was important because it preserved the authenticated AirTCP session without restarting the user's normal Chrome process.

## Find The AirTCP Tab

Open CDP targets were listed with:

```bash
curl -fsS http://127.0.0.1:9229/json/list
```

The AirTCP target was the page with:

```text
title: 首页 — AirTCP
url:   https://5.airtcp.me/user
type:  page
```

The `webSocketDebuggerUrl` from this target was used for runtime inspection.

## Inspect The Dashboard

The dashboard DOM was inspected with CDP `Runtime.evaluate`. The useful page text included a `便捷导入` section with:

```text
一键导入 ClashX / CFW / CFA 配置
一键导入 sing-box 配置
【推荐】一键导入 Shadowrocket 配置
Surge 托管配置
复制 V2Ray 订阅链接
```

The relevant visible anchor was:

```html
<a
  href="##"
  class="btn btn-icon icon-left btn-primary btn-singbox btn-lg btn-round"
  onclick="importSublink('singbox')"
>
  一键导入 sing-box 配置
</a>
```

This identified `.btn-singbox` as the Sing-box one-click import button and `importSublink('singbox')` as the handler to inspect.

## Inspect The Button And Inline Scripts

The following page-runtime inspection listed the Sing-box button, related import buttons, clipboard URLs, and inline scripts:

```javascript
(() => {
  const clean = (s) => (s || "").replace(/\s+/g, " ").trim();
  const selectors = [
    ".btn-singbox",
    ".btn-clash",
    ".btn-shadowrocket",
    ".btn-v2ray",
    ".copy-text",
  ];

  const nodes = selectors.flatMap((selector) =>
    Array.from(document.querySelectorAll(selector)).map((el) => ({
      selector,
      tag: el.tagName,
      text: clean(el.innerText || el.textContent || el.value || el.title),
      href: el.getAttribute("href"),
      onclick: el.getAttribute("onclick"),
      data: Array.from(el.attributes).map((attr) => [attr.name, attr.value]),
      html: el.outerHTML,
    })),
  );

  const scripts = Array.from(document.scripts)
    .map((script, idx) => ({
      idx,
      src: script.src,
      text: (script.textContent || "").slice(0, 4000),
    }))
    .filter((script) =>
      /sing|clash|v2ray|sub|subscribe|token|link|clipboard/i.test(
        script.src + " " + script.text,
      ),
    );

  return { nodes, scripts };
})();
```

The `.btn-singbox` element had:

```text
onclick="importSublink('singbox')"
```

The inline `importSublink` function contained the Sing-box branch:

```javascript
if (client == "singbox") {
  oneclickImport(
    "singbox",
    "https://spring.mailrelay.us/link/<AIRTCP_LINK_TOKEN>?singbox=1",
  );
}
```

That branch is the source of the direct subscription URL.

## Inspect The One-click Import Wrapper

The page's `oneclickImport` function was then printed from the runtime:

```javascript
(() => String(window.oneclickImport))();
```

The relevant part of the function was:

```javascript
function oneclickImport(client, subLink) {
  var sublink = {
    surfboard: "surfboard:///install-config?url=" + encodeURIComponent(subLink),
    quantumult: "quantumult://configuration?server=" + btoa(subLink).replace(/=/g, ""),
    shadowrocket: "shadowrocket://add/sub://" + btoa(subLink) + "?remarks=" + appName,
    surge: "surge:///install-config?url=" + encodeURIComponent(subLink),
    surge3: "surge3:///install-config?url=" + encodeURIComponent(subLink),
    clash: "clash://install-config?url=" + encodeURIComponent(subLink),
    singbox: "sing-box://import-remote-profile?url=" + encodeURIComponent(subLink) + "#AirTCP",
    ssr: "sub://" + btoa(subLink),
  };

  window.location.href = sublink[client];
}
```

The real function also contains a desktop warning dialog for clients other than Clash and SSR. That warning does not change how the Sing-box URL is built; it only delays navigation until the user confirms.

This confirmed two things:

1. The direct Sing-box subscription URL is the `subLink` passed by `importSublink('singbox')`.
2. The full one-click import URL is built as `sing-box://import-remote-profile?url=<encoded subLink>#AirTCP`.

## Decode Or Rebuild The Import URL

The import URL can be rebuilt from the direct subscription URL:

```bash
python3 - <<'PY'
from urllib.parse import quote

sub_url = "https://spring.mailrelay.us/link/<AIRTCP_LINK_TOKEN>?singbox=1"
import_url = "sing-box://import-remote-profile?url=" + quote(sub_url, safe="") + "#AirTCP"

print(import_url)
PY
```

Expected output:

```text
sing-box://import-remote-profile?url=https%3A%2F%2Fspring.mailrelay.us%2Flink%2F<AIRTCP_LINK_TOKEN>%3Fsingbox%3D1#AirTCP
```

The direct subscription URL can also be recovered from the import URL:

```bash
python3 - <<'PY'
from urllib.parse import parse_qs, urlparse

import_url = (
    "sing-box://import-remote-profile?"
    "url=https%3A%2F%2Fspring.mailrelay.us%2Flink%2F<AIRTCP_LINK_TOKEN>%3Fsingbox%3D1"
    "#AirTCP"
)

print(parse_qs(urlparse(import_url).query)["url"][0])
PY
```

Expected output:

```text
https://spring.mailrelay.us/link/<AIRTCP_LINK_TOKEN>?singbox=1
```

## Why No API Call Was Needed

Unlike the Baobeiyun dashboard, AirTCP exposed the Sing-box subscription URL directly in the rendered page's inline JavaScript. The extraction did not require making an authenticated API request.

The useful chain was:

```text
.btn-singbox
  -> onclick="importSublink('singbox')"
  -> importSublink calls oneclickImport("singbox", "https://spring.mailrelay.us/link/<AIRTCP_LINK_TOKEN>?singbox=1")
  -> oneclickImport wraps it as sing-box://import-remote-profile?url=<encoded URL>#AirTCP
```

## Summary

The URL was recovered by:

1. Connecting to the copied Chrome profile through CDP on port `9229`.
2. Finding the authenticated AirTCP tab through `/json/list`.
3. Inspecting the dashboard DOM and locating the `.btn-singbox` button.
4. Reading its inline handler, `importSublink('singbox')`.
5. Inspecting the inline `importSublink` function and extracting the `?singbox=1` subscription URL passed to `oneclickImport`.
6. Inspecting `oneclickImport` to confirm the generated Sing-box import URL format.
7. URL-encoding the subscription URL inside `sing-box://import-remote-profile?url=...#AirTCP`.
