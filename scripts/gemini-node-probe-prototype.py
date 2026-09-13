"""PROTOTYPE — disposable Gemini per-node usability probe.

Question: can an isolated sing-box instance plus a fresh browser context classify
each configured node without changing the live selector or reusing old flows?

Run: python scripts/gemini-node-probe-prototype.py [--limit N]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TARGET_URL = "https://gemini.google.com/"
GROUPS = ("宝贝云", "白嫖机场")


def classify(html: str, exit_code: int) -> str:
    """Pure classification logic intended to survive beyond this shell."""
    folded = html.casefold()
    if "gemini 目前不支持你所在的地区" in folded or (
        "gemini" in folded and "isn\'t currently supported in your country" in folded
    ):
        return "region_blocked"
    if "异常流量" in html or "unusual traffic" in folded:
        return "google_abuse_blocked"
    if any(marker in folded for marker in ("err_proxy", "err_connection", "err_tunnel")):
        return "transport_failed"
    positive = (
        "发起新对话",
        "ask gemini",
        "rich-textarea",
        "input-area",
        "chat-input",
    )
    if any(marker in folded for marker in positive):
        return "usable"
    return "transport_failed" if exit_code else "reachable_unknown"


def classification_evidence(html: str) -> dict:
    folded = html.casefold()
    markers = ["发起新对话", "ask gemini", "rich-textarea", "input-area", "chat-input"]
    return {
        "positive_markers": [marker for marker in markers if marker in folded],
        "chinese_region_marker": "gemini 目前不支持你所在的地区" in folded,
        "english_region_marker": "isn't currently supported in your country" in folded,
    }


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def request_json(url: str, method: str = "GET", body: dict | None = None):
    payload = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        url,
        data=payload,
        method=method,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        data = response.read()
    return json.loads(data) if data else None


def wait_for_controller(base_url: str, process: subprocess.Popen, timeout: float = 12) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"isolated sing-box exited with {process.returncode}")
        try:
            request_json(base_url + "/proxies")
            return
        except (OSError, urllib.error.URLError):
            time.sleep(0.15)
    raise RuntimeError("isolated sing-box controller did not start")


def find_chrome() -> Path:
    candidates = [
        Path(os.environ.get("PROGRAMFILES", "")) / "Google/Chrome/Application/chrome.exe",
        Path(os.environ.get("PROGRAMFILES(X86)", "")) / "Google/Chrome/Application/chrome.exe",
        Path(os.environ.get("LOCALAPPDATA", "")) / "Google/Chrome/Application/chrome.exe",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise RuntimeError("Google Chrome was not found")


def build_config(source: dict, mixed_port: int, controller_port: int) -> tuple[dict, list[dict]]:
    by_tag = {item.get("tag"): item for item in source.get("outbounds", [])}
    nodes = []
    seen = set()
    for group in GROUPS:
        selector = by_tag.get(group, {})
        for tag in selector.get("outbounds", []):
            outbound = by_tag.get(tag)
            if not outbound or outbound.get("type") in {"selector", "urltest", "direct", "block"}:
                continue
            key = (group, tag)
            if key not in seen:
                nodes.append({"group": group, "node": tag})
                seen.add(key)

    concrete_tags = {item["node"] for item in nodes}
    concrete = [item for item in source.get("outbounds", []) if item.get("tag") in concrete_tags]
    concrete.extend(item for item in source.get("outbounds", []) if item.get("type") in {"direct", "block"})
    probe_selector = {
        "type": "selector",
        "tag": "gemini-probe",
        "outbounds": [item["node"] for item in nodes],
        "default": nodes[0]["node"],
        "interrupt_exist_connections": True,
    }
    config = {
        "log": {"level": "warn"},
        "inbounds": [{
            "type": "mixed",
            "tag": "gemini-probe-in",
            "listen": "127.0.0.1",
            "listen_port": mixed_port,
        }],
        "outbounds": concrete + [probe_selector],
        "route": {"final": "gemini-probe", "auto_detect_interface": True},
        "experimental": {"clash_api": {"external_controller": f"127.0.0.1:{controller_port}"}},
    }
    return config, nodes


def render_state(done: int, total: int, counts: dict[str, int], current: dict | None) -> None:
    os.system("cls" if os.name == "nt" else "clear")
    print("\033[1mPROTOTYPE — isolated Gemini node probe\033[0m")
    print(f"\033[1mprogress\033[0m: {done}/{total}")
    print(f"\033[1mcounts\033[0m: {json.dumps(counts, ensure_ascii=False, sort_keys=True)}")
    print(f"\033[1mcurrent\033[0m: {json.dumps(current, ensure_ascii=False) if current else '-'}")
    print("\n\033[2mEach node uses a fresh headless Chrome profile through an isolated sing-box.\033[0m")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--limit", type=int)
    parser.add_argument("--match")
    parser.add_argument("--timeout", type=float, default=18.0)
    parser.add_argument("--output", type=Path, default=ROOT / "gemini-node-probe-results.json")
    args = parser.parse_args()

    source = json.loads((ROOT / "config.json").read_text(encoding="utf-8"))
    mixed_port, controller_port = free_port(), free_port()
    config, nodes = build_config(source, mixed_port, controller_port)
    if args.match:
        nodes = [item for item in nodes if args.match.casefold() in item["node"].casefold()]
    if args.limit is not None:
        nodes = nodes[: args.limit]

    sing_box = Path(os.environ["LOCALAPPDATA"]) / "sing-box-tui/core/sing-box.exe"
    chrome = find_chrome()
    results = []
    counts: dict[str, int] = {}

    with tempfile.TemporaryDirectory(prefix="gemini-node-probe-") as temp_name:
        temp = Path(temp_name)
        config_path = temp / "config.json"
        config_path.write_text(json.dumps(config, ensure_ascii=False), encoding="utf-8")
        log_file = (temp / "sing-box.log").open("w", encoding="utf-8")
        process = subprocess.Popen(
            [str(sing_box), "run", "--config", str(config_path)],
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )
        try:
            base_url = f"http://127.0.0.1:{controller_port}"
            wait_for_controller(base_url, process)
            for index, item in enumerate(nodes, 1):
                request_json(
                    base_url + "/proxies/" + urllib.parse.quote("gemini-probe", safe=""),
                    "PUT",
                    {"name": item["node"]},
                )
                try:
                    proxy = urllib.request.ProxyHandler({
                        "http": f"http://127.0.0.1:{mixed_port}",
                        "https": f"http://127.0.0.1:{mixed_port}",
                    })
                    with urllib.request.build_opener(proxy).open(
                        "https://api4.ipify.org", timeout=12
                    ) as response:
                        egress_ipv4 = response.read().decode().strip()
                except OSError:
                    egress_ipv4 = None
                try:
                    with urllib.request.build_opener(proxy).open(
                        "https://api6.ipify.org", timeout=12
                    ) as response:
                        egress_ipv6 = response.read().decode().strip()
                except OSError:
                    egress_ipv6 = None
                profile = temp / f"chrome-{index}"
                command = [
                    str(chrome), "--headless=new", "--disable-gpu", "--disable-quic",
                    "--no-first-run", "--no-default-browser-check", "--lang=zh-CN",
                    f"--user-data-dir={profile}",
                    f"--proxy-server=http://127.0.0.1:{mixed_port}",
                    "--proxy-bypass-list=<-loopback>", "--virtual-time-budget=7000",
                    "--dump-dom", TARGET_URL,
                ]
                started = time.monotonic()
                try:
                    completed = subprocess.run(command, capture_output=True, timeout=args.timeout)
                    html = completed.stdout.decode("utf-8", errors="replace")
                    status = classify(html, completed.returncode)
                    diagnostic = {
                        "browser_exit_code": completed.returncode,
                        "rendered_bytes": len(completed.stdout),
                        "classification_evidence": classification_evidence(html),
                        "stderr_tail": completed.stderr.decode("utf-8", errors="replace")[-240:],
                    }
                except subprocess.TimeoutExpired as error:
                    html = (error.stdout or b"").decode("utf-8", errors="replace")
                    status = classify(html, 124) if html else "transport_failed"
                    diagnostic = {"browser_exit_code": 124, "rendered_bytes": len(error.stdout or b"")}
                shutil.rmtree(profile, ignore_errors=True)
                result = {
                    **item,
                    "status": status,
                    "elapsed_ms": round((time.monotonic() - started) * 1000),
                    "egress_ipv4": egress_ipv4,
                    "egress_ipv6": egress_ipv6,
                    "diagnostic": diagnostic,
                }
                results.append(result)
                counts[status] = counts.get(status, 0) + 1
                args.output.write_text(
                    json.dumps({"target": TARGET_URL, "results": results}, ensure_ascii=False, indent=2),
                    encoding="utf-8",
                )
                render_state(index, len(nodes), counts, result)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
            log_file.close()

    print(f"\nResults: {args.output}")
    for result in results:
        if result["status"] == "usable":
            print(f"USABLE  {result['group']} / {result['node']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
