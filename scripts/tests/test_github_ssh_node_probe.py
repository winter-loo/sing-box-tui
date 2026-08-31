#!/usr/bin/env python3
"""Offline contract tests for the GitHub SSH usability probe."""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
ADAPTER = REPOSITORY / "scripts" / "github-ssh-node-probe.py"
UNIX_MANIFEST = REPOSITORY / "examples" / "usability-probes" / "unix" / "github-ssh.json"
WINDOWS_MANIFEST = REPOSITORY / "examples" / "usability-probes" / "windows" / "github-ssh.json"


class ConnectProxyFixture:
    def __init__(self, outcomes: list[str]) -> None:
        self.outcomes = outcomes
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.port = self.listener.getsockname()[1]
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self) -> None:
        for outcome in self.outcomes:
            connection, _ = self.listener.accept()
            with connection:
                request = b""
                while b"\r\n\r\n" not in request:
                    request += connection.recv(4096)
                if outcome == "ssh":
                    connection.sendall(b"HTTP/1.1 200 Connection established\r\n\r\nSSH-2.0-fixture\r\n")
                elif outcome == "reject":
                    connection.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                else:
                    connection.sendall(b"HTTP/1.1 200 Connection established\r\n\r\nnot ssh\r\n")
        self.listener.close()


class GithubSshProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="github-ssh-probe-")
        self.directory = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def _write_manager(self, port: int, mode: str = "success") -> Path:
        script = self.directory / "manager.py"
        body = f'''
import json
import sys
nodes = ["美国HY1轻量", "node-rejected", "node-non-ssh"]
cursor = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if method == "initialize":
        result = {{}}
    elif method == "create_runtime":
        nodes = request["params"].get("candidates", nodes)
        result = {{"runtime_id": "runtime", "proxy": {{"http": "http://127.0.0.1:{port}"}}, "total_candidates": 3, "candidates": nodes}}
    elif method == "next_step" and {mode!r} == "error":
        print(json.dumps({{"id": request["id"], "error": {{"code": "runtime_failed", "message": "secret path"}}}}), flush=True)
        continue
    elif method == "next_step":
        if cursor == len(nodes):
            result = {{"end": True}}
        else:
            result = {{"end": False, "node": {{"tag": nodes[cursor]}}, "scanned": cursor + 1, "reachable": nodes[cursor] != "node-rejected"}}
            cursor += 1
    elif method == "close_runtime":
        result = {{"closed": True}}
    print(json.dumps({{"id": request["id"], "result": result}}), flush=True)
'''
        script.write_text(f"#!{sys.executable}\n" + textwrap.dedent(body).lstrip(), encoding="utf-8")
        script.chmod(0o700)
        if os.name != "nt":
            return script
        wrapper = self.directory / "manager.cmd"
        wrapper.write_text(f'@"{sys.executable}" "{script}" %*\n', encoding="utf-8")
        return wrapper

    def test_reports_banner_success_and_node_attributable_failures(self) -> None:
        proxy = ConnectProxyFixture(["ssh", "reject", "non-ssh"])
        environment = os.environ.copy()
        environment.update(
            HTTP_PROXY="http://127.0.0.1:9",
            HTTPS_PROXY="http://127.0.0.1:9",
            ALL_PROXY="http://127.0.0.1:9",
            NO_PROXY="",
        )
        completed = subprocess.run(
            [sys.executable, str(ADAPTER), "--manager", str(self._write_manager(proxy.port)), "--timeout", "1"],
            capture_output=True, text=True, encoding="utf-8", errors="strict",
            timeout=10, check=False, env=environment,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        records = [json.loads(line) for line in completed.stdout.splitlines()]
        progress = [record["message"] for record in records if record["type"] == "progress"]
        self.assertEqual(progress[0], "Starting GitHub SSH isolated runtime...")
        self.assertIn("Scanning 3 candidate(s)", progress[1])
        https_progress = [
            record for record in records
            if record["type"] == "progress"
            and record.get("candidate") is False
            and "prefilter checking" in record["message"]
        ]
        self.assertEqual(
            [(record["node"], record["message"]) for record in https_progress],
            [
                ("美国HY1轻量", "GitHub HTTPS prefilter checking 1/3: 美国HY1轻量"),
                ("node-rejected", "GitHub HTTPS prefilter checking 2/3: node-rejected"),
                ("node-non-ssh", "GitHub HTTPS prefilter checking 3/3: node-non-ssh"),
            ],
        )
        node_progress = [
            record for record in records
            if record["type"] == "progress" and record.get("candidate") is True
        ]
        self.assertEqual(
            [record["node"] for record in node_progress],
            ["美国HY1轻量", "node-non-ssh"],
        )
        self.assertEqual(
            node_progress[0]["progress"],
            {
                "https_scanned": 1,
                "https_total": 3,
                "tcp_completed": 0,
                "tcp_total": 1,
                "accepted": 0,
            },
        )
        completed_progress = [
            record["progress"] for record in records
            if record["type"] == "progress" and "TCP 22 completed" in record["message"]
        ]
        self.assertEqual(completed_progress[-1], {
            "https_scanned": 3,
            "https_total": 3,
            "tcp_completed": 2,
            "tcp_total": 2,
            "accepted": 1,
        })
        node_results = [record for record in records if record["type"] == "node_result"]
        self.assertEqual([(r["node"], r["usable"]) for r in node_results], [
            ("美国HY1轻量", True), ("node-non-ssh", False)
        ])
        self.assertIn("SSH-2.0", node_results[0]["detail"])
        self.assertTrue(records[-1]["complete"])
        self.assertEqual(records[-1]["message"], "GitHub SSH available on 1/2 assessed node(s)")

    def test_manager_failure_is_incomplete_without_false_node_results(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(ADAPTER), "--manager", str(self._write_manager(9, "error")), "--timeout", "1"],
            capture_output=True, text=True, encoding="utf-8", errors="strict",
            timeout=10, check=False,
        )
        records = [json.loads(line) for line in completed.stdout.splitlines()]
        self.assertNotEqual(completed.returncode, 0)
        summaries = [record for record in records if record["type"] == "summary"]
        self.assertEqual(summaries, [{"type": "summary", "complete": False, "message": "GitHub SSH probe incomplete (manager_runtime_failed)"}])

    def test_tui_candidate_scope_drives_all_progress_and_results(self) -> None:
        proxy = ConnectProxyFixture(["non-ssh"])
        environment = os.environ.copy()
        expected_nodes = ["node-rejected", "node-non-ssh"]
        environment["SING_BOX_TUI_USABILITY_CANDIDATES"] = json.dumps(expected_nodes)
        completed = subprocess.run(
            [sys.executable, str(ADAPTER), "--manager", str(self._write_manager(proxy.port)), "--timeout", "1"],
            capture_output=True, text=True, encoding="utf-8", errors="strict",
            timeout=10, check=False, env=environment,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        records = [json.loads(line) for line in completed.stdout.splitlines()]
        metrics = [record["progress"] for record in records if "progress" in record]
        self.assertTrue(metrics)
        self.assertTrue(all(metric["https_total"] == 2 for metric in metrics))
        results = [record for record in records if record["type"] == "node_result"]
        self.assertEqual([record["node"] for record in results], ["node-non-ssh"])
        self.assertNotIn("secret path", completed.stdout + completed.stderr)

    def test_manifests_are_manual_low_latency_and_shell_free(self) -> None:
        unix = json.loads(UNIX_MANIFEST.read_text(encoding="utf-8"))
        windows = json.loads(WINDOWS_MANIFEST.read_text(encoding="utf-8"))
        for manifest in (unix, windows):
            self.assertEqual(manifest["id"], "github-ssh")
            self.assertEqual(manifest["ranking"], "low-latency")
            self.assertNotIn("background", manifest)
            self.assertIsInstance(manifest["args"], list)
        self.assertTrue(unix["executable"].endswith("github-ssh-node-probe.py"))
        self.assertTrue(windows["executable"].lower().endswith("python.exe"))


if __name__ == "__main__":
    unittest.main()
