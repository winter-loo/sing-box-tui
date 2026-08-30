#!/usr/bin/env python3
"""Offline contract tests for the Agy Gemini TUI adapter."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
ADAPTER = REPOSITORY / "scripts" / "agy-gemini-node-probe.py"
UNIX_MANIFEST = (
    REPOSITORY / "examples" / "usability-probes" / "unix" / "agy-gemini.json"
)
WINDOWS_MANIFEST = (
    REPOSITORY / "examples" / "usability-probes" / "windows" / "agy-gemini.json"
)


class AgyGeminiProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="agy-probe-test-")
        directory = Path(self.temporary.name)
        self.calls = directory / "agy-calls.jsonl"
        self.manager = self._write_executable(
            directory / "manager-fixture.py",
            r'''
import json
import os
import sys

cursor = 0
print("secret manager config: /private/source.json", file=sys.stderr, flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if method == "initialize":
        result = {"total_candidates": 2}
    elif method == "create_runtime":
        result = {
            "runtime_id": "fixture-runtime",
            "proxy": {"http": "http://127.0.0.1:9"},
            "total_candidates": 2,
        }
    elif method == "next" and os.environ.get("MANAGER_FIXTURE_MODE") == "runtime-error":
        print(json.dumps({
            "id": request["id"],
            "error": {
                "code": "runtime_failed",
                "message": "secret-config=/private/source.json",
            },
        }), flush=True)
        continue
    elif method == "next":
        nodes = ["node-alpha", "node-beta"]
        if cursor == len(nodes):
            result = {"end": True}
        else:
            result = {"end": False, "node": {"tag": nodes[cursor]}}
            cursor += 1
    elif method == "close_runtime":
        result = {"closed": True}
    else:
        raise AssertionError(method)
    print(json.dumps({"id": request["id"], "result": result}), flush=True)
''',
        )
        self.agy = self._write_executable(
            directory / "agy-fixture.py",
            r'''
import json
import os
import sys
import time

with open(os.environ["AGY_FIXTURE_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps({
        "argv": sys.argv[1:],
        "NO_PROXY": os.environ.get("NO_PROXY"),
        "no_proxy": os.environ.get("no_proxy"),
    }) + "\n")
mode = os.environ.get("AGY_FIXTURE_MODE", "success")
if mode == "auth":
    print("Authentication required for secret-account@example.test", file=sys.stderr)
    raise SystemExit(7)
if mode == "process-error":
    print("private project secret-project", file=sys.stderr)
    raise SystemExit(9)
if mode == "timeout":
    time.sleep(10)
print(json.dumps({"response": "OK", "private": "secret-response"}))
''',
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def _write_executable(self, path: Path, body: str) -> Path:
        path.write_text(
            f"#!{sys.executable}\n" + textwrap.dedent(body).lstrip(), encoding="utf-8"
        )
        path.chmod(0o700)
        return path

    def _run(
        self, *, agy_mode: str = "success", manager_mode: str = "success"
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            AGY_FIXTURE_CALLS=str(self.calls),
            AGY_FIXTURE_MODE=agy_mode,
            MANAGER_FIXTURE_MODE=manager_mode,
            # The adapter and both fixtures must stay useful on an offline CI runner. Any
            # accidental external request is sent to an unbound loopback port and fails closed.
            HTTP_PROXY="http://127.0.0.1:9",
            HTTPS_PROXY="http://127.0.0.1:9",
            ALL_PROXY="http://127.0.0.1:9",
            NO_PROXY="gemini.google.com",
            no_proxy="*",
        )
        return subprocess.run(
            [
                sys.executable,
                str(ADAPTER),
                "--tui-jsonl",
                "--manager",
                str(self.manager),
                "--agy",
                str(self.agy),
                "--url",
                "https://probe.invalid/",
                "--timeout",
                "1",
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=10,
            env=environment,
            check=False,
        )

    def test_fixture_success_streams_only_real_command_successes(self) -> None:
        completed = self._run()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        records = [json.loads(line) for line in completed.stdout.splitlines()]
        self.assertEqual(
            [(record["node"], record["usable"]) for record in records[:-1]],
            [("node-alpha", True), ("node-beta", True)],
        )
        self.assertEqual(records[-1]["type"], "summary")
        self.assertTrue(records[-1]["complete"])
        self.assertNotIn("secret-response", completed.stdout + completed.stderr)
        self.assertNotIn("secret manager config", completed.stdout + completed.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual(len(calls), 2)
        for call in calls:
            self.assertIn("--agent", call["argv"])
            self.assertIn("gemini", call["argv"])
            self.assertEqual(call["NO_PROXY"], "")
            self.assertEqual(call["no_proxy"], "")

    def test_authentication_and_process_failures_are_incomplete_not_unusable(self) -> None:
        for mode in ("auth", "process-error"):
            with self.subTest(mode=mode):
                completed = self._run(agy_mode=mode)
                records = [json.loads(line) for line in completed.stdout.splitlines()]
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(records[-1]["type"], "summary")
                self.assertFalse(records[-1]["complete"])
                self.assertFalse(any(record.get("usable") is False for record in records))
                combined = completed.stdout + completed.stderr
                self.assertNotIn("secret-account", combined)
                self.assertNotIn("secret-project", combined)

    def test_runtime_failure_is_incomplete_and_hides_manager_diagnostic(self) -> None:
        completed = self._run(manager_mode="runtime-error")
        records = [json.loads(line) for line in completed.stdout.splitlines()]
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(records, [
            {
                "type": "summary",
                "complete": False,
                "message": "Agy Gemini probe incomplete (manager_runtime_failed)",
            }
        ])
        self.assertNotIn("secret-config", completed.stdout + completed.stderr)
        self.assertFalse(self.calls.exists(), "runtime failure must not invoke Agy")

    def test_timeout_is_incomplete_not_unusable(self) -> None:
        completed = self._run(agy_mode="timeout")
        records = [json.loads(line) for line in completed.stdout.splitlines()]
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(records[-1]["type"], "summary")
        self.assertFalse(records[-1]["complete"])
        self.assertIn("agy_timeout", records[-1]["message"])
        self.assertFalse(any(record.get("usable") is False for record in records))

    def test_platform_manifests_are_manual_balanced_and_shell_free(self) -> None:
        unix = json.loads(UNIX_MANIFEST.read_text(encoding="utf-8"))
        windows = json.loads(WINDOWS_MANIFEST.read_text(encoding="utf-8"))
        for manifest in (unix, windows):
            self.assertEqual(manifest["id"], "agy-gemini")
            self.assertEqual(manifest["ranking"], "balanced")
            self.assertNotIn("background", manifest)
            self.assertEqual(manifest["args"][-1], "--tui-jsonl")
            self.assertFalse(any("profile" in argument for argument in manifest["args"]))
        self.assertTrue(unix["executable"].endswith("agy-gemini-node-probe.py"))
        self.assertTrue(windows["executable"].lower().endswith("python.exe"))
        self.assertTrue(windows["args"][0].endswith("agy-gemini-node-probe.py"))


if __name__ == "__main__":
    unittest.main()
