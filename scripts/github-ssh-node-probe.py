#!/usr/bin/env python3
"""Assess whether each reachable sing-box node can speak GitHub SSH on TCP 22."""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


DEFAULT_PREFILTER_URL = "https://github.com/"
DEFAULT_TARGET_HOST = "github.com"
DEFAULT_TARGET_PORT = 22
PROBE_CANDIDATES_ENV = "SING_BOX_TUI_USABILITY_CANDIDATES"
MAX_HTTP_HEADER_BYTES = 16 * 1024
MAX_SSH_BANNER_LINES = 50


def configure_utf8_output() -> None:
    """Keep the TUI JSON Lines protocol UTF-8 on every Windows code page."""
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="strict")
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")


class ProbeInfrastructureError(RuntimeError):
    """A non-node-attributable failure that makes the whole run incomplete."""

    def __init__(self, code: str) -> None:
        super().__init__(code)
        self.code = code


class Manager:
    def __init__(self, executable: Path) -> None:
        try:
            self.process = subprocess.Popen(
                [str(executable), "node-runtime-manager", "--stdio"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                encoding="utf-8",
                bufsize=1,
            )
        except OSError as error:
            raise ProbeInfrastructureError("manager_start_failed") from error
        if self.process.stdin is None or self.process.stdout is None:
            raise ProbeInfrastructureError("manager_pipe_failed")
        self._request_id = 0

    def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._request_id += 1
        request = {"id": self._request_id, "method": method, "params": params}
        try:
            self.process.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
            self.process.stdin.flush()
            line = self.process.stdout.readline()
        except OSError as error:
            raise ProbeInfrastructureError("manager_io_failed") from error
        if not line:
            raise ProbeInfrastructureError("manager_exited")
        try:
            response = json.loads(line)
        except json.JSONDecodeError as error:
            raise ProbeInfrastructureError("manager_protocol_error") from error
        if response.get("id") != self._request_id:
            raise ProbeInfrastructureError("manager_protocol_error")
        if "error" in response:
            error = response["error"]
            code = error.get("code", "unknown") if isinstance(error, dict) else "unknown"
            raise ProbeInfrastructureError(f"manager_{safe_error_code(code)}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise ProbeInfrastructureError("manager_protocol_error")
        return result

    def close(self) -> None:
        try:
            if self.process.stdin and not self.process.stdin.closed:
                self.process.stdin.close()
        except OSError:
            self.process.kill()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def safe_error_code(value: Any) -> str:
    if not isinstance(value, str) or not (1 <= len(value) <= 64):
        return "unknown"
    if not all(character.isascii() and (character.isalnum() or character == "_") for character in value):
        return "unknown"
    return value


def runtime_fields(runtime: dict[str, Any]) -> tuple[str, str, list[str]]:
    runtime_id = runtime.get("runtime_id")
    proxy = runtime.get("proxy")
    proxy_url = proxy.get("http") if isinstance(proxy, dict) else None
    candidates = runtime.get("candidates")
    if (
        not isinstance(runtime_id, str)
        or not isinstance(proxy_url, str)
        or not isinstance(candidates, list)
        or not all(isinstance(node, str) for node in candidates)
    ):
        raise ProbeInfrastructureError("manager_protocol_error")
    return runtime_id, proxy_url, candidates


def candidate_node(candidate: dict[str, Any]) -> str | None:
    if candidate.get("end") is True:
        return None
    node = candidate.get("node")
    tag = node.get("tag") if isinstance(node, dict) else None
    if candidate.get("end") is not False or not isinstance(tag, str):
        raise ProbeInfrastructureError("manager_protocol_error")
    return tag


def candidate_scanned(candidate: dict[str, Any]) -> int:
    scanned = candidate.get("scanned")
    if not isinstance(scanned, int) or scanned < 0:
        raise ProbeInfrastructureError("manager_protocol_error")
    return scanned


def candidate_reachable(candidate: dict[str, Any]) -> bool:
    reachable = candidate.get("reachable")
    if not isinstance(reachable, bool):
        raise ProbeInfrastructureError("manager_protocol_error")
    return reachable


def selected_candidates() -> list[str] | None:
    encoded = os.environ.get(PROBE_CANDIDATES_ENV)
    if encoded is None:
        return None
    try:
        candidates = json.loads(encoded)
    except json.JSONDecodeError as error:
        raise ProbeInfrastructureError("candidate_scope_invalid") from error
    if (
        not isinstance(candidates, list)
        or not candidates
        or not all(isinstance(node, str) and node for node in candidates)
        or len(set(candidates)) != len(candidates)
    ):
        raise ProbeInfrastructureError("candidate_scope_invalid")
    return candidates


def read_until(stream: socket.socket, marker: bytes, limit: int, initial: bytes = b"") -> tuple[bytes, bytes]:
    data = initial
    while marker not in data:
        if len(data) >= limit:
            raise ValueError("response_too_large")
        chunk = stream.recv(min(4096, limit - len(data)))
        if not chunk:
            raise EOFError("unexpected_eof")
        data += chunk
    head, remainder = data.split(marker, 1)
    return head, remainder


def probe_github_ssh(proxy_url: str, target_host: str, target_port: int, timeout: float) -> tuple[bool, str]:
    parsed = urlsplit(proxy_url)
    if parsed.scheme != "http" or parsed.hostname is None or parsed.port is None:
        raise ProbeInfrastructureError("manager_protocol_error")
    started = time.monotonic()
    try:
        with socket.create_connection((parsed.hostname, parsed.port), timeout=timeout) as stream:
            stream.settimeout(timeout)
            authority = f"{target_host}:{target_port}"
            request = (
                f"CONNECT {authority} HTTP/1.1\r\n"
                f"Host: {authority}\r\n"
                "Proxy-Connection: close\r\n\r\n"
            ).encode("ascii")
            stream.sendall(request)
            header, remainder = read_until(stream, b"\r\n\r\n", MAX_HTTP_HEADER_BYTES)
            status_line = header.split(b"\r\n", 1)[0].decode("ascii", errors="replace")
            fields = status_line.split(" ", 2)
            if len(fields) < 2 or not fields[1].isdigit():
                return False, "invalid CONNECT response"
            if int(fields[1]) != 200:
                return False, f"CONNECT rejected ({fields[1]})"

            pending = remainder
            for _ in range(MAX_SSH_BANNER_LINES):
                line, pending = read_until(stream, b"\n", 1024, pending)
                banner = line.rstrip(b"\r").decode("ascii", errors="replace")
                if banner.startswith("SSH-"):
                    elapsed_ms = round((time.monotonic() - started) * 1000)
                    protocol = banner.split("-", 2)[:2]
                    return True, f"GitHub SSH banner {('-'.join(protocol))} in {elapsed_ms}ms"
            return False, "SSH banner not received"
    except TimeoutError:
        return False, "GitHub SSH timed out"
    except (ConnectionError, EOFError, OSError):
        return False, "GitHub SSH connection closed"
    except ValueError:
        return False, "invalid proxy response"


def emit(record: dict[str, Any]) -> None:
    print(json.dumps(record, ensure_ascii=False), flush=True)


def emit_incomplete(code: str) -> None:
    emit({"type": "summary", "complete": False, "message": f"GitHub SSH probe incomplete ({code})"})
    print(f"GitHub SSH probe incomplete: {code}", file=sys.stderr, flush=True)


def main() -> int:
    configure_utf8_output()
    parser = argparse.ArgumentParser(description="Test GitHub SSH over TCP 22 through every reachable node.")
    parser.add_argument("--manager", type=Path, default=Path("sing-box-tui"))
    parser.add_argument("--config", type=Path)
    parser.add_argument("--sing-box", type=Path)
    parser.add_argument("--url", default=DEFAULT_PREFILTER_URL)
    parser.add_argument("--host", default=DEFAULT_TARGET_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_TARGET_PORT)
    parser.add_argument("--timeout", type=float, default=5.0)
    args = parser.parse_args()
    if (args.config is None) != (args.sing_box is None):
        parser.error("--config and --sing-box must be supplied together")
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    if not (1 <= args.port <= 65535):
        parser.error("--port must be between 1 and 65535")

    manager: Manager | None = None
    runtime_id: str | None = None
    assessed = usable = tcp_total = 0
    try:
        emit({"type": "progress", "message": "Starting GitHub SSH isolated runtime..."})
        manager = Manager(args.manager)
        initialization: dict[str, Any] = {}
        if args.config is not None:
            initialization = {
                "config_path": str(args.config.resolve()),
                "sing_box_executable": str(args.sing_box.resolve()),
            }
        manager.call("initialize", initialization)
        runtime_params: dict[str, Any] = {"url": args.url}
        candidates = selected_candidates()
        if candidates is not None:
            runtime_params["candidates"] = candidates
        runtime = manager.call("create_runtime", runtime_params)
        runtime_id, proxy_url, candidates = runtime_fields(runtime)
        total_candidates = len(candidates)
        emit({
            "type": "progress",
            "message": f"Scanning {total_candidates} candidate(s) for GitHub HTTPS reachability...",
            "progress": {
                "https_scanned": 0,
                "https_total": total_candidates,
                "tcp_completed": 0,
                "tcp_total": 0,
                "accepted": 0,
            },
        })
        for ordinal, expected_node in enumerate(candidates, start=1):
            emit({
                "type": "progress",
                "message": f"GitHub HTTPS prefilter checking {ordinal}/{total_candidates}: {expected_node}",
                "node": expected_node,
                "candidate": False,
                "progress": {
                    "https_scanned": ordinal - 1,
                    "https_total": total_candidates,
                    "tcp_completed": assessed,
                    "tcp_total": tcp_total,
                    "accepted": usable,
                },
            })
            candidate = manager.call("next_step", {"runtime_id": runtime_id})
            node = candidate_node(candidate)
            if node is None or node != expected_node:
                raise ProbeInfrastructureError("manager_protocol_error")
            scanned = candidate_scanned(candidate)
            if scanned != ordinal:
                raise ProbeInfrastructureError("manager_protocol_error")
            if not candidate_reachable(candidate):
                emit({
                    "type": "progress",
                    "message": f"GitHub HTTPS prefilter scanned {scanned}/{total_candidates}; {node} did not qualify for TCP 22",
                    "node": node,
                    "candidate": False,
                    "progress": {
                        "https_scanned": scanned,
                        "https_total": total_candidates,
                        "tcp_completed": assessed,
                        "tcp_total": tcp_total,
                        "accepted": usable,
                    },
                })
                continue
            tcp_total += 1
            emit({
                "type": "progress",
                "message": f"GitHub HTTPS prefilter scanned {scanned}/{total_candidates}; checking TCP 22 for {node}",
                "node": node,
                "candidate": True,
                "progress": {
                    "https_scanned": scanned,
                    "https_total": total_candidates,
                    "tcp_completed": assessed,
                    "tcp_total": tcp_total,
                    "accepted": usable,
                },
            })
            is_usable, detail = probe_github_ssh(proxy_url, args.host, args.port, args.timeout)
            assessed += 1
            usable += int(is_usable)
            emit({"type": "node_result", "node": node, "usable": is_usable, "detail": detail})
            emit({
                "type": "progress",
                "message": f"GitHub TCP 22 completed {assessed}/{tcp_total}; accepted {usable}",
                "candidate": False,
                "progress": {
                    "https_scanned": scanned,
                    "https_total": total_candidates,
                    "tcp_completed": assessed,
                    "tcp_total": tcp_total,
                    "accepted": usable,
                },
            })
            print(f"[{assessed}/{total_candidates}] {'INCLUDED' if is_usable else 'EXCLUDED'}", file=sys.stderr, flush=True)
        manager.call("close_runtime", {"runtime_id": runtime_id})
        runtime_id = None
    except ProbeInfrastructureError as error:
        emit_incomplete(error.code)
        return 1
    except KeyboardInterrupt:
        emit_incomplete("interrupted")
        return 130
    finally:
        if runtime_id is not None and manager is not None:
            try:
                manager.call("close_runtime", {"runtime_id": runtime_id})
            except (OSError, RuntimeError, ProbeInfrastructureError):
                pass
        if manager is not None:
            manager.close()

    emit({"type": "summary", "complete": True, "message": f"GitHub SSH available on {usable}/{assessed} assessed node(s)"})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
