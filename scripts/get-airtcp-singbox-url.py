#!/usr/bin/env python3
"""
Extract the AirTCP sing-box subscription URL from an authenticated Chrome tab.

This script has no third-party Python dependencies. It talks directly to an
already-running Chrome DevTools Protocol (CDP) endpoint, matching the
Baobeiyun extractor workflow.

Start Chrome with CDP enabled first, for example:
  open -n -a "Google Chrome" --args \
    --user-data-dir=/tmp/chrome-cdp-profile-9229 \
    --remote-debugging-address=127.0.0.1 \
    --remote-debugging-port=9229 \
    --remote-allow-origins='*' \
    --new-window 'https://5.airtcp.me/user'

Then log in to AirTCP in that Chrome window. This script prints the direct
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
DEFAULT_USER_URL = "https://5.airtcp.me/user"
DEFAULT_PROVIDER_NAME = "airtcp"


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

        target = find_airtcp_target(targets, args.tab_url_contains)
        if target is None:
            target = open_user_target(args.cdp_url, args.url)

        websocket_url = target.get("webSocketDebuggerUrl")
        if not websocket_url:
            raise ExtractionError("selected AirTCP target has no webSocketDebuggerUrl")

        with CdpConnection(websocket_url) as cdp:
            cdp.call("Page.enable")
            cdp.call("Runtime.enable")
            cdp.call("Page.bringToFront")
            subscription_url = capture_subscription_url(cdp, args.timeout_ms)

        if args.import_url:
            output = build_singbox_import_url(subscription_url)
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
        description="Extract the AirTCP sing-box subscription URL from a CDP Chrome tab.",
    )
    parser.add_argument(
        "--cdp-url",
        default=DEFAULT_CDP_URL,
        help=f"Chrome DevTools endpoint. Default: {DEFAULT_CDP_URL}",
    )
    parser.add_argument(
        "--url",
        default=DEFAULT_USER_URL,
        help=f"AirTCP user page URL to open if no matching tab is found. Default: {DEFAULT_USER_URL}",
    )
    parser.add_argument(
        "--timeout-ms",
        type=int,
        default=20000,
        help="Timeout for page waits and URL capture. Default: 20000",
    )
    parser.add_argument(
        "--tab-url-contains",
        default="airtcp",
        help="Substring used to identify the AirTCP tab URL. Default: airtcp",
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


def open_user_target(cdp_url: str, user_url: str) -> dict[str, Any]:
    path = f"/json/new?{quote(user_url, safe=':/?=&')}"
    try:
        target = cdp_http_json(cdp_url, path, method="PUT")
    except Exception:
        target = cdp_http_json(cdp_url, path, method="GET")
    if not isinstance(target, dict):
        raise ExtractionError("CDP /json/new did not return a page target")
    return target


def find_airtcp_target(
    targets: list[dict[str, Any]],
    url_hint: str,
) -> dict[str, Any] | None:
    matches: list[tuple[int, dict[str, Any]]] = []
    for target in targets:
        if target.get("type") != "page":
            continue
        title = str(target.get("title") or "")
        url = str(target.get("url") or "")
        score = airtcp_target_score(title, url, url_hint)
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


def airtcp_target_score(title: str, url: str, url_hint: str) -> int:
    parsed = urlparse(url)
    if parsed.scheme not in {"http", "https"}:
        return 0

    score = 0
    host = parsed.netloc.lower()
    lower_url = url.lower()
    lower_title = title.lower()
    if "airtcp" in host:
        score += 10
    if parsed.path.rstrip("/") == "/user":
        score += 8
    if url_hint.lower() in lower_url:
        score += 5
    if "airtcp" in lower_title:
        score += 3
    return score


def capture_subscription_url(cdp: CdpConnection, timeout_ms: int) -> str:
    snapshot = evaluate(cdp, js_collect_page_snapshot(), timeout=timeout_ms / 1000)
    for label, text in snapshot.get("snippets", []):
        subscription_url = extract_subscription_url_from_text(str(text))
        if subscription_url:
            return subscription_url

    import_url = capture_import_url_from_click(cdp, timeout_ms)
    if import_url:
        subscription_url, _profile = decode_singbox_import_url(import_url)
        return subscription_url

    body = clean_space(str(snapshot.get("body", "")))
    button = clean_space(str(snapshot.get("button", "")))
    raise ExtractionError(
        "failed to find an AirTCP sing-box subscription URL in the authenticated tab"
        + (f"; button={button}" if button else "")
        + (f"; page={truncate(body, 260)}" if body else "")
    )


def capture_import_url_from_click(cdp: CdpConnection, timeout_ms: int) -> str | None:
    clicked = evaluate(cdp, js_click_singbox_button(), timeout=timeout_ms / 1000)
    if not clicked:
        return None

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
        push("importSublink", window.importSublink ? String(window.importSublink) : "");
        push("oneclickImport", window.oneclickImport ? String(window.oneclickImport) : "");

        for (const el of document.querySelectorAll(".btn-singbox, [onclick*='singbox' i], a, button")) {
            const text = clean(el.innerText || el.textContent || el.value || el.title || "");
            const onclick = el.getAttribute("onclick") || "";
            const href = el.getAttribute("href") || "";
            const html = el.outerHTML || "";
            if (/sing|box|订阅|导入/i.test(text + " " + onclick + " " + href + " " + html)) {
                push("node", [text, onclick, href, html].join("\n"));
            }
        }

        for (const script of Array.from(document.scripts)) {
            push("script " + (script.src || "<inline>"), script.textContent || "");
        }

        const button = document.querySelector(".btn-singbox");
        return {
            title: document.title,
            url: window.location.href,
            body: clean(document.body ? document.body.innerText : "").slice(0, 1000),
            button: button ? clean(button.outerHTML || "") : "",
            snippets,
        };
    })()"""


def js_click_singbox_button() -> str:
    return r"""(() => {
        const el = document.querySelector(".btn-singbox, [onclick*='singbox' i]");
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
            if is_singbox_subscription_url(candidate):
                return candidate

    direct_match = re.search(
        r"https?://[^\s\"'`<>),;]+?singbox=1(?:[^\s\"'`<>),;]*)?",
        normalized,
        flags=re.IGNORECASE,
    )
    if direct_match:
        candidate = direct_match.group(0)
        if is_singbox_subscription_url(candidate):
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
    )


def is_singbox_subscription_url(candidate: str) -> bool:
    parsed = urlparse(candidate)
    query = parse_qs(parsed.query)
    return parsed.scheme in {"http", "https"} and bool(parsed.netloc) and query.get("singbox") == ["1"]


def build_singbox_import_url(subscription_url: str) -> str:
    return (
        "sing-box://import-remote-profile?url="
        + quote(subscription_url, safe="")
        + "#AirTCP"
    )


def decode_singbox_import_url(import_url: str) -> tuple[str, str]:
    parsed = urlparse(import_url)
    if parsed.scheme != "sing-box":
        raise ExtractionError(f"not a sing-box import URL: {redact_token(import_url)}")

    values = parse_qs(parsed.query).get("url")
    if not values:
        raise ExtractionError("sing-box import URL did not contain a url= query parameter")

    subscription_url = values[0]
    if not is_singbox_subscription_url(subscription_url):
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

    if "/link/" in parsed.path:
        redacted_path = re.sub(r"(/link/)[^/?#]+", r"\1<AIRTCP_LINK_TOKEN>", parsed.path)
        return parsed._replace(path=redacted_path).geturl()
    return url


def clean_space(value: str) -> str:
    return re.sub(r"\s+", " ", value).strip()


def truncate(value: str, max_len: int) -> str:
    return value if len(value) <= max_len else value[: max_len - 3] + "..."


if __name__ == "__main__":
    raise SystemExit(main())
