#!/usr/bin/env python3
"""
Extract the BaiPiao (白嫖机场) sing-box subscription URL from an authenticated Chrome tab.

This script has no third-party Python dependencies. It talks directly to an
already-running Chrome DevTools Protocol (CDP) endpoint, matching the existing
provider extractor workflow.

Start Chrome with CDP enabled first, for example:
  open -n -a "Google Chrome" --args \
    --user-data-dir=/tmp/chrome-cdp-profile-9229 \
    --remote-debugging-address=127.0.0.1 \
    --remote-debugging-port=9229 \
    --remote-allow-origins='*' \
    --new-window 'https://yes.xn--mesv7f5toqlp.biz/console'

Then log in to 白嫖机场 in that Chrome window. This script prints the direct
HTTPS subscription URL by default. When writing to a file, it writes
`<provider name> = <url>` unless `--raw` is passed. Do not commit the printed
URL or any output file that contains the token.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import html
import json
import os
import re
import socket
import ssl
import struct
import sys
import time
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, quote, unquote, urlparse
from urllib.request import Request, urlopen


DEFAULT_CDP_URL = "http://127.0.0.1:9229"
DEFAULT_CONSOLE_URL = "https://yes.xn--mesv7f5toqlp.biz/console"
DEFAULT_PROVIDER_NAME = "白嫖机场"


class ExtractionError(RuntimeError):
    pass


class CdpConnection:
    def __init__(self, websocket_url: str):
        self.websocket_url = websocket_url
        self.sock: socket.socket | ssl.SSLSocket | None = None
        self.next_id = 1
        self.events: list[dict[str, Any]] = []

    def __enter__(self) -> "CdpConnection":
        self.connect()
        return self

    def __exit__(self, _exc_type: Any, _exc: Any, _tb: Any) -> None:
        self.close()

    def connect(self) -> None:
        parsed = urlparse(self.websocket_url)
        if parsed.scheme not in {"ws", "wss"}:
            raise ExtractionError(f"unsupported WebSocket URL: {self.websocket_url}")

        host = parsed.hostname
        if not host:
            raise ExtractionError(f"WebSocket URL has no host: {self.websocket_url}")
        port = parsed.port or (443 if parsed.scheme == "wss" else 80)
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"

        raw_sock = socket.create_connection((host, port), timeout=10)
        if parsed.scheme == "wss":
            self.sock = ssl.create_default_context().wrap_socket(raw_sock, server_hostname=host)
        else:
            self.sock = raw_sock

        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        self.sock.sendall(request.encode("ascii"))
        header = self._read_until(b"\r\n\r\n", timeout=10)
        if b" 101 " not in header.split(b"\r\n", 1)[0]:
            raise ExtractionError(f"CDP WebSocket upgrade failed: {header!r}")

        expected_accept = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()
        ).decode("ascii")
        if expected_accept.encode("ascii") not in header:
            raise ExtractionError("CDP WebSocket handshake returned an invalid accept key")

    def close(self) -> None:
        if self.sock is None:
            return
        try:
            self._send_frame(b"", opcode=0x8)
        except OSError:
            pass
        try:
            self.sock.close()
        finally:
            self.sock = None

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        request_id = self.next_id
        self.next_id += 1
        payload = {"id": request_id, "method": method}
        if params:
            payload["params"] = params
        self._send_json(payload)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            message = self.recv_json(timeout=max(0.05, deadline - time.monotonic()))
            if message is None:
                continue
            if "method" in message:
                self.events.append(message)
                continue
            if message.get("id") != request_id:
                continue
            if "error" in message:
                error = message["error"]
                raise ExtractionError(f"CDP {method} failed: {error}")
            return message.get("result", {})

        raise ExtractionError(f"timed out waiting for CDP response to {method}")

    def recv_json(self, timeout: float) -> dict[str, Any] | None:
        frame = self._recv_frame(timeout)
        if frame is None:
            return None
        return json.loads(frame.decode("utf-8"))

    def pop_singbox_navigation_url(self) -> str | None:
        kept = []
        found = None
        for event in self.events:
            url = event.get("params", {}).get("url")
            if found is None and isinstance(url, str) and url.startswith("sing-box://"):
                found = url
            else:
                kept.append(event)
        self.events = kept
        return found

    def _send_json(self, payload: dict[str, Any]) -> None:
        self._send_frame(json.dumps(payload, separators=(",", ":")).encode("utf-8"), opcode=0x1)

    def _send_frame(self, payload: bytes, opcode: int) -> None:
        if self.sock is None:
            raise ExtractionError("CDP WebSocket is not connected")

        first = 0x80 | opcode
        length = len(payload)
        if length < 126:
            header = bytes([first, 0x80 | length])
        elif length < 65536:
            header = bytes([first, 0x80 | 126]) + struct.pack("!H", length)
        else:
            header = bytes([first, 0x80 | 127]) + struct.pack("!Q", length)

        mask = os.urandom(4)
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def _recv_frame(self, timeout: float) -> bytes | None:
        if self.sock is None:
            raise ExtractionError("CDP WebSocket is not connected")

        self.sock.settimeout(timeout)
        try:
            first_two = self._read_exact(2)
        except socket.timeout:
            return None

        first, second = first_two
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_exact(8))[0]

        mask = self._read_exact(4) if masked else b""
        payload = self._read_exact(length) if length else b""
        if masked:
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))

        if opcode == 0x8:
            raise ExtractionError("CDP WebSocket closed")
        if opcode == 0x9:
            self._send_frame(payload, opcode=0xA)
            return self._recv_frame(timeout)
        if opcode == 0xA:
            return self._recv_frame(timeout)
        if opcode not in {0x1, 0x0}:
            return self._recv_frame(timeout)
        return payload

    def _read_until(self, delimiter: bytes, timeout: float) -> bytes:
        if self.sock is None:
            raise ExtractionError("CDP WebSocket is not connected")
        self.sock.settimeout(timeout)
        data = bytearray()
        while delimiter not in data:
            chunk = self.sock.recv(1)
            if not chunk:
                raise ExtractionError("CDP WebSocket ended during handshake")
            data.extend(chunk)
        return bytes(data)

    def _read_exact(self, length: int) -> bytes:
        if self.sock is None:
            raise ExtractionError("CDP WebSocket is not connected")
        chunks = bytearray()
        while len(chunks) < length:
            chunk = self.sock.recv(length - len(chunks))
            if not chunk:
                raise ExtractionError("CDP WebSocket ended unexpectedly")
            chunks.extend(chunk)
        return bytes(chunks)


def main() -> int:
    args = parse_args()
    try:
        targets = list_targets(args.cdp_url)
        if args.list_pages:
            print_targets(targets, file=sys.stderr)

        target = find_baipiao_target(targets, args.tab_url_contains)
        if target is None:
            target = open_console_target(args.cdp_url, args.url)

        websocket_url = target.get("webSocketDebuggerUrl")
        if not websocket_url:
            raise ExtractionError("selected BaiPiao target has no webSocketDebuggerUrl")

        with CdpConnection(websocket_url) as cdp:
            cdp.call("Page.enable")
            cdp.call("Runtime.enable")
            cdp.call("Page.bringToFront")
            ensure_console_page(cdp, args.url, args.timeout_ms)
            subscription_url = capture_subscription_url(cdp, args.timeout_ms)

        if args.import_url:
            output = build_singbox_import_url(subscription_url, args.provider_name)
        else:
            output = subscription_url

        write_or_print(
            output,
            args.output,
            args.append,
            provider_name=args.provider_name,
            raw=args.raw,
        )

        if args.verbose:
            print(f"subscription_url={redact_token(subscription_url)}", file=sys.stderr)
            if args.import_url:
                print(f"import_url={redact_token(output)}", file=sys.stderr)

        return 0
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Extract the BaiPiao sing-box subscription URL from a CDP Chrome tab.",
    )
    parser.add_argument(
        "--cdp-url",
        default=DEFAULT_CDP_URL,
        help=f"Chrome DevTools endpoint. Default: {DEFAULT_CDP_URL}",
    )
    parser.add_argument(
        "--url",
        default=DEFAULT_CONSOLE_URL,
        help=f"BaiPiao console URL to open if no matching tab is found. Default: {DEFAULT_CONSOLE_URL}",
    )
    parser.add_argument(
        "--timeout-ms",
        type=int,
        default=20000,
        help="Timeout for page waits and URL capture. Default: 20000",
    )
    parser.add_argument(
        "--tab-url-contains",
        default="xn--mesv7f5toqlp",
        help="Substring used to identify the BaiPiao tab URL. Default: xn--mesv7f5toqlp",
    )
    parser.add_argument(
        "--list-pages",
        action="store_true",
        help="Print CDP page targets to stderr before extracting.",
    )
    parser.add_argument(
        "--import-url",
        action="store_true",
        help="Output the full sing-box:// import URL instead of the decoded HTTPS subscription URL.",
    )
    parser.add_argument(
        "--provider-name",
        default=DEFAULT_PROVIDER_NAME,
        help=f"Provider name used when writing '<provider name> = <url>' to --output. Default: {DEFAULT_PROVIDER_NAME}",
    )
    parser.add_argument(
        "--raw",
        action="store_true",
        help="Write/print only the URL, without '<provider name> = '. Stdout is raw by default unless this flag is combined with --output.",
    )
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        help="Write the extracted URL to this file instead of stdout.",
    )
    parser.add_argument(
        "--append",
        action="store_true",
        help="Append to --output instead of replacing it.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print redacted extraction details to stderr.",
    )
    return parser.parse_args()


def cdp_http_json(cdp_url: str, path: str, method: str = "GET") -> Any:
    url = f"{cdp_url.rstrip('/')}{path}"
    request = Request(url, method=method)
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read().decode("utf-8"))


def list_targets(cdp_url: str) -> list[dict[str, Any]]:
    targets = cdp_http_json(cdp_url, "/json/list")
    if not isinstance(targets, list):
        raise ExtractionError("CDP /json/list did not return a target list")
    return targets


def open_console_target(cdp_url: str, console_url: str) -> dict[str, Any]:
    path = f"/json/new?{quote(console_url, safe=':/?=&')}"
    try:
        target = cdp_http_json(cdp_url, path, method="PUT")
    except Exception:
        target = cdp_http_json(cdp_url, path, method="GET")
    if not isinstance(target, dict):
        raise ExtractionError("CDP /json/new did not return a page target")
    return target


def find_baipiao_target(
    targets: list[dict[str, Any]],
    url_hint: str,
) -> dict[str, Any] | None:
    matches: list[tuple[int, dict[str, Any]]] = []
    for target in targets:
        if target.get("type") != "page":
            continue
        title = str(target.get("title") or "")
        url = str(target.get("url") or "")
        score = baipiao_target_score(title, url, url_hint)
        if score > 0:
            matches.append((score, target))
    matches.sort(key=lambda item: item[0], reverse=True)
    return matches[0][1] if matches else None


def print_targets(targets: list[dict[str, Any]], file: Any) -> None:
    for index, target in enumerate(targets):
        if target.get("type") != "page":
            continue
        title = str(target.get("title") or "")
        url = str(target.get("url") or "")
        print(f"[{index}] {title} {url}", file=file)


def baipiao_target_score(title: str, url: str, url_hint: str) -> int:
    parsed = urlparse(url)
    if parsed.scheme not in {"http", "https"}:
        return 0

    score = 0
    host = parsed.netloc.lower()
    lower_url = url.lower()
    lower_title = title.lower()
    decoded_host = decode_idna(host)
    if "xn--mesv7f5toqlp" in host or "白嫖机场" in decoded_host:
        score += 12
    if parsed.path.rstrip("/") == "/console":
        score += 8
    if url_hint.lower() in lower_url:
        score += 5
    if "白嫖机场" in title or "baipiao" in lower_title:
        score += 3
    return score


def decode_idna(value: str) -> str:
    try:
        return value.encode("ascii").decode("idna")
    except UnicodeError:
        return value


def capture_subscription_url(cdp: CdpConnection, timeout_ms: int) -> str:
    subscription_url = fetch_subscription_url_from_page_api(cdp, timeout_ms)
    if subscription_url:
        return subscription_url

    snapshot = evaluate(cdp, js_collect_page_snapshot(), timeout=timeout_ms / 1000)
    for _label, text in snapshot.get("snippets", []):
        subscription_url = extract_subscription_url_from_text(str(text))
        if subscription_url:
            return subscription_url

    import_url = capture_import_url_from_interaction(cdp, timeout_ms)
    if import_url:
        subscription_url, _profile = decode_singbox_import_url(import_url)
        return subscription_url

    snapshot = evaluate(cdp, js_collect_page_snapshot(), timeout=timeout_ms / 1000)
    for _label, text in snapshot.get("snippets", []):
        subscription_url = extract_subscription_url_from_text(str(text))
        if subscription_url:
            return subscription_url

    body = clean_space(str(snapshot.get("body", "")))
    raise ExtractionError(
        "failed to find a BaiPiao sing-box subscription URL in the authenticated tab"
        + (f"; page={truncate(body, 280)}" if body else "")
    )


def fetch_subscription_url_from_page_api(cdp: CdpConnection, timeout_ms: int) -> str | None:
    result = evaluate(cdp, js_fetch_subscribe_api(), timeout=timeout_ms / 1000)
    if not isinstance(result, dict):
        return None
    subscription_url = result.get("subscribe_url")
    if isinstance(subscription_url, str) and is_probable_subscription_url(subscription_url):
        return subscription_url
    return None


def ensure_console_page(cdp: CdpConnection, console_url: str, timeout_ms: int) -> None:
    current_url = evaluate(
        cdp,
        "(() => window.location.href)()",
        timeout=min(5.0, timeout_ms / 1000),
    )
    if same_normalized_url(str(current_url), console_url):
        return

    cdp.call("Page.navigate", {"url": console_url}, timeout=min(5.0, timeout_ms / 1000))
    deadline = time.monotonic() + timeout_ms / 1000
    while time.monotonic() < deadline:
        try:
            state = evaluate(
                cdp,
                "(() => ({ url: window.location.href, ready: document.readyState }))()",
                timeout=1.0,
            )
        except ExtractionError:
            time.sleep(0.2)
            continue
        if (
            isinstance(state, dict)
            and same_normalized_url(str(state.get("url", "")), console_url)
            and state.get("ready") != "loading"
        ):
            return
        time.sleep(0.2)
    raise ExtractionError(f"timed out waiting for BaiPiao console page: {console_url}")


def same_normalized_url(left: str, right: str) -> bool:
    left_parsed = urlparse(left)
    right_parsed = urlparse(right)
    return (
        left_parsed.scheme == right_parsed.scheme
        and left_parsed.netloc == right_parsed.netloc
        and left_parsed.path.rstrip("/") == right_parsed.path.rstrip("/")
    )


def capture_import_url_from_interaction(cdp: CdpConnection, timeout_ms: int) -> str | None:
    interaction = evaluate(cdp, js_click_subscription_controls(), timeout=timeout_ms / 1000)
    if isinstance(interaction, dict):
        for key in ("clipboard", "opened", "location"):
            value = interaction.get(key)
            if isinstance(value, str) and value.startswith("sing-box://"):
                return value
            if isinstance(value, str):
                subscription_url = extract_subscription_url_from_text(value)
                if subscription_url:
                    return build_singbox_import_url(subscription_url, DEFAULT_PROVIDER_NAME)

    deadline = time.monotonic() + timeout_ms / 1000
    while time.monotonic() < deadline:
        url = cdp.pop_singbox_navigation_url()
        if url:
            return url
        message = cdp.recv_json(timeout=0.1)
        if message and "method" in message:
            cdp.events.append(message)
    return None


def evaluate(cdp: CdpConnection, expression: str, timeout: float) -> Any:
    result = cdp.call(
        "Runtime.evaluate",
        {
            "expression": expression,
            "awaitPromise": True,
            "returnByValue": True,
            "userGesture": True,
        },
        timeout=timeout,
    )
    if "exceptionDetails" in result:
        raise ExtractionError(f"page evaluation failed: {format_exception_details(result['exceptionDetails'])}")
    remote_object = result.get("result", {})
    return remote_object.get("value")


def format_exception_details(detail: dict[str, Any]) -> str:
    exception = detail.get("exception") or {}
    candidates = [
        exception.get("description"),
        exception.get("value"),
        detail.get("text"),
    ]
    message = next((str(item) for item in candidates if item), "unknown JavaScript error")
    line = detail.get("lineNumber")
    column = detail.get("columnNumber")
    if line is not None and column is not None:
        message = f"{message} at line {line}, column {column}"
    return message


def js_collect_page_snapshot() -> str:
    return r"""(() => {
        const clean = (value) => (value || "").replace(/\s+/g, " ").trim();
        const snippets = [];
        const push = (label, value) => {
            if (value !== undefined && value !== null && String(value).trim()) {
                snippets.push([label, String(value)]);
            }
        };

        push("location", window.location.href);
        push("body", document.body ? document.body.innerText : "");

        for (const storage of [window.localStorage, window.sessionStorage]) {
            if (!storage) continue;
            for (let i = 0; i < storage.length; i++) {
                const key = storage.key(i);
                if (/sub|subscribe|sing|clash|token|url/i.test(key || "")) {
                    push("storage " + key, storage.getItem(key));
                }
            }
        }

        for (const el of Array.from(document.querySelectorAll("a, button, [role='button'], [onclick]")).slice(0, 250)) {
            const text = clean(el.innerText || el.textContent || el.value || el.title || "");
            const onclick = el.getAttribute("onclick") || "";
            const href = el.getAttribute("href") || "";
            const klass = el.getAttribute("class") || "";
            const html = el.outerHTML || "";
            if (/sing|box|sub|subscribe|订阅|导入|复制|客户端|一键/i.test(text + " " + onclick + " " + href + " " + klass + " " + html)) {
                push("node", [text, onclick, href, klass, html].join("\n"));
            }
        }

        return {
            title: document.title,
            url: window.location.href,
            body: clean(document.body ? document.body.innerText : "").slice(0, 1600),
            snippets,
        };
    })()"""


def js_fetch_subscribe_api() -> str:
    return r"""(async () => {
        const base = localStorage.getItem("base_url") || window.location.origin;
        const token = localStorage.getItem("idcnlink-token") || "";
        if (!base || !token) {
            return {};
        }

        const response = await fetch(base.replace(/\/$/, "") + "/user/getSubscribe", {
            headers: {
                authorization: token,
            },
        });
        if (!response.ok) {
            return { status: response.status };
        }
        const payload = await response.json();
        return {
            status: response.status,
            subscribe_url: payload && payload.data ? payload.data.subscribe_url : "",
        };
    })()"""


def js_click_subscription_controls() -> str:
    return r"""(async () => {
        const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));
        const clean = (value) => (value || "").replace(/\s+/g, " ").trim();
        const capture = { clipboard: "", opened: "", location: "" };

        if (navigator.clipboard && navigator.clipboard.writeText) {
            const originalWriteText = navigator.clipboard.writeText.bind(navigator.clipboard);
            navigator.clipboard.writeText = async (value) => {
                capture.clipboard = String(value);
                return originalWriteText(value);
            };
        }

        const originalOpen = window.open;
        window.open = (...args) => {
            capture.opened = args[0] ? String(args[0]) : "";
            return originalOpen.apply(window, args);
        };

        const isVisible = (el) => {
            const rect = el.getBoundingClientRect();
            const style = window.getComputedStyle(el);
            return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
        };
        const descriptor = (el) => [
            clean(el.innerText || el.textContent || el.value || el.title || ""),
            el.getAttribute("href") || "",
            el.getAttribute("onclick") || "",
            el.getAttribute("class") || "",
            el.outerHTML || ""
        ].join(" ");
        const click = async (el) => {
            el.scrollIntoView({ block: "center", inline: "center" });
            await sleep(120);
            el.click();
            await sleep(1200);
            capture.location = window.location.href;
            return capture.clipboard || capture.opened;
        };
        const candidates = () => Array
            .from(document.querySelectorAll("a, button, [role='button'], [onclick]"))
            .filter(isVisible)
            .map(el => ({ el, text: descriptor(el) }));
        const find = (regex) => candidates().filter(item => regex.test(item.text));

        const singBoxItems = find(/sing\s*-?\s*box|singbox|导入.*sing|sing.*导入/i);
        for (const item of singBoxItems) {
            const value = await click(item.el);
            if (value) return capture;
        }

        return capture;
    })()"""


def extract_subscription_url_from_text(text: str) -> str | None:
    normalized = normalize_js_text(text)

    import_match = re.search(
        r"sing-box://import-remote-profile\?url=([^\s\"'`<>]+)",
        normalized,
        flags=re.IGNORECASE,
    )
    if import_match:
        values = parse_qs(urlparse(import_match.group(0)).query).get("url")
        if values:
            candidate = values[0]
            if is_probable_subscription_url(candidate):
                return candidate

    for match in re.finditer(r"https?://[^\s\"'`<>]+", normalized, flags=re.IGNORECASE):
        candidate = trim_url_candidate(match.group(0))
        if is_probable_singbox_subscription_url(candidate):
            return candidate

    return None


def normalize_js_text(text: str) -> str:
    return (
        html.unescape(text)
        .replace("\\/", "/")
        .replace("\\u002F", "/")
        .replace("\\x2F", "/")
        .replace("\\u003D", "=")
        .replace("\\x3D", "=")
        .replace("\\u0026", "&")
        .replace("\\x26", "&")
        .replace("\\u003A", ":")
        .replace("\\x3A", ":")
    )


def trim_url_candidate(value: str) -> str:
    return value.rstrip(".,;:)\\]}\"'")


def is_probable_subscription_url(candidate: str) -> bool:
    parsed = urlparse(candidate)
    return parsed.scheme in {"http", "https"} and bool(parsed.netloc)


def is_probable_singbox_subscription_url(candidate: str) -> bool:
    if not is_probable_subscription_url(candidate):
        return False
    parsed = urlparse(candidate)
    haystack = " ".join(
        [
            parsed.path,
            parsed.query,
            " ".join(f"{key}={value}" for key, value in parse_qs(parsed.query).items()),
        ]
    ).lower()
    return any(
        marker in haystack
        for marker in (
            "singbox",
            "sing-box",
            "sing_box",
            "target=sing",
            "flag=sing",
            "client=sing",
            "type=sing",
        )
    )


def build_singbox_import_url(subscription_url: str, provider_name: str) -> str:
    return (
        "sing-box://import-remote-profile?url="
        + quote(subscription_url, safe="")
        + "#"
        + quote(provider_name, safe="")
    )


def decode_singbox_import_url(import_url: str) -> tuple[str, str]:
    parsed = urlparse(import_url)
    if parsed.scheme != "sing-box":
        raise ExtractionError(f"not a sing-box import URL: {redact_token(import_url)}")

    values = parse_qs(parsed.query).get("url")
    if not values:
        raise ExtractionError("sing-box import URL did not contain a url= query parameter")

    subscription_url = values[0]
    if not is_probable_subscription_url(subscription_url):
        raise ExtractionError(f"decoded Sing-box subscription URL is invalid: {redact_token(subscription_url)}")
    return subscription_url, unquote(parsed.fragment)


def write_or_print(
    value: str,
    output: Path | None,
    append: bool,
    provider_name: str,
    raw: bool,
) -> None:
    if output is None:
        print(value)
        return

    line = value if raw else f"{provider_name} = {value}"
    mode = "a" if append else "w"
    with output.open(mode, encoding="utf-8") as handle:
        handle.write(line)
        handle.write("\n")


def redact_token(url: str) -> str:
    parsed = urlparse(url)
    if parsed.scheme == "sing-box":
        values = parse_qs(parsed.query)
        inner = values.get("url", [""])[0]
        return f"sing-box://import-remote-profile?url={redact_token(inner)}#{parsed.fragment}"

    pairs = parse_qs(parsed.query, keep_blank_values=True)
    if pairs:
        redacted_pairs = []
        for key, values in pairs.items():
            value = "<REDACTED>" if is_secret_key(key) else values[-1]
            redacted_pairs.append((key, value))
        path = parsed.path
        query = "&".join(f"{key}={quote(value, safe='')}" for key, value in redacted_pairs)
        return parsed._replace(path=redact_token_path(path), query=query).geturl()
    return parsed._replace(path=redact_token_path(parsed.path)).geturl()


def is_secret_key(key: str) -> bool:
    return key.lower() in {"token", "access_token", "sub_token", "key", "auth", "password"}


def redact_token_path(path: str) -> str:
    return re.sub(r"(/(?:link|sub|subscribe|subscription)/)[^/?#]+", r"\1<REDACTED>", path)


def clean_space(value: str) -> str:
    return re.sub(r"\s+", " ", value).strip()


def truncate(value: str, max_len: int) -> str:
    return value if len(value) <= max_len else value[: max_len - 3] + "..."


if __name__ == "__main__":
    raise SystemExit(main())
