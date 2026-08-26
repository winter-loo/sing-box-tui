"""Minimal client for `sing-box-tui node-runtime-manager --stdio`.

This demonstrates isolated node iteration. Application-specific usability
decisions and result aggregation deliberately remain in the caller.
"""

from __future__ import annotations

import argparse
import json
import queue
import subprocess
import threading
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any


class NodeRuntimeManager:
    def __init__(self, command: list[str]) -> None:
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        self._next_id = 1
        self._write_lock = threading.Lock()
        self._pending: dict[int, queue.Queue[dict[str, Any]]] = {}
        self._reader = threading.Thread(target=self._read_responses, daemon=True)
        self._reader.start()

    def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        with self._write_lock:
            request_id = self._next_id
            self._next_id += 1
            response_queue: queue.Queue[dict[str, Any]] = queue.Queue(maxsize=1)
            self._pending[request_id] = response_queue
            request = {"id": request_id, "method": method, "params": params}
            self.process.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
            self.process.stdin.flush()
        response = response_queue.get()
        if "error" in response:
            error = response["error"]
            raise RuntimeError(f"{error['code']}: {error['message']}")
        return response["result"]

    def close(self) -> None:
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        self.process.wait(timeout=10)

    def _read_responses(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            response = json.loads(line)
            request_id = response.get("id")
            response_queue = self._pending.pop(request_id, None)
            if response_queue is not None:
                response_queue.put(response)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manager", default="sing-box-tui")
    parser.add_argument("--config", type=Path)
    parser.add_argument("--sing-box", type=Path)
    parser.add_argument("urls", nargs="+", help="One different prefilter URL per runtime")
    args = parser.parse_args()
    if (args.config is None) != (args.sing_box is None):
        parser.error("--config and --sing-box must be supplied together")

    manager = NodeRuntimeManager([args.manager, "node-runtime-manager", "--stdio"])
    initialize: dict[str, Any] = {}
    if args.config is not None:
        initialize.update(
            config_path=str(args.config.resolve()),
            sing_box_executable=str(args.sing_box.resolve()),
        )
    manager.call("initialize", initialize)
    runtimes = [manager.call("create_runtime", {"url": url}) for url in args.urls]

    def fetch_one(runtime: dict[str, Any]) -> dict[str, Any]:
        return manager.call("next", {"runtime_id": runtime["runtime_id"]})

    try:
        with ThreadPoolExecutor(max_workers=len(runtimes)) as executor:
            for result in executor.map(fetch_one, runtimes):
                print(json.dumps(result, ensure_ascii=False))
    finally:
        for runtime in runtimes:
            manager.call("close_runtime", {"runtime_id": runtime["runtime_id"]})
        manager.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
