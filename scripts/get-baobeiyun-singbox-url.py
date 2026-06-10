#!/usr/bin/env python3
"""
Extract the Baobeiyun sing-box subscription URL from an authenticated Chrome tab.

This script has no third-party Python dependencies. It talks directly to an
already-running Chrome DevTools Protocol (CDP) endpoint.

Start Chrome with CDP enabled first, for example:
  open -n -a "Google Chrome" --args \
    --user-data-dir=/tmp/chrome-cdp-profile-9229 \
    --remote-debugging-address=127.0.0.1 \
    --remote-debugging-port=9229 \
    --remote-allow-origins='*' \
    --new-window 'https://web1.bby004.com/#/dashboard'

Then log in to Baobeiyun in that Chrome window. This script prints the direct
HTTPS subscription URL by default. When writing to a file, it writes
`<provider name> = <url>` unless `--raw` is passed. Do not commit the printed
URL or any output file that contains the token.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import socket
import ssl
import struct
import sys
import time
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, quote, unquote, urlparse
from urllib.request import ProxyHandler, Request, build_opener

from cdp_wsl import (
    CDP_URL_ENV,
    WINDOWS_RELAY_ENV,
    WINDOWS_HOST_ENV,
    rewrite_loopback_websocket_url,
    wsl_windows_cdp_url,
)

DEFAULT_CDP_URL = os.environ.get(CDP_URL_ENV, "http://127.0.0.1:9229")
DEFAULT_DASHBOARD_URL = "https://web1.bby004.com/#/dashboard"


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
            data.extend(self.sock.recv(1))
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
        relay = args.cdp_windows_relay or os.environ.get(WINDOWS_RELAY_ENV) == "1"
        with wsl_windows_cdp_url(
            args.cdp_url,
            enabled=args.cdp_windows,
            windows_host=args.windows_host,
            relay=relay,
        ) as cdp_url:
            targets = list_targets(cdp_url)
            if args.list_pages or args.list_pages_only:
                print_targets(targets, file=sys.stderr)
            if args.list_pages_only:
                return 0

            target = find_baobeiyun_target(targets, args.tab_url_contains)
            if target is None:
                target = open_dashboard_target(cdp_url, args.dashboard_url)

            websocket_url = target.get("webSocketDebuggerUrl")
            if not websocket_url:
                raise ExtractionError("selected Baobeiyun target has no webSocketDebuggerUrl")
            websocket_url = rewrite_loopback_websocket_url(websocket_url, cdp_url)

            with CdpConnection(websocket_url) as cdp:
                cdp.call("Page.enable")
                cdp.call("Runtime.enable")
                cdp.call("Page.bringToFront")

                singbox_import_url = capture_singbox_import_url(cdp, args.timeout_ms)
                subscription_url, profile_name = decode_singbox_import_url(singbox_import_url)

        if args.import_url:
            write_or_print(
                singbox_import_url,
                args.output,
                provider_name=args.provider_name,
                raw=args.raw,
            )
        else:
            write_or_print(
                subscription_url,
                args.output,
                provider_name=args.provider_name,
                raw=args.raw,
            )

        if args.verbose:
            print(f"profile={profile_name}", file=sys.stderr)
            print(f"import_url={redact_token(singbox_import_url)}", file=sys.stderr)
            print(f"subscription_url={redact_token(subscription_url)}", file=sys.stderr)

        return 0
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Extract the Baobeiyun sing-box subscription URL from a CDP Chrome tab.",
    )
    parser.add_argument(
        "--cdp-url",
        default=DEFAULT_CDP_URL,
        help=(
            f"Chrome DevTools endpoint. Default: {DEFAULT_CDP_URL}. "
            f"Can also be set with ${CDP_URL_ENV}."
        ),
    )
    parser.add_argument(
        "--cdp-windows",
        action="store_true",
        help=(
            "Treat a loopback --cdp-url as a Windows-hosted CDP endpoint from WSL. "
            "The script resolves the Windows host IP and rewrites loopback debugger "
            "WebSocket URLs returned by Chrome."
        ),
    )
    parser.add_argument(
        "--windows-host",
        help=(
            "Windows host/IP to use with --cdp-windows. "
            f"Default: ${WINDOWS_HOST_ENV}, then WSL default gateway."
        ),
    )
    parser.add_argument(
        "--cdp-windows-relay",
        action="store_true",
        help=(
            "Start a temporary Windows PowerShell TCP relay from the WSL-visible "
            "Windows host to Windows 127.0.0.1. Use this when Windows Chrome's "
            "CDP port is loopback-only. Can also be enabled with "
            f"{WINDOWS_RELAY_ENV}=1."
        ),
    )
    parser.add_argument(
        "--dashboard-url",
        default=DEFAULT_DASHBOARD_URL,
        help=f"Baobeiyun dashboard URL to open if no matching tab is found. Default: {DEFAULT_DASHBOARD_URL}",
    )
    parser.add_argument(
        "--timeout-ms",
        type=int,
        default=20000,
        help="Timeout for page waits and URL capture. Default: 20000",
    )
    parser.add_argument(
        "--tab-url-contains",
        default="bby",
        help="Substring used to identify the Baobeiyun tab URL. Default: bby",
    )
    parser.add_argument(
        "--list-pages",
        action="store_true",
        help="Print CDP page targets to stderr before extracting.",
    )
    parser.add_argument(
        "--list-pages-only",
        action="store_true",
        help="Print CDP page targets to stderr, then exit without extracting.",
    )
    parser.add_argument(
        "--import-url",
        action="store_true",
        help="Output the full sing-box:// import URL instead of the decoded HTTPS subscription URL.",
    )
    parser.add_argument(
        "--provider-name",
        default="宝贝云",
        help="Provider name used when writing '<provider name> = <url>' to --output. Default: 宝贝云",
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
        help="Append the extracted URL to this file instead of printing to stdout.",
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
    opener = build_opener(ProxyHandler({}))
    with opener.open(request, timeout=10) as response:
        return json.loads(response.read().decode("utf-8"))


def list_targets(cdp_url: str) -> list[dict[str, Any]]:
    targets = cdp_http_json(cdp_url, "/json/list")
    if not isinstance(targets, list):
        raise ExtractionError("CDP /json/list did not return a target list")
    return targets


def open_dashboard_target(cdp_url: str, dashboard_url: str) -> dict[str, Any]:
    path = f"/json/new?{quote(dashboard_url, safe=':/?=&')}"
    try:
        target = cdp_http_json(cdp_url, path, method="PUT")
    except Exception:
        target = cdp_http_json(cdp_url, path, method="GET")
    if not isinstance(target, dict):
        raise ExtractionError("CDP /json/new did not return a page target")
    return target


def find_baobeiyun_target(
    targets: list[dict[str, Any]],
    url_hint: str,
) -> dict[str, Any] | None:
    matches: list[tuple[int, dict[str, Any]]] = []
    for target in targets:
        if target.get("type") != "page":
            continue
        title = str(target.get("title") or "")
        url = str(target.get("url") or "")
        score = baobeiyun_target_score(title, url, url_hint)
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


def page_matches_baobeiyun(title: str, url: str, url_hint: str) -> bool:
    return baobeiyun_target_score(title, url, url_hint) > 0


def baobeiyun_target_score(title: str, url: str, url_hint: str) -> int:
    parsed = urlparse(url)
    if parsed.scheme not in {"http", "https"}:
        return 0

    score = 0
    title_match = "宝贝云" in title or "baobeiyun" in title.lower()
    url_match = url_hint.lower() in url.lower()
    dashboard_match = "web1.bby004.com" in url or "/#/dashboard" in url
    if dashboard_match:
        score += 10
    if url_match:
        score += 5
    if title_match:
        score += 3
    return score


def capture_singbox_import_url(cdp: CdpConnection, timeout_ms: int) -> str:
    deadline = time.monotonic() + timeout_ms / 1000

    result = evaluate(cdp, js_open_one_click_modal(), timeout=timeout_ms / 1000)
    if not result.get("ok"):
        auth_hint = ""
        if not safe_has_authorization(cdp):
            auth_hint = " localStorage.authorization is also missing; the tab may not be logged in."
        raise ExtractionError(f"{result.get('reason')}: {result.get('body', '')}{auth_hint}")

    wait_for_js(cdp, "Boolean(document.querySelector('.sing-box'))", deadline)
    clicked = evaluate(cdp, js_click_singbox_item(), timeout=timeout_ms / 1000)
    if not clicked:
        raise ExtractionError("could not find .sing-box modal item")

    while time.monotonic() < deadline:
        url = cdp.pop_singbox_navigation_url()
        if url:
            return url
        message = cdp.recv_json(timeout=0.1)
        if message and "method" in message:
            cdp.events.append(message)

    raise ExtractionError("timed out waiting for sing-box:// navigation event")


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


def safe_has_authorization(cdp: CdpConnection) -> bool:
    try:
        return bool(evaluate(cdp, js_has_authorization(), timeout=2))
    except Exception:
        return False


def wait_for_js(cdp: CdpConnection, expression: str, deadline: float) -> None:
    while time.monotonic() < deadline:
        if evaluate(cdp, f"(() => {expression})()", timeout=2):
            return
        time.sleep(0.1)
    raise ExtractionError(f"timed out waiting for page condition: {expression}")


def js_has_authorization() -> str:
    return """(() => {
        return Boolean(
            localStorage.getItem("authorization")
            || sessionStorage.getItem("authorization")
        );
    })()"""


def js_open_one_click_modal() -> str:
    return """(() => {
        const clean = (value) => (value || "").replace(/\\s+/g, " ").trim();
        const visible = (el) => {
            const rect = el.getBoundingClientRect();
            return rect.width > 0 && rect.height > 0;
        };
        const clickElement = (el) => {
            const rect = el.getBoundingClientRect();
            const eventInit = {
                bubbles: true,
                cancelable: true,
                view: window,
                clientX: rect.left + rect.width / 2,
                clientY: rect.top + rect.height / 2,
            };
            for (const type of ["mousedown", "mouseup", "click"]) {
                el.dispatchEvent(new MouseEvent(type, eventInit));
            }
        };

        const existingSingbox = document.querySelector(".sing-box");
        if (existingSingbox && visible(existingSingbox)) {
            return { ok: true, reason: "modal already open" };
        }

        const shortcut = Array.from(document.querySelectorAll(".v2board-shortcuts-item"))
            .find((el) => {
                const text = clean(el.innerText || el.textContent);
                return text.startsWith("一键订阅") || text.includes("快速将节点导入");
            });

        if (!shortcut) {
            return {
                ok: false,
                reason: "could not find the one-click subscription shortcut",
                body: clean(document.body.innerText).slice(0, 500),
            };
        }

        clickElement(shortcut);
        return { ok: true, reason: "opened modal" };
    })()"""


def js_click_singbox_item() -> str:
    return """(() => {
        const el = document.querySelector(".sing-box");
        if (!el) {
            return false;
        }
        const rect = el.getBoundingClientRect();
        const eventInit = {
            bubbles: true,
            cancelable: true,
            view: window,
            clientX: rect.left + rect.width / 2,
            clientY: rect.top + rect.height / 2,
        };
        for (const type of ["mousedown", "mouseup", "click"]) {
            el.dispatchEvent(new MouseEvent(type, eventInit));
        }
        return true;
    })()"""


def decode_singbox_import_url(import_url: str) -> tuple[str, str]:
    parsed = urlparse(import_url)
    if parsed.scheme != "sing-box":
        raise ExtractionError(f"not a sing-box import URL: {redact_token(import_url)}")

    values = parse_qs(parsed.query).get("url")
    if not values:
        raise ExtractionError("sing-box import URL did not contain a url= query parameter")

    subscription_url = values[0]
    subscription = urlparse(subscription_url)
    if subscription.scheme not in {"http", "https"} or not subscription.netloc:
        raise ExtractionError(
            "decoded Sing-box subscription URL is invalid "
            f"({subscription_url!r}). The Baobeiyun page likely is not logged in, "
            "or its subscription data has not loaded yet; reload the dashboard and try again."
        )

    return subscription_url, unquote(parsed.fragment)


def write_or_print(
    value: str,
    output: Path | None,
    provider_name: str,
    raw: bool,
) -> None:
    if output is None:
        print(value)
        return

    line = value if raw else f"{provider_name} = {value}"
    with output.open("a", encoding="utf-8") as handle:
        handle.write(line)
        handle.write("\n")


def redact_token(url: str) -> str:
    parsed = urlparse(url)
    if parsed.scheme == "sing-box":
        query = parse_qs(parsed.query)
        inner = query.get("url", [""])[0]
        redacted_inner = redact_token(inner)
        return f"{parsed.scheme}://{parsed.netloc}{parsed.path}?url={redacted_inner}#{parsed.fragment}"

    values = parse_qs(parsed.query)
    if "token" not in values:
        return url
    return url.replace(values["token"][0], "<SUBSCRIBE_TOKEN>")


if __name__ == "__main__":
    raise SystemExit(main())
