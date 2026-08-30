#!/usr/bin/env python3
"""Probe every reachable sing-box node with a real Agy Gemini request."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


DEFAULT_PREFILTER_URL = "https://gemini.google.com/"
DEFAULT_PROMPT = "Reply with exactly: OK"


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
                # Manager diagnostics may contain source configuration paths or runtime details.
                # Its structured RPC code is the only diagnostic safe to expose through the TUI.
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
            # Manager messages may mention configuration paths. Preserve only the bounded RPC
            # code so neither TUI JSONL nor its captured stderr can disclose source config data.
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


def run_agy(
    agy: Path,
    proxy_url: str,
    prompt: str,
    timeout: float,
    model: str | None,
) -> dict[str, Any]:
    command = [
        str(agy),
        "--agent",
        "gemini",
        "--print",
        prompt,
        "--output-format",
        "json",
        "--print-timeout",
        f"{max(1, int(timeout))}s",
        "--disable-slash-commands",
    ]
    if model:
        command.extend(["--model", model])
    environment = os.environ.copy()
    for name in (
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ):
        environment[name] = proxy_url
    # A caller's bypass list would silently route Gemini outside the candidate-bound runtime and
    # make a direct-host success look like evidence for the node currently under assessment.
    environment["NO_PROXY"] = ""
    environment["no_proxy"] = ""

    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=timeout + 5,
            env=environment,
        )
        diagnostic_text = (completed.stdout + completed.stderr).decode(
            "utf-8", errors="replace"
        )
        if authentication_required(diagnostic_text):
            raise ProbeInfrastructureError("agy_authentication_required")
        if completed.returncode != 0:
            raise ProbeInfrastructureError("agy_process_failed")
        return {
            "usable": True,
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }
    except subprocess.TimeoutExpired as error:
        diagnostic_text = ((error.stdout or b"") + (error.stderr or b"")).decode(
            "utf-8", errors="replace"
        )
        if authentication_required(diagnostic_text):
            raise ProbeInfrastructureError("agy_authentication_required") from error
        raise ProbeInfrastructureError("agy_timeout") from error
    except OSError as error:
        raise ProbeInfrastructureError("agy_start_failed") from error


def authentication_required(text: str) -> bool:
    normalized = text.casefold()
    return any(
        marker in normalized
        for marker in (
            "authentication required",
            "not authenticated",
            "unauthenticated",
            "login required",
        )
    )


def safe_error_code(value: Any) -> str:
    if not isinstance(value, str) or not (1 <= len(value) <= 64):
        return "unknown"
    if not all(
        character.isascii() and (character.isalnum() or character == "_")
        for character in value
    ):
        return "unknown"
    return value


def emit_tui_record(record: dict[str, Any]) -> None:
    print(json.dumps(record, ensure_ascii=False), flush=True)


def emit_incomplete(code: str) -> None:
    # Keep stdout protocol-safe and stderr non-sensitive. The TUI deliberately preserves the
    # previous complete run when it receives this terminal record.
    emit_tui_record(
        {
            "type": "summary",
            "complete": False,
            "message": f"Agy Gemini probe incomplete ({code})",
        }
    )
    print(f"Agy Gemini probe incomplete: {code}", file=sys.stderr, flush=True)


def runtime_fields(runtime: dict[str, Any]) -> tuple[str, str, int]:
    runtime_id = runtime.get("runtime_id")
    proxy = runtime.get("proxy")
    proxy_url = proxy.get("http") if isinstance(proxy, dict) else None
    total_candidates = runtime.get("total_candidates")
    if (
        not isinstance(runtime_id, str)
        or not isinstance(proxy_url, str)
        or not isinstance(total_candidates, int)
    ):
        raise ProbeInfrastructureError("manager_protocol_error")
    return runtime_id, proxy_url, total_candidates


def candidate_node(candidate: dict[str, Any]) -> str | None:
    end = candidate.get("end")
    if end is True:
        return None
    node = candidate.get("node")
    tag = node.get("tag") if isinstance(node, dict) else None
    if end is not False or not isinstance(tag, str):
        raise ProbeInfrastructureError("manager_protocol_error")
    return tag


def write_results(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Test every reachable configured node with Agy's Gemini agent."
    )
    parser.add_argument("--manager", type=Path, default=Path("sing-box-tui"))
    parser.add_argument("--agy", type=Path, default=Path("agy"))
    parser.add_argument("--config", type=Path)
    parser.add_argument("--sing-box", type=Path)
    parser.add_argument("--url", default=DEFAULT_PREFILTER_URL)
    parser.add_argument("--prompt", default=DEFAULT_PROMPT)
    parser.add_argument("--model")
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--limit", type=int)
    parser.add_argument(
        "--tui-jsonl",
        action="store_true",
        help="publish safe progressive usability records for a TUI executable manifest",
    )
    parser.add_argument(
        "--output", type=Path, default=Path("agy-gemini-node-probe-results.json")
    )
    args = parser.parse_args()
    if (args.config is None) != (args.sing_box is None):
        parser.error("--config and --sing-box must be supplied together")
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    if args.limit is not None and args.limit < 1:
        parser.error("--limit must be at least one")

    if args.tui_jsonl and args.limit is not None:
        emit_incomplete("partial_runs_are_not_supported")
        return 2

    manager: Manager | None = None
    runtime_id: str | None = None
    results: list[dict[str, Any]] = []
    try:
        manager = Manager(args.manager)
        initialization: dict[str, Any] = {}
        if args.config is not None:
            initialization = {
                "config_path": str(args.config.resolve()),
                "sing_box_executable": str(args.sing_box.resolve()),
            }
        manager.call("initialize", initialization)
        runtime = manager.call("create_runtime", {"url": args.url})
        runtime_id, proxy_url, total_candidates = runtime_fields(runtime)

        while args.limit is None or len(results) < args.limit:
            candidate = manager.call("next", {"runtime_id": runtime_id})
            node_tag = candidate_node(candidate)
            if node_tag is None:
                runtime_id = None
                break
            outcome = run_agy(args.agy, proxy_url, args.prompt, args.timeout, args.model)
            result = {"tag": node_tag, **outcome}
            results.append(result)
            if args.tui_jsonl:
                emit_tui_record(
                    {
                        "type": "node_result",
                        "node": node_tag,
                        "usable": True,
                        "detail": f"Agy Gemini command succeeded in {outcome['elapsed_ms']}ms",
                    }
                )
                print(
                    f"[{len(results)}/{total_candidates}] INCLUDED",
                    file=sys.stderr,
                    flush=True,
                )
            else:
                write_results(
                    args.output,
                    {"probe": "agy_gemini", "prefilter_url": args.url, "results": results},
                )
                print(f"[{len(results)}/{total_candidates}] USABLE  {node_tag}")
    except ProbeInfrastructureError as error:
        if args.tui_jsonl:
            emit_incomplete(error.code)
        else:
            print(f"probe incomplete: {error.code}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        if args.tui_jsonl:
            emit_incomplete("interrupted")
        else:
            print("interrupted", file=sys.stderr)
        return 130
    finally:
        if runtime_id is not None and manager is not None:
            try:
                manager.call("close_runtime", {"runtime_id": runtime_id})
            except (OSError, RuntimeError):
                pass
        if manager is not None:
            manager.close()

    usable = sum(1 for result in results if result["usable"])
    if args.tui_jsonl:
        emit_tui_record(
            {
                "type": "summary",
                "complete": True,
                "message": f"Agy Gemini command succeeded on {usable} node(s)",
            }
        )
    else:
        print(f"usable: {usable}/{len(results)}; results: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
