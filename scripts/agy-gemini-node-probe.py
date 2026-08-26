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


class Manager:
    def __init__(self, executable: Path) -> None:
        self.process = subprocess.Popen(
            [str(executable), "node-runtime-manager", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        if self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("failed to open node runtime manager pipes")
        self._request_id = 0

    def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._request_id += 1
        request = {"id": self._request_id, "method": method, "params": params}
        self.process.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        if not line:
            raise RuntimeError(
                f"node runtime manager exited unexpectedly ({self.process.poll()})"
            )
        response = json.loads(line)
        if response.get("id") != self._request_id:
            raise RuntimeError("node runtime manager returned an unexpected response id")
        if "error" in response:
            error = response["error"]
            raise RuntimeError(f"{error['code']}: {error['message']}")
        return response["result"]

    def close(self) -> None:
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
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
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        environment[name] = proxy_url

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
        if "Authentication required" in diagnostic_text:
            return {"usable": False, "error": "agy_authentication_required"}
        return {
            "usable": completed.returncode == 0,
            "agy_exit_code": completed.returncode,
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }
    except subprocess.TimeoutExpired as error:
        diagnostic_text = ((error.stdout or b"") + (error.stderr or b"")).decode(
            "utf-8", errors="replace"
        )
        if "Authentication required" in diagnostic_text:
            return {"usable": False, "error": "agy_authentication_required"}
        return {
            "usable": False,
            "error": "agy_timeout",
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }
    except OSError as error:
        return {
            "usable": False,
            "error": f"agy_start_failed: {error}",
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }


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
        "--output", type=Path, default=Path("agy-gemini-node-probe-results.json")
    )
    args = parser.parse_args()
    if (args.config is None) != (args.sing_box is None):
        parser.error("--config and --sing-box must be supplied together")
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    if args.limit is not None and args.limit < 1:
        parser.error("--limit must be at least one")

    manager = Manager(args.manager)
    runtime_id: str | None = None
    results: list[dict[str, Any]] = []
    try:
        initialization: dict[str, Any] = {}
        if args.config is not None:
            initialization = {
                "config_path": str(args.config.resolve()),
                "sing_box_executable": str(args.sing_box.resolve()),
            }
        manager.call("initialize", initialization)
        runtime = manager.call("create_runtime", {"url": args.url})
        runtime_id = runtime["runtime_id"]
        proxy_url = runtime["proxy"]["http"]
        total_candidates = runtime["total_candidates"]

        while args.limit is None or len(results) < args.limit:
            candidate = manager.call("next", {"runtime_id": runtime_id})
            if candidate["end"]:
                runtime_id = None
                break
            node = candidate["node"]
            outcome = run_agy(args.agy, proxy_url, args.prompt, args.timeout, args.model)
            if outcome.get("error") == "agy_authentication_required":
                raise RuntimeError(
                    "Agy is not authenticated; run one interactive `agy --agent gemini` "
                    "request first, then rerun this probe"
                )
            result = {**node, **outcome}
            results.append(result)
            write_results(
                args.output,
                {"probe": "agy_gemini", "prefilter_url": args.url, "results": results},
            )
            state = "USABLE" if result["usable"] else "FAILED"
            print(f"[{len(results)}/{total_candidates}] {state}  {node['tag']}")
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
    finally:
        if runtime_id is not None:
            try:
                manager.call("close_runtime", {"runtime_id": runtime_id})
            except (OSError, RuntimeError):
                pass
        manager.close()

    usable = sum(1 for result in results if result["usable"])
    print(f"usable: {usable}/{len(results)}; results: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
