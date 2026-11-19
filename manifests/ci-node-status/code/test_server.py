"""Unit tests for ci-node-status (manifests/ci-node-status/code/server.py)."""

from __future__ import annotations

import json
import socket
import sys
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import server


def make_ars(name: str, min_runners: int | None = 0) -> dict[str, Any]:
    spec: dict[str, Any] = {"runnerScaleSetName": name}
    if min_runners is not None:
        spec["minRunners"] = min_runners
    return {
        "metadata": {
            "name": name,
            "labels": {"actions.github.com/scale-set-name": name},
        },
        "spec": spec,
    }


def make_er(
    name: str,
    scale_set: str,
    *,
    phase: str = "Pending",
    job_id: str | None = None,
    job_name: str | None = None,
    repository: str | None = None,
    workflow_run_id: int | str | None = None,
    org: str = "Bafff",
    repo_label: str = "example-repo",
    deletion_timestamp: str | None = None,
) -> dict[str, Any]:
    meta: dict[str, Any] = {
        "name": name,
        "labels": {
            "actions.github.com/scale-set-name": scale_set,
            "actions.github.com/organization": org,
            "actions.github.com/repository": repo_label,
        },
    }
    if deletion_timestamp is not None:
        meta["deletionTimestamp"] = deletion_timestamp

    status: dict[str, Any] = {}
    if phase:
        status["phase"] = phase
    if job_id is not None:
        status["jobId"] = job_id
    if job_name is not None:
        status["jobDisplayName"] = job_name
    if repository is not None:
        status["jobRepositoryName"] = repository
    if workflow_run_id is not None:
        status["workflowRunId"] = workflow_run_id

    return {"metadata": meta, "status": status}


class LightPoolMatcherTests(unittest.TestCase):
    def test_defaults_match_light_and_keep_heavy(self) -> None:
        matcher = server.LightPoolMatcher()
        for light in (
            "leiloes-5950x-route",
            "idealista-5950x-route",
            "cars-spy-5950x-route",
            "idealista-5950x-light",
            "idealista-5950x-deploy",
            "ponte-5950x-deploy",
            "sonarqube-5950x",
            "sonarqube-gate",
        ):
            self.assertTrue(matcher.is_light(light), f"expected light: {light}")

        for heavy in (
            "idealista-5950x",
            "idealista-5950x-build",
            "leiloes-ci",
            "cars-spy-5950x",
            "ponte-5950x",
            "pt309-5950x",
            "5950x-k8s",
            "",
        ):
            self.assertFalse(matcher.is_light(heavy), f"expected heavy: {heavy}")

    def test_env_custom_patterns_and_overrides(self) -> None:
        matcher = server.LightPoolMatcher.from_env(
            {"LIGHT_SCALE_SETS": "5950x-k8s, canary-* , "}
        )
        self.assertTrue(matcher.is_light("5950x-k8s"))
        self.assertTrue(matcher.is_light("canary-test"))
        self.assertTrue(matcher.is_light("leiloes-5950x-route"))
        self.assertFalse(matcher.is_light("leiloes-ci"))

        strict = server.LightPoolMatcher.from_env(
            {
                "LIGHT_SCALE_SETS": "only-this",
                "LIGHT_SCALE_SET_SUFFIXES": "",
                "LIGHT_SCALE_SET_PREFIXES": "",
            }
        )
        self.assertTrue(strict.is_light("only-this"))
        self.assertFalse(strict.is_light("leiloes-5950x-route"))
        self.assertFalse(strict.is_light("sonarqube-5950x"))


class ProcReadersTests(unittest.TestCase):
    def test_read_load1_and_mem_available(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            loadavg = Path(tmp) / "loadavg"
            meminfo = Path(tmp) / "meminfo"
            loadavg.write_text("1.42 0.98 0.50 2/412 9876\n", encoding="utf-8")
            meminfo.write_text(
                "MemTotal:       52428800 kB\n"
                "MemFree:        10485760 kB\n"
                "MemAvailable:   41943040 kB\n",
                encoding="utf-8",
            )
            self.assertAlmostEqual(server.read_load1(loadavg), 1.42)
            self.assertEqual(server.read_mem_available_mib(meminfo), 40960)

    def test_meminfo_fallback_and_missing_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            meminfo = Path(tmp) / "meminfo"
            meminfo.write_text("MemFree:        2048000 kB\n", encoding="utf-8")
            self.assertEqual(server.read_mem_available_mib(meminfo), 2000)

            missing = Path(tmp) / "missing"
            self.assertIsNone(server.read_load1(missing))
            self.assertIsNone(server.read_mem_available_mib(missing))


class EvaluateStatusTests(unittest.TestCase):
    def setUp(self) -> None:
        self.matcher = server.LightPoolMatcher()
        self.min_runners = server.extract_min_runners(
            [
                make_ars("idealista-5950x", 0),
                make_ars("idealista-5950x-build", 1),
                make_ars("idealista-5950x-route", 1),
                make_ars("idealista-5950x-light", 0),
                make_ars("idealista-5950x-deploy", 0),
                make_ars("leiloes-ci", 0),
                make_ars("leiloes-5950x-route", 1),
                make_ars("ponte-5950x", 1),
                make_ars("cars-spy-5950x", None),
            ]
        )

    def test_idle_warm_runners_are_not_busy(self) -> None:
        ers = [
            make_er("ponte-5950x-runner-1", "ponte-5950x", phase="Pending"),
            make_er("idealista-5950x-build-runner-1", "idealista-5950x-build", phase="Pending"),
            make_er("leiloes-5950x-route-runner-1", "leiloes-5950x-route", phase="Running", job_id="j-route"),
            make_er("idealista-5950x-light-runner-1", "idealista-5950x-light", phase="Running", job_id="j-light"),
            make_er("idealista-5950x-deploy-runner-1", "idealista-5950x-deploy", phase="Running", job_id="j-deploy"),
            make_er("sonarqube-scan-runner-1", "sonarqube-scan", phase="Running", job_id="j-sonar"),
        ]
        result = server.evaluate_status(
            node="k3d-5950x-ci-0",
            ephemeral_runners=ers,
            min_runners_by_set=self.min_runners,
            matcher=self.matcher,
            load1=0.25,
            mem_available_mib=45000,
        )
        self.assertEqual(
            result,
            {
                "node": "k3d-5950x-ci-0",
                "busy": False,
                "busy_count": 0,
                "busy_by_set": {},
                "heavy_jobs": [],
                "load1": 0.25,
                "mem_available_mib": 45000,
            },
        )

    def test_warm_heavy_runner_active_when_running_or_job_id_set(self) -> None:
        ers = [
            make_er("ponte-5950x-runner-idle", "ponte-5950x", phase="Pending"),
            make_er(
                "ponte-5950x-runner-busy",
                "ponte-5950x",
                phase="Running",
                job_id="uuid-ponte",
                job_name="test",
                repository="Bafff/voice-call-translator-pt",
                workflow_run_id=1001,
            ),
            make_er(
                "idealista-5950x-build-runner-busy",
                "idealista-5950x-build",
                phase="Pending",
                job_id="uuid-build",
                job_name="build",
                repository="Bafff/idealista-tracker",
                workflow_run_id=1002,
            ),
        ]
        result = server.evaluate_status(
            node="k3d-5950x-ci-0",
            ephemeral_runners=ers,
            min_runners_by_set=self.min_runners,
            matcher=self.matcher,
        )
        self.assertTrue(result["busy"])
        self.assertEqual(result["busy_count"], 2)
        self.assertEqual(
            result["busy_by_set"],
            {"idealista-5950x-build": 1, "ponte-5950x": 1},
        )
        self.assertEqual([j["name"] for j in result["heavy_jobs"]], [
            "idealista-5950x-build-runner-busy",
            "ponte-5950x-runner-busy",
        ])

    def test_min_runners_zero_pending_is_busy_until_terminal_or_terminating(self) -> None:
        ers = [
            make_er(
                "idealista-5950x-runner-booting",
                "idealista-5950x",
                phase="Pending",
                org="Bafff",
                repo_label="idealista-tracker",
            ),
            make_er(
                "cars-spy-5950x-runner-new",
                "cars-spy-5950x",
                phase="",
                org="Bafff",
                repo_label="cars-spy",
            ),
            make_er(
                "leiloes-ci-runner-done",
                "leiloes-ci",
                phase="Succeeded",
                job_id="uuid-done",
                job_name="test",
                workflow_run_id=2001,
            ),
            make_er(
                "leiloes-ci-runner-failed",
                "leiloes-ci",
                phase="Failed",
                job_id="uuid-fail",
                job_name="test",
                workflow_run_id=2002,
            ),
            make_er(
                "leiloes-ci-runner-outdated",
                "leiloes-ci",
                phase="Outdated",
            ),
            make_er(
                "ponte-5950x-runner-outdated",
                "ponte-5950x",
                phase="Outdated",
                job_id="uuid-outdated",
                job_name="test",
                workflow_run_id=2003,
            ),
            make_er(
                "leiloes-ci-runner-cancelled",
                "leiloes-ci",
                phase="Pending",
                deletion_timestamp="2026-11-19T18:00:00Z",
            ),
        ]
        result = server.evaluate_status(
            node="k3d-5950x-ci-0",
            ephemeral_runners=ers,
            min_runners_by_set=self.min_runners,
            matcher=self.matcher,
        )
        self.assertTrue(result["busy"])
        self.assertEqual(result["busy_count"], 2)
        self.assertEqual(
            result["busy_by_set"],
            {"cars-spy-5950x": 1, "idealista-5950x": 1},
        )
        by_name = {j["name"]: j for j in result["heavy_jobs"]}
        self.assertEqual(by_name["idealista-5950x-runner-booting"]["repository"], "Bafff/idealista-tracker")
        self.assertIsNone(by_name["idealista-5950x-runner-booting"]["workflow_run_id"])
        self.assertEqual(by_name["cars-spy-5950x-runner-new"]["repository"], "Bafff/cars-spy")

    def test_exclude_run_and_exclude_job(self) -> None:
        ers = [
            make_er(
                "leiloes-ci-runner-proven",
                "leiloes-ci",
                phase="Running",
                job_id="uuid-1",
                job_name="proven",
                repository="Bafff/leiloes-radar",
                workflow_run_id=555001,
            ),
            make_er(
                "leiloes-ci-runner-self",
                "leiloes-ci",
                phase="Running",
                job_id="uuid-2",
                job_name="test",
                repository="Bafff/leiloes-radar",
                workflow_run_id=555002,
            ),
            make_er(
                "idealista-5950x-runner-other",
                "idealista-5950x",
                phase="Running",
                job_id="uuid-3",
                job_name="pytest",
                repository="Bafff/idealista-tracker",
                workflow_run_id=999001,
            ),
        ]
        result = server.evaluate_status(
            node="k3d-5950x-ci-0",
            ephemeral_runners=ers,
            min_runners_by_set=self.min_runners,
            matcher=self.matcher,
            exclude_runs=frozenset({"555002"}),
            exclude_jobs=frozenset({"proven"}),
        )
        self.assertTrue(result["busy"])
        self.assertEqual(result["busy_count"], 1)
        self.assertEqual(result["heavy_jobs"][0]["name"], "idealista-5950x-runner-other")

        # Excluding all active jobs makes node idle.
        idle = server.evaluate_status(
            node="k3d-5950x-ci-0",
            ephemeral_runners=ers,
            min_runners_by_set=self.min_runners,
            matcher=self.matcher,
            exclude_runs=frozenset({"555002", "999001"}),
            exclude_jobs=frozenset({"proven"}),
        )
        self.assertFalse(idle["busy"])
        self.assertEqual(idle["busy_count"], 0)
        self.assertEqual(idle["busy_by_set"], {})


class FakeK8s:
    def __init__(
        self,
        ars: list[dict[str, Any]],
        ers: list[dict[str, Any]],
        *,
        error: Exception | None = None,
    ) -> None:
        self.namespace = "arc-runners"
        self.ars = ars
        self.ers = ers
        self.error = error
        self.ars_calls = 0
        self.er_calls = 0

    def list_autoscaling_runner_sets(self) -> list[dict[str, Any]]:
        self.ars_calls += 1
        if self.error is not None:
            raise self.error
        return list(self.ars)

    def list_ephemeral_runners(self) -> list[dict[str, Any]]:
        self.er_calls += 1
        if self.error is not None:
            raise self.error
        return list(self.ers)


class StatusServiceCacheTests(unittest.TestCase):
    def test_ars_cache_ttl_and_unknown_set_refresh(self) -> None:
        now = [100.0]
        fake = FakeK8s(
            ars=[make_ars("idealista-5950x", 0)],
            ers=[make_er("idealista-5950x-runner-1", "idealista-5950x", phase="Pending")],
        )
        service = server.StatusService(
            k8s=fake,
            node="k3d-5950x-ci-0",
            matcher=server.LightPoolMatcher(),
            ars_cache_seconds=10.0,
            clock=lambda: now[0],
        )

        first = service.status()
        self.assertTrue(first["busy"])
        self.assertEqual(fake.ars_calls, 1)
        self.assertEqual(fake.er_calls, 1)

        # Within TTL, ARS is cached while ER is still fetched fresh.
        now[0] = 105.0
        service.status()
        self.assertEqual(fake.ars_calls, 1)
        self.assertEqual(fake.er_calls, 2)

        # New heavy scale set appears in ER -> triggers immediate ARS refresh.
        fake.ars.append(make_ars("ponte-5950x", 1))
        fake.ers = [make_er("ponte-5950x-runner-1", "ponte-5950x", phase="Pending")]
        now[0] = 106.0
        third = service.status()
        self.assertEqual(fake.ars_calls, 2)
        self.assertFalse(third["busy"])

        # After TTL expires, ARS refreshes again.
        now[0] = 120.0
        service.status()
        self.assertEqual(fake.ars_calls, 3)


class HttpServerTests(unittest.TestCase):
    def _start_server(
        self,
        fake: FakeK8s,
        *,
        max_concurrency: int = server.DEFAULT_MAX_CONCURRENCY,
    ) -> tuple[server.ReusableThreadingHTTPServer, str]:
        tmp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(tmp_dir.cleanup)
        loadavg = Path(tmp_dir.name) / "loadavg"
        meminfo = Path(tmp_dir.name) / "meminfo"
        loadavg.write_text("0.75 0.50 0.25 1/100 123\n", encoding="utf-8")
        meminfo.write_text("MemAvailable:   32768000 kB\n", encoding="utf-8")

        service = server.StatusService(
            k8s=fake,
            node="k3d-5950x-ci-0",
            matcher=server.LightPoolMatcher(),
            loadavg_path=loadavg,
            meminfo_path=meminfo,
        )
        httpd = server.ReusableThreadingHTTPServer(
            ("127.0.0.1", 0),
            server.make_handler(service),
            max_concurrency=max_concurrency,
        )
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        port = httpd.server_address[1]
        return httpd, f"http://127.0.0.1:{port}"

    def _get(self, url: str) -> tuple[int, dict[str, Any]]:
        try:
            with urllib.request.urlopen(url, timeout=3.0) as resp:
                return resp.status, json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            with exc:
                return exc.code, json.loads(exc.read().decode("utf-8"))

    def test_endpoints_and_query_filters(self) -> None:
        fake = FakeK8s(
            ars=[make_ars("idealista-5950x", 0), make_ars("leiloes-ci", 0)],
            ers=[
                make_er(
                    "idealista-5950x-runner-a",
                    "idealista-5950x",
                    phase="Running",
                    job_id="id-1",
                    job_name="pytest",
                    repository="Bafff/idealista-tracker",
                    workflow_run_id=44590432742,
                ),
                make_er(
                    "leiloes-ci-runner-b",
                    "leiloes-ci",
                    phase="Running",
                    job_id="id-2",
                    job_name="proven",
                    repository="Bafff/leiloes-radar",
                    workflow_run_id=44590432999,
                ),
            ],
        )
        _, base = self._start_server(fake)

        code, health = self._get(f"{base}/healthz")
        self.assertEqual(code, 200)
        self.assertEqual(health, {"ok": True})

        code, status = self._get(f"{base}/status")
        self.assertEqual(code, 200)
        self.assertEqual(status["node"], "k3d-5950x-ci-0")
        self.assertTrue(status["busy"])
        self.assertEqual(status["busy_count"], 2)
        self.assertEqual(status["load1"], 0.75)
        self.assertEqual(status["mem_available_mib"], 32000)

        code, filtered = self._get(
            f"{base}/status?exclude_run=44590432742&exclude_job=proven,other"
        )
        self.assertEqual(code, 200)
        self.assertFalse(filtered["busy"])
        self.assertEqual(filtered["busy_count"], 0)

        code, repeated = self._get(
            f"{base}/status?exclude_job=proven&exclude_job=pytest"
        )
        self.assertEqual(code, 200)
        self.assertFalse(repeated["busy"])

        code, missing = self._get(f"{base}/does-not-exist")
        self.assertEqual(code, 404)
        self.assertIn("error", missing)

    def test_k8s_error_returns_503(self) -> None:
        fake = FakeK8s(
            ars=[],
            ers=[],
            error=server.KubernetesAPIError("apiserver unreachable"),
        )
        _, base = self._start_server(fake)
        code, body = self._get(f"{base}/status")
        self.assertEqual(code, 503)
        self.assertEqual(body["node"], "k3d-5950x-ci-0")
        self.assertIn("apiserver unreachable", body["error"])

    def test_concurrency_limit_returns_503_and_recovers(self) -> None:
        entered = threading.Event()
        release = threading.Event()

        class BlockingK8s(FakeK8s):
            def list_ephemeral_runners(self) -> list[dict[str, Any]]:
                entered.set()
                release.wait(timeout=3.0)
                return super().list_ephemeral_runners()

        fake = BlockingK8s(ars=[], ers=[])
        _, base = self._start_server(fake, max_concurrency=1)

        first_result: list[tuple[int, dict[str, Any]]] = []

        def run_first() -> None:
            first_result.append(self._get(f"{base}/status"))

        worker = threading.Thread(target=run_first, daemon=True)
        worker.start()
        self.assertTrue(entered.wait(timeout=2.0))

        code, body = self._get(f"{base}/healthz")
        self.assertEqual(code, 503)
        self.assertIn("too many concurrent requests", body["error"])

        release.set()
        worker.join(timeout=3.0)
        self.assertEqual(first_result[0][0], 200)

        # Semaphore slot released -> subsequent requests succeed.
        code, health = self._get(f"{base}/healthz")
        self.assertEqual(code, 200)
        self.assertEqual(health, {"ok": True})


class KubernetesClientTests(unittest.TestCase):
    def test_reads_rotated_token_and_hides_secret_on_error(self) -> None:
        received_auth: list[str] = []
        status_code = [200]

        class MockAPI(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                received_auth.append(self.headers.get("Authorization", ""))
                body = json.dumps({"items": [{"metadata": {"name": "ok"}}]}).encode("utf-8")
                self.send_response(status_code[0])
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: Any) -> None:
                return

        httpd = server.ReusableThreadingHTTPServer(("127.0.0.1", 0), MockAPI)
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        with tempfile.TemporaryDirectory() as tmp:
            token_file = Path(tmp) / "token"
            token_file.write_text("secret-token-v1\n", encoding="utf-8")
            client = server.KubernetesClient(
                namespace="arc-runners",
                api_url=f"http://127.0.0.1:{httpd.server_address[1]}",
                token_path=token_file,
                ca_path=Path(tmp) / "no-ca",
            )
            items = client.list_ephemeral_runners()
            self.assertEqual(len(items), 1)
            self.assertEqual(received_auth[-1], "Bearer secret-token-v1")

            token_file.write_text("secret-token-v2\n", encoding="utf-8")
            client.list_autoscaling_runner_sets()
            self.assertEqual(received_auth[-1], "Bearer secret-token-v2")

            status_code[0] = 403
            with self.assertRaises(server.KubernetesAPIError) as ctx:
                client.list_ephemeral_runners()
            self.assertIn("403", str(ctx.exception))
            self.assertNotIn("secret-token-v2", str(ctx.exception))

    def test_truncated_response_raises_k8s_api_error(self) -> None:
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        port = listener.getsockname()[1]
        self.addCleanup(listener.close)

        def serve_truncated() -> None:
            conn, _ = listener.accept()
            with conn:
                conn.recv(4096)
                conn.sendall(
                    b"HTTP/1.1 200 OK\r\n"
                    b"Content-Type: application/json\r\n"
                    b"Content-Length: 100\r\n\r\n"
                    b'{"items":'
                )

        t = threading.Thread(target=serve_truncated, daemon=True)
        t.start()

        with tempfile.TemporaryDirectory() as tmp:
            token_file = Path(tmp) / "token"
            token_file.write_text("secret-token\n", encoding="utf-8")
            client = server.KubernetesClient(
                namespace="arc-runners",
                api_url=f"http://127.0.0.1:{port}",
                token_path=token_file,
                ca_path=Path(tmp) / "no-ca",
            )
            with self.assertRaises(server.KubernetesAPIError) as ctx:
                client.list_ephemeral_runners()
            self.assertNotIn("secret-token", str(ctx.exception))
        t.join(timeout=2.0)


if __name__ == "__main__":
    unittest.main()
